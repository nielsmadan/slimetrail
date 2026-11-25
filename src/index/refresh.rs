use super::{
    IndexSession, Refreshed, Writer,
    estimate::{self, Gate, Work},
    store::{self, SourceStatus},
};
use crate::{
    Agent, AgentEstimate, AppendState, CachedSource, Coverage, Diagnostic, Discovery,
    DiscoveryCache, Envelope, Error, IndexStats, JobEstimate, Observer, ParsedSession, Progress,
    RawSession, Rebuild, RefreshEvent, RefreshMode, RefreshOptions, Result, SourceState,
    sources::{PrefixCheck, Resume, Source},
};
use rusqlite::Connection;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Instant,
};

fn diagnostic(agent: Agent, source: Option<String>, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        agent: Some(agent),
        source,
        code: code.into(),
        message: message.into(),
    }
}

#[derive(PartialEq, Eq)]
struct Stat {
    identity: (u64, u64),
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn stat(path: &Path) -> Option<Stat> {
    fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|m| Stat {
            identity: (m.dev(), m.ino()),
            size: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        })
}

fn stable(before: &Stat, after: Option<&Stat>) -> bool {
    after.is_some_and(|after| {
        before.identity == after.identity && (after.size > before.size || after == before)
    })
}

enum Outcome {
    Stable {
        complete: bool,
        diagnostics: Vec<Diagnostic>,
    },
    Changed,
    Failed,
    Rejected,
}

fn ingest(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    raw: &RawSession,
    generation: i64,
) -> Result<(Outcome, bool)> {
    if raw.resume.is_some() {
        match ingest_once(index, writer, source, raw, generation)? {
            Outcome::Rejected => {}
            outcome => return Ok((outcome, true)),
        }
        let fresh = raw.without_resume();
        return Ok((
            ingest_once(index, writer, source, &fresh, generation)?,
            false,
        ));
    }
    Ok((ingest_once(index, writer, source, raw, generation)?, false))
}

fn ingest_once(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    raw: &RawSession,
    generation: i64,
) -> Result<Outcome> {
    let before = match source.transcript(raw) {
        Some(path) => match stat(path) {
            Some(before) => Some((path, before)),
            None => return Ok(Outcome::Changed),
        },
        None => None,
    };
    let coverage = writer.coverage;
    let transaction = writer.connection.savepoint()?;
    let resume = raw.resume.as_ref().map(|resume| resume.state.entries);
    prepare_rows(&transaction, &index.namespace, raw, resume, generation)?;
    let mut write_failed = false;
    let mut expected_index = resume.unwrap_or(0);
    let parsed = source.parse(raw, &mut |entry| {
        if entry.index != expected_index {
            return Err(Error::InvalidArgument(
                "source emitted a non-dense entry index".into(),
            ));
        }
        expected_index += 1;
        let result = store::entry(&transaction, &index.namespace, raw, &entry, coverage);
        write_failed |= result.is_err();
        result
    });
    if write_failed {
        return Err(parsed.err().unwrap_or_else(|| {
            Error::InvalidArgument("source swallowed an index write failure".into())
        }));
    }
    if resume.is_some() && parsed.is_err() {
        return Ok(Outcome::Rejected);
    }
    if let Some((path, before)) = &before
        && !stable(before, stat(path).as_ref())
    {
        return Ok(Outcome::Changed);
    }
    let Ok(parsed) = parsed else {
        return Ok(Outcome::Failed);
    };
    if parsed.session.key != raw.key || parsed.session.source != raw.source {
        return Ok(if source.drifted(raw) {
            Outcome::Changed
        } else {
            Outcome::Failed
        });
    }
    let outcome = record(
        &transaction,
        &index.namespace,
        raw,
        parsed,
        resume.is_some(),
        generation,
    )?;
    if matches!(outcome, Outcome::Stable { .. }) {
        transaction.commit()?;
    }
    Ok(outcome)
}

fn prepare_rows(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    resume: Option<u64>,
    generation: i64,
) -> Result<()> {
    store::source(
        connection,
        namespace,
        raw,
        SourceStatus::Pending,
        &[],
        generation,
        None,
    )?;
    resume.map_or_else(
        || store::delete_session(connection, namespace, raw),
        |entries| store::truncate(connection, namespace, raw, entries),
    )
}

fn record(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    mut parsed: ParsedSession,
    resumed: bool,
    generation: i64,
) -> Result<Outcome> {
    if !parsed.complete && parsed.diagnostics.is_empty() {
        parsed.diagnostics.push(diagnostic(
            raw.key.agent,
            Some(raw.source.clone()),
            "partial_source",
            "Source parsing was incomplete",
        ));
    }
    let complete = parsed.complete && parsed.diagnostics.is_empty();
    if !resumed {
        store::session(connection, namespace, raw, &parsed.session)?;
    } else if !store::update_session(connection, namespace, raw, &parsed.session)? {
        return Ok(Outcome::Rejected);
    }
    let status = match (complete, parsed.recheck) {
        (_, true) => SourceStatus::Recheck,
        (true, false) => SourceStatus::Fresh,
        (false, false) => SourceStatus::Partial,
    };
    store::source(
        connection,
        namespace,
        raw,
        status,
        &parsed.diagnostics,
        generation,
        parsed.state.as_ref(),
    )?;
    Ok(Outcome::Stable {
        complete,
        diagnostics: parsed.diagnostics,
    })
}

pub(super) fn full(
    index: &mut IndexSession,
    agents: &BTreeSet<Agent>,
    options: &RefreshOptions<'_>,
    readers: Vec<Box<dyn Source>>,
) -> Result<Refreshed> {
    let mut writer = index
        .writer(options.wait, options.observer)?
        .ok_or(Error::IndexBusy)?;
    let started = Instant::now();
    writer.persist_coverage()?;
    let target = index.context.coverage;
    if writer.coverage != target || writer.pending.is_some() {
        store::rebuild_documents(
            &mut writer.connection,
            target,
            &mut |entries, total_entries| {
                emit(
                    options.observer,
                    RefreshEvent::Rebuilding(Rebuild {
                        entries,
                        total_entries,
                    }),
                );
            },
        )?;
        writer.coverage = target;
        writer.pending = None;
    }
    let result = write(index, &mut writer, agents, readers, options, started)?;
    index.publish(writer, false)?;
    Ok(result)
}

fn emit(observer: Option<&dyn Observer>, event: RefreshEvent) {
    if let Some(observer) = observer {
        observer.observe(event);
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

#[derive(Default)]
struct Ingested {
    parsed: u64,
    hashed: u64,
    micros: u64,
}

impl Ingested {
    const fn add(&mut self, micros: u64, work: Work) {
        self.micros = self.micros.saturating_add(micros);
        self.parsed = self.parsed.saturating_add(work.parsed());
        self.hashed = self.hashed.saturating_add(work.hashed);
    }

    fn observation(&self) -> Option<(u64, u64)> {
        estimate::observation(self.parsed, self.hashed, self.micros)
    }
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static DISCOVERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(feature = "test-hooks")]
pub fn discoveries() -> usize {
    DISCOVERIES.with(std::cell::Cell::get)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Reuse(Vec<Diagnostic>),
    Parse(Work),
}

enum Inventory {
    Ready(Discovery, Vec<Step>),
    Released,
    Unavailable,
}

pub(super) struct Planned {
    source: Box<dyn Source>,
    inventory: Inventory,
    pub(super) estimate: AgentEstimate,
}

impl Planned {
    fn released(self) -> Self {
        match &self.inventory {
            Inventory::Ready(discovery, _)
                if discovery.sessions.iter().any(|raw| raw.snapshot.is_some()) =>
            {
                Self {
                    inventory: Inventory::Released,
                    ..self
                }
            }
            _ => self,
        }
    }
}

const fn spot_checked(cached: &CachedSource) -> bool {
    matches!(&cached.state, Some(SourceState::Append(state)) if state.spot_checked)
}

fn reusable(raw: &RawSession, previous: &DiscoveryCache) -> bool {
    raw.snapshot.is_none()
        && previous.by_source.get(&raw.source).is_some_and(|cached| {
            cached.key == raw.key
                && cached.dependencies == raw.dependencies
                && cached.fingerprint == raw.fingerprint
                && !(previous.verify && spot_checked(cached))
        })
}

fn resume_state(
    raw: &RawSession,
    previous: &DiscoveryCache,
    recheck: &BTreeMap<String, CachedSource>,
) -> Option<AppendState> {
    let cached = previous
        .by_source
        .get(&raw.source)
        .or_else(|| recheck.get(&raw.source))?;
    match &cached.state {
        Some(SourceState::Append(append))
            if cached.key == raw.key && cached.dependencies == raw.dependencies =>
        {
            Some(append.clone())
        }
        _ => None,
    }
}

const fn prefix_check(mode: RefreshMode) -> PrefixCheck {
    match mode {
        RefreshMode::Query => PrefixCheck::Ends,
        RefreshMode::Full => PrefixCheck::Whole,
    }
}

fn file_size(sizes: &mut BTreeMap<PathBuf, u64>, path: &Path) -> u64 {
    *sizes
        .entry(path.to_path_buf())
        .or_insert_with(|| fs::metadata(path).map_or(0, |m| m.len()))
}

pub(super) fn plan(
    index: &IndexSession,
    connection: &Connection,
    agents: &BTreeSet<Agent>,
    readers: Vec<Box<dyn Source>>,
    mode: RefreshMode,
) -> Result<Vec<Planned>> {
    readers
        .into_iter()
        .filter(|s| agents.is_empty() || agents.contains(&s.agent()))
        .map(|source| plan_agent(index, connection, source, mode))
        .collect()
}

fn steps(
    source: &dyn Source,
    discovery: &mut Discovery,
    cached: &store::Cached,
    recheck: &BTreeMap<String, CachedSource>,
    mode: RefreshMode,
) -> Vec<Step> {
    for raw in &mut discovery.sessions {
        raw.resume = resume_state(raw, &cached.discovery, recheck).map(|state| Resume {
            state,
            check: prefix_check(mode),
        });
    }
    let sizes = estimate::Sizes::new(&discovery.sessions);
    let mut lengths = BTreeMap::new();
    let mut size = |path: &Path| file_size(&mut lengths, path);
    discovery
        .sessions
        .iter()
        .map(|raw| {
            if reusable(raw, &cached.discovery) {
                Step::Reuse(
                    cached
                        .diagnostics
                        .get(&raw.source)
                        .cloned()
                        .unwrap_or_default(),
                )
            } else {
                Step::Parse(sizes.work(raw, source.transcript(raw), &mut size))
            }
        })
        .collect()
}

fn plan_agent(
    index: &IndexSession,
    connection: &Connection,
    source: Box<dyn Source>,
    mode: RefreshMode,
) -> Result<Planned> {
    let agent = source.agent();
    let mut cached = store::cache(connection, &index.namespace, agent)?;
    cached.discovery.verify = mode == RefreshMode::Full;
    let recheck = store::rechecked(connection, &index.namespace, agent)?;
    #[cfg(feature = "test-hooks")]
    DISCOVERIES.with(|n| n.set(n.get() + 1));
    let inventory = source.discover(&index.context, &cached.discovery).map_or(
        Inventory::Unavailable,
        |mut discovery| {
            let steps = steps(source.as_ref(), &mut discovery, &cached, &recheck, mode);
            Inventory::Ready(discovery, steps)
        },
    );
    let parsed: Vec<Work> = match &inventory {
        Inventory::Ready(_, steps) => steps
            .iter()
            .filter_map(|step| match step {
                Step::Parse(work) => Some(*work),
                Step::Reuse(_) => None,
            })
            .collect(),
        Inventory::Released | Inventory::Unavailable => Vec::new(),
    };
    let compact = store::throughput(connection, agent, Coverage::Compact)?;
    let full = store::throughput(connection, agent, Coverage::Full)?;
    let estimate = estimate::agent(
        agent,
        parsed.len() as u64,
        parsed.into_iter().sum(),
        |coverage| match coverage {
            Coverage::Compact => compact,
            Coverage::Full => full,
        },
    );
    Ok(Planned {
        source,
        inventory,
        estimate,
    })
}

struct Tracker<'a> {
    progress: Progress,
    observer: Option<&'a dyn Observer>,
}

impl<'a> Tracker<'a> {
    fn start(observer: Option<&'a dyn Observer>, progress: Progress) -> Self {
        let tracker = Self { progress, observer };
        tracker.emit(RefreshEvent::Started(progress));
        tracker
    }

    fn emit(&self, event: RefreshEvent) {
        emit(self.observer, event);
    }

    fn advance(&mut self, bytes: u64) {
        self.progress = self.progress.advanced(bytes);
        self.emit(RefreshEvent::Advanced(self.progress));
    }

    const fn replan(&mut self, before: &AgentEstimate, after: &AgentEstimate) {
        self.progress = self.progress.replanned(
            (before.sessions, before.bytes),
            (after.sessions, after.bytes),
        );
    }

    fn finish(&self) {
        self.emit(RefreshEvent::Finished(self.progress));
    }
}

pub(super) fn write(
    index: &mut IndexSession,
    writer: &mut Writer,
    agents: &BTreeSet<Agent>,
    readers: Vec<Box<dyn Source>>,
    options: &RefreshOptions<'_>,
    started: Instant,
) -> Result<Refreshed> {
    let coverage = writer.coverage;
    let seq = store::begin_run(&writer.connection)?;
    let planned = plan(index, &writer.connection, agents, readers, options.mode)?;
    let estimates: Vec<_> = planned.iter().map(|p| p.estimate.clone()).collect();
    let decision = estimate::gate(&estimates, coverage, options.build, &mut |agent| {
        store::has_published(&writer.connection, &index.namespace, agent)
    })?;
    let admitted = match decision {
        Gate::Build(admitted) => admitted,
        Gate::Abort(pending) => {
            return Err(Error::IndexRequired(Box::new(JobEstimate {
                agents: pending,
                coverage,
                coverage_locked: writer.locked,
            })));
        }
        Gate::Skip(pending) => {
            for &agent in agents {
                index.mark_stale(agent);
            }
            let diagnostics = pending
                .iter()
                .map(|e| estimate::required(e, coverage))
                .collect();
            return Ok(Refreshed {
                status: super::skipped(diagnostics, started),
                available: false,
            });
        }
    };
    let (planned, gated): (Vec<_>, Vec<_>) = planned
        .into_iter()
        .partition(|p| admitted.contains(&p.estimate.agent));
    let gated: Vec<_> = gated.into_iter().map(|p| p.estimate).collect();
    for estimate in &gated {
        index.mark_stale(estimate.agent);
    }
    let mut diagnostics: Vec<_> = gated
        .iter()
        .map(|e| estimate::required(e, coverage))
        .collect();
    let planned: Vec<_> = planned
        .into_iter()
        .enumerate()
        .map(|(position, p)| if position == 0 { p } else { p.released() })
        .collect();
    let mut tracker = Tracker::start(
        options.observer,
        Progress::new(
            planned.iter().map(|p| p.estimate.sessions).sum(),
            planned.iter().map(|p| p.estimate.bytes).sum(),
        ),
    );
    let mut stats = IndexStats::default();
    for planned in planned {
        let agent = planned.source.agent();
        index.mark_refreshing(agent);
        let planned = if matches!(planned.inventory, Inventory::Released) {
            let replanned = plan_agent(index, &writer.connection, planned.source, options.mode)?;
            tracker.replan(&planned.estimate, &replanned.estimate);
            replanned
        } else {
            planned
        };
        let (found, totals) = refresh_agent(index, writer, planned, seq, &mut tracker)?;
        stats = combined(&stats, &totals);
        diagnostics.extend(found);
        index.mark_refreshed(agent);
    }
    store::reclaim(&writer.connection)?;
    tracker.finish();
    stats.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    Ok(Refreshed::available(Envelope {
        version: 1,
        incomplete: !diagnostics.is_empty(),
        diagnostics,
        data: Some(stats),
    }))
}

fn refresh_agent(
    index: &IndexSession,
    writer: &mut Writer,
    planned: Planned,
    seq: i64,
    tracker: &mut Tracker<'_>,
) -> Result<(Vec<Diagnostic>, IndexStats)> {
    let source = planned.source.as_ref();
    let agent = source.agent();
    let Inventory::Ready(discovery, steps) = planned.inventory else {
        let failed = vec![diagnostic(
            agent,
            None,
            "source_unavailable",
            "Selected source could not be enumerated",
        )];
        store::discovery_failed(&writer.connection, &index.namespace, agent, &failed, seq)?;
        return Ok((failed, IndexStats::default()));
    };
    let generation = store::next_generation(&writer.connection, &index.namespace, agent)?;
    let run = ingest_agent(index, writer, source, discovery, steps, generation, tracker)?;
    publish_agent(
        &mut writer.connection,
        &index.namespace,
        agent,
        (generation, seq),
        writer.coverage,
        run,
    )
}

const fn combined(total: &IndexStats, run: &IndexStats) -> IndexStats {
    IndexStats {
        discovered: total.discovered + run.discovered,
        reused: total.reused + run.reused,
        reparsed: total.reparsed + run.reparsed,
        removed: total.removed + run.removed,
        failed: total.failed + run.failed,
        elapsed_ms: total.elapsed_ms,
    }
}

struct AgentRun {
    diagnostics: Vec<Diagnostic>,
    enumeration_complete: bool,
    seen: BTreeSet<String>,
    reused: BTreeSet<String>,
    ingested: Ingested,
    stats: IndexStats,
}

impl AgentRun {
    fn note(&mut self, diagnostics: Vec<Diagnostic>) {
        let unseen: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| !self.diagnostics.contains(d))
            .collect();
        self.diagnostics.extend(unseen);
    }

    fn reuse(&mut self, source: String, diagnostics: Vec<Diagnostic>) {
        self.reused.insert(source);
        self.stats.reused += 1;
        if !diagnostics.is_empty() {
            self.stats.failed += 1;
            self.note(diagnostics);
        }
    }

    fn record(
        &mut self,
        connection: &Connection,
        namespace: &str,
        raw: &RawSession,
        generation: i64,
        (work, result): (Work, Attempt),
    ) -> Result<()> {
        match result.outcome {
            Outcome::Stable {
                complete,
                diagnostics,
            } => {
                let work = if result.resumed {
                    work
                } else {
                    work.reparsed()
                };
                self.ingested.add(result.micros, work);
                self.stats.reparsed += 1;
                if !complete {
                    self.stats.failed += 1;
                }
                self.note(diagnostics);
            }
            Outcome::Changed | Outcome::Failed | Outcome::Rejected => {
                self.stats.failed += 1;
                let failed = diagnostic(
                    raw.key.agent,
                    Some(raw.source.clone()),
                    "source_unavailable",
                    "Source could not be parsed consistently; previous entries are excluded",
                );
                store::source(
                    connection,
                    namespace,
                    raw,
                    SourceStatus::Failed,
                    std::slice::from_ref(&failed),
                    generation,
                    None,
                )?;
                self.diagnostics.push(failed);
            }
        }
        Ok(())
    }
}

struct Attempt {
    outcome: Outcome,
    resumed: bool,
    micros: u64,
}

fn attempt(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    raw: &RawSession,
    generation: i64,
) -> Result<Attempt> {
    let timer = Instant::now();
    let (outcome, resumed) = ingest(index, writer, source, raw, generation)?;
    Ok(Attempt {
        outcome,
        resumed,
        micros: elapsed_micros(timer),
    })
}

fn rediscover(index: &IndexSession, source: &dyn Source, raw: &RawSession) -> Option<RawSession> {
    let mut discovery = source
        .discover(&index.context, &DiscoveryCache::default())
        .ok()?;
    let found = discovery
        .sessions
        .iter()
        .position(|s| s.source == raw.source)
        .or_else(|| discovery.sessions.iter().position(|s| s.key == raw.key))?;
    Some(discovery.sessions.swap_remove(found))
}

struct Settled {
    raw: RawSession,
    attempt: Attempt,
    changed_from: Option<String>,
}

fn settle(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    raw: RawSession,
    generation: i64,
) -> Result<Settled> {
    let first = attempt(index, writer, source, &raw, generation)?;
    if !matches!(first.outcome, Outcome::Changed) {
        return Ok(Settled {
            raw,
            attempt: first,
            changed_from: None,
        });
    }
    let changed_from = Some(raw.source.clone());
    Ok(match rediscover(index, source, &raw) {
        Some(live) => {
            let retry = attempt(index, writer, source, &live, generation)?;
            let micros = first.micros.saturating_add(retry.micros);
            Settled {
                raw: live,
                attempt: Attempt { micros, ..retry },
                changed_from,
            }
        }
        None => Settled {
            raw,
            attempt: first,
            changed_from,
        },
    })
}

fn ingest_agent(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    discovery: Discovery,
    steps: Vec<Step>,
    generation: i64,
    tracker: &mut Tracker<'_>,
) -> Result<AgentRun> {
    let agent = source.agent();
    let enumeration_complete =
        discovery.complete_agents.contains(&agent) && discovery.diagnostics.is_empty();
    let mut run = AgentRun {
        diagnostics: discovery.diagnostics,
        enumeration_complete,
        seen: BTreeSet::new(),
        reused: BTreeSet::new(),
        ingested: Ingested::default(),
        stats: IndexStats {
            discovered: discovery.sessions.len() as u64,
            ..IndexStats::default()
        },
    };
    if !enumeration_complete && run.diagnostics.is_empty() {
        run.diagnostics.push(diagnostic(
            agent,
            None,
            "incomplete_inventory",
            "Selected source inventory is incomplete",
        ));
    }
    run.diagnostics.extend(discovery.advisories);
    let mut identities = BTreeSet::new();
    for (raw, step) in discovery.sessions.into_iter().zip(steps) {
        run.seen.insert(raw.source.clone());
        if raw.key.agent != agent || !identities.insert(raw.key.id.clone()) {
            if let Step::Parse(work) = step {
                tracker.advance(work.parsed());
            }
            run.enumeration_complete = false;
            run.stats.failed += 1;
            run.diagnostics.push(diagnostic(
                agent,
                Some(raw.source),
                "duplicate_identity",
                "Source inventory contains an invalid or duplicate identity",
            ));
            continue;
        }
        let work = match step {
            Step::Reuse(diagnostics) => {
                run.reuse(raw.source, diagnostics);
                continue;
            }
            Step::Parse(work) => work,
        };
        let settled = settle(index, writer, source, raw, generation)?;
        if let Some(original) = settled.changed_from {
            run.enumeration_complete = false;
            run.diagnostics.push(diagnostic(
                agent,
                Some(original),
                "source_changed",
                "Source changed during refresh; inventory will be reconciled again",
            ));
            run.seen.insert(settled.raw.source.clone());
        }
        run.record(
            &writer.connection,
            &index.namespace,
            &settled.raw,
            generation,
            (work, settled.attempt),
        )?;
        tracker.advance(work.parsed());
    }
    Ok(run)
}

fn publish_agent(
    connection: &mut Connection,
    namespace: &str,
    agent: Agent,
    (generation, seq): (i64, i64),
    coverage: Coverage,
    run: AgentRun,
) -> Result<(Vec<Diagnostic>, IndexStats)> {
    let transaction = connection.transaction()?;
    store::keep_reused(&transaction, namespace, agent, generation, &run.reused)?;
    let removed = if run.enumeration_complete {
        store::remove_unlisted(&transaction, namespace, agent, generation)?
    } else {
        store::carry_forward(&transaction, namespace, agent, generation, &run.seen)?;
        0
    };
    let complete = run.enumeration_complete && run.diagnostics.is_empty();
    store::publish_run(
        &transaction,
        namespace,
        agent,
        complete,
        &run.diagnostics,
        seq,
    )?;
    if let Some((parsed, spent)) = run.ingested.observation() {
        store::observe(&transaction, agent, coverage, parsed, spent)?;
    }
    transaction.commit()?;
    Ok((
        run.diagnostics,
        IndexStats {
            removed,
            ..run.stats
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Context, Entry, SessionKey, sources,
        testing::{context, context_at},
    };
    use std::{
        cell::Cell,
        fs,
        path::{Path, PathBuf},
        rc::Rc,
    };

    struct ChangingSource {
        reader: Box<dyn Source>,
        path: PathBuf,
        changes: usize,
        calls: Rc<Cell<usize>>,
        fail: bool,
    }

    impl Source for ChangingSource {
        fn agent(&self) -> Agent {
            Agent::Claude
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            let parsed = self.reader.parse(raw, emit)?;
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() <= self.changes {
                write(&self.path, &format!("revision {}", self.calls.get()));
            }
            if self.fail {
                Err(Error::InvalidArgument("synthetic parse failure".into()))
            } else {
                Ok(parsed)
            }
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
        fn drifted(&self, raw: &RawSession) -> bool {
            self.reader.drifted(raw)
        }
    }

    struct HookedSource {
        reader: Box<dyn Source>,
        hook: Box<dyn Fn()>,
        calls: Rc<Cell<usize>>,
    }

    impl Source for HookedSource {
        fn agent(&self) -> Agent {
            self.reader.agent()
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            let parsed = self.reader.parse(raw, emit)?;
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == 1 {
                (self.hook)();
            }
            Ok(parsed)
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
        fn drifted(&self, raw: &RawSession) -> bool {
            self.reader.drifted(raw)
        }
    }

    struct EditedDiscovery {
        reader: Box<dyn Source>,
        edit: Box<dyn Fn(Result<Discovery>) -> Result<Discovery>>,
    }

    impl Source for EditedDiscovery {
        fn agent(&self) -> Agent {
            self.reader.agent()
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            (self.edit)(self.reader.discover(ctx, previous))
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            self.reader.parse(raw, emit)
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
        fn drifted(&self, raw: &RawSession) -> bool {
            self.reader.drifted(raw)
        }
    }

    fn rollout(text: &str) -> String {
        [
            serde_json::json!({"type":"session_meta","payload":{"id":"stable","source":"cli"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":text}}),
        ]
        .iter()
        .map(|r| r.to_string() + "\n")
        .collect()
    }

    fn run_sources(
        index: &mut IndexSession,
        agents: &BTreeSet<Agent>,
        readers: Vec<Box<dyn Source>>,
    ) -> Result<Envelope<IndexStats>> {
        Ok(index
            .refresh_from(agents, &RefreshOptions::QUERY, readers)?
            .status)
    }

    fn retry(ctx: &Context, hook: Box<dyn Fn()>) -> (IndexSession, Envelope<IndexStats>) {
        let agents = BTreeSet::from([Agent::Codex]);
        let mut index = IndexSession::open(ctx).unwrap();
        let calls = Rc::new(Cell::new(0));
        let reader = HookedSource {
            reader: sources::source_for(ctx, Agent::Codex).unwrap(),
            hook,
            calls: Rc::clone(&calls),
        };
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert_eq!(calls.get(), 2);
        (index, result)
    }

    fn texts(index: &IndexSession, key: &SessionKey) -> Vec<String> {
        let mut texts = Vec::new();
        index
            .visit_entries(key, &mut |e| {
                texts.push(e.text);
                Ok(())
            })
            .unwrap();
        texts
    }

    fn write(path: &Path, text: &str) {
        write_session(path, "stable", text);
    }

    fn line(id: &str, text: &str) -> String {
        serde_json::json!({"type":"user","sessionId":id,"message":text}).to_string() + "\n"
    }

    fn write_session(path: &Path, id: &str, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, line(id, text)).unwrap();
    }

    #[test]
    fn changing_source_retries_once_and_excludes_unstable_old_rows() {
        for changes in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            let path = ctx.roots.claude_projects.join("project/source.jsonl");
            write(&path, "original");
            let agents = BTreeSet::from([Agent::Claude]);
            let mut index = IndexSession::open(&ctx).unwrap();
            index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
            write(&path, "before change");
            let calls = Rc::new(Cell::new(0));
            let reader = ChangingSource {
                reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
                path: path.clone(),
                changes,
                calls: Rc::clone(&calls),
                fail: false,
            };
            let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
            assert!(result.incomplete);
            assert_eq!(calls.get(), 2);
            let key = SessionKey {
                agent: Agent::Claude,
                id: "stable".into(),
            };
            let mut texts = Vec::new();
            index
                .visit_entries(&key, &mut |e| {
                    texts.push(e.text);
                    Ok(())
                })
                .unwrap();
            if changes == 1 {
                assert_eq!(texts, ["revision 1"]);
            } else {
                assert_eq!(texts, [] as [std::string::String; 0]);
                assert_eq!(
                    index
                        .connection
                        .query_row("SELECT text FROM entries", [], |r| r.get::<_, String>(0))
                        .unwrap(),
                    "original"
                );
            }
            assert!(
                !index
                    .refresh(&agents, &RefreshOptions::QUERY)
                    .unwrap()
                    .incomplete
            );
            index.verify_integrity().unwrap();
        }
    }

    #[test]
    fn parser_failure_rolls_back_streamed_entries_and_excludes_prior_rows() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/source.jsonl");
        write(&path, "original");
        let agents = BTreeSet::from([Agent::Claude]);
        let mut index = IndexSession::open(&ctx).unwrap();
        index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        write(&path, "replacement");
        let reader = ChangingSource {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            path,
            changes: 0,
            calls: Rc::new(Cell::new(0)),
            fail: true,
        };
        let failed = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert!(failed.incomplete);
        assert_eq!(failed.data.unwrap().failed, 1);
        assert_eq!(
            index
                .connection
                .query_row("SELECT text FROM entries", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "original"
        );
        assert!(
            index
                .session(&SessionKey {
                    agent: Agent::Claude,
                    id: "stable".into()
                })
                .unwrap()
                .is_none()
        );
        index.verify_integrity().unwrap();
    }

    struct RejectedResume {
        reader: Box<dyn Source>,
        fallback: bool,
        resumed: Rc<Cell<bool>>,
    }

    impl Source for RejectedResume {
        fn agent(&self) -> Agent {
            self.reader.agent()
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            if raw.resume.is_some() {
                self.resumed.set(true);
                self.reader.parse(raw, emit)?;
                return Err(Error::InvalidArgument("synthetic resume rejection".into()));
            }
            if self.fallback {
                self.reader.parse(raw, emit)
            } else {
                Err(Error::InvalidArgument("synthetic parse failure".into()))
            }
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
        fn drifted(&self, raw: &RawSession) -> bool {
            self.reader.drifted(raw)
        }
    }

    type Rows = (Vec<(i64, i64, String)>, Vec<(i64, Option<i64>, String)>);

    fn rows(index: &IndexSession) -> Rows {
        let connection = &index.connection;
        let entries = connection
            .prepare("SELECT rowid,entry_index,text FROM entries ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let documents = connection
            .prepare(
                "SELECT document_id,entry_index,kind FROM search_documents ORDER BY document_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (entries, documents)
    }

    fn indexed(index: &IndexSession, key: &SessionKey) -> (Option<crate::Session>, Vec<Entry>) {
        let mut entries = Vec::new();
        index
            .visit_entries(key, &mut |e| {
                entries.push(e);
                Ok(())
            })
            .unwrap();
        (index.session(key).unwrap(), entries)
    }

    fn learned(index: &IndexSession) -> Option<(u64, u64)> {
        store::throughput(&index.connection, Agent::Claude, crate::Coverage::Compact).unwrap()
    }

    #[test]
    fn failed_ingests_leave_the_learned_rate_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/source.jsonl");
        let large = "large text ".repeat(100_000);
        write(&path, &large);
        let agents = BTreeSet::from([Agent::Claude]);
        let mut index = IndexSession::open(&ctx).unwrap();
        index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        let before = learned(&index);
        assert!(before.is_some());
        for revision in ["first", "second"] {
            write(&path, &format!("{revision} {large}"));
            let reader = ChangingSource {
                reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
                path: path.clone(),
                changes: 0,
                calls: Rc::new(Cell::new(0)),
                fail: true,
            };
            let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
            assert_eq!(result.data.unwrap().failed, 1);
            assert_eq!(learned(&index), before);
        }
    }

    #[test]
    fn rejected_resume_fallback_records_the_full_source_size() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/source.jsonl");
        write(&path, &"large text ".repeat(100_000));
        let agents = BTreeSet::from([Agent::Claude]);
        let mut index = IndexSession::open(&ctx).unwrap();
        index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        let (bytes, _) = learned(&index).unwrap();
        fs::write(
            &path,
            fs::read_to_string(&path).unwrap() + &line("stable", "appended"),
        )
        .unwrap();
        let resumed = Rc::new(Cell::new(false));
        let reader = RejectedResume {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            fallback: true,
            resumed: Rc::clone(&resumed),
        };
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert!(resumed.get());
        assert_eq!(result.data.unwrap().reparsed, 1);
        let (after, _) = learned(&index).unwrap();
        assert_eq!(after - bytes, fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn rejected_resume_rolls_back_its_writes() {
        for fallback in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            let path = ctx.roots.claude_projects.join("project/source.jsonl");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let title = serde_json::json!({"type":"custom-title","customTitle":"Native"});
            fs::write(&path, line("stable", "original") + &format!("{title}\n")).unwrap();
            let agents = BTreeSet::from([Agent::Claude]);
            let mut index = IndexSession::open(&ctx).unwrap();
            index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
            let before = rows(&index);
            assert!(before.1.iter().any(|(_, entry, _)| entry.is_none()));
            fs::write(
                &path,
                fs::read_to_string(&path).unwrap() + &line("stable", "appended"),
            )
            .unwrap();
            let resumed = Rc::new(Cell::new(false));
            let reader = RejectedResume {
                reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
                fallback,
                resumed: Rc::clone(&resumed),
            };
            let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
            assert!(resumed.get());
            let key = stable_key(Agent::Claude);
            if fallback {
                assert_eq!(codes(&result), [] as [&str; 0]);
                let cold = context_at(dir.path())
                    .index_dir(dir.path().join("cold"))
                    .build();
                let mut other = IndexSession::open(&cold).unwrap();
                other.refresh(&agents, &RefreshOptions::QUERY).unwrap();
                assert_eq!(indexed(&index, &key), indexed(&other, &key));
            } else {
                assert_eq!(codes(&result), ["source_unavailable"]);
                assert_eq!(rows(&index), before);
            }
            index.verify_integrity().unwrap();
        }
    }

    #[test]
    fn compressed_rollout_decompressed_mid_refresh_retries_at_the_plain_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let plain = ctx
            .roots
            .codex_sessions
            .join("2026/11/01/rollout-stable.jsonl");
        let zst = plain.with_extension("jsonl.zst");
        fs::create_dir_all(plain.parent().unwrap()).unwrap();
        fs::write(
            &zst,
            ruzstd::encoding::compress_to_vec(
                rollout("compressed text").as_bytes(),
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        let (swap_plain, swap_zst) = (plain.clone(), zst);
        let (mut index, result) = retry(
            &ctx,
            Box::new(move || {
                fs::write(&swap_plain, rollout("compressed text")).unwrap();
                fs::remove_file(&swap_zst).unwrap();
            }),
        );
        let codes: Vec<_> = result.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, ["source_changed"]);
        assert_eq!(result.data.unwrap().failed, 0);
        let key = SessionKey {
            agent: Agent::Codex,
            id: "stable".into(),
        };
        let session = index.session(&key).unwrap().unwrap();
        assert_eq!(session.source, plain.to_string_lossy());
        assert_eq!(texts(&index, &key), ["compressed text"]);
        let count: i64 = index
            .connection
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let agents = BTreeSet::from([Agent::Codex]);
        assert!(
            !index
                .refresh(&agents, &RefreshOptions::QUERY)
                .unwrap()
                .incomplete
        );
        index.verify_integrity().unwrap();
    }

    #[test]
    fn moved_source_retries_at_the_live_copy_of_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let first = ctx.roots.codex_sessions.join("rollout-first.jsonl");
        let moved = ctx.roots.codex_sessions.join("rollout-moved.jsonl");
        let archived = ctx
            .roots
            .codex_archived_sessions
            .join("rollout-archived.jsonl");
        fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
        fs::create_dir_all(&ctx.roots.codex_archived_sessions).unwrap();
        fs::write(&first, rollout("live text")).unwrap();
        let (from, to, copy) = (first, moved.clone(), archived);
        let (index, result) = retry(
            &ctx,
            Box::new(move || {
                fs::rename(&from, &to).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                fs::write(&copy, rollout("archived text")).unwrap();
            }),
        );
        assert_eq!(result.data.unwrap().failed, 0);
        let key = SessionKey {
            agent: Agent::Codex,
            id: "stable".into(),
        };
        let session = index.session(&key).unwrap().unwrap();
        assert_eq!(session.source, moved.to_string_lossy());
        assert!(!session.archived);
        assert_eq!(texts(&index, &key), ["live text"]);
    }

    fn hooked(
        index: &mut IndexSession,
        reader: Box<dyn Source>,
        hook: Box<dyn Fn()>,
    ) -> (Envelope<IndexStats>, usize) {
        let agents = BTreeSet::from([reader.agent()]);
        let calls = Rc::new(Cell::new(0));
        let reader = HookedSource {
            reader,
            hook,
            calls: Rc::clone(&calls),
        };
        let result = run_sources(index, &agents, vec![Box::new(reader)]).unwrap();
        (result, calls.get())
    }

    fn codes(result: &Envelope<IndexStats>) -> Vec<&str> {
        result.diagnostics.iter().map(|d| d.code.as_str()).collect()
    }

    fn keys(index: &IndexSession) -> Vec<String> {
        let mut keys = Vec::new();
        index
            .visit_sessions(&mut |s| {
                keys.push(format!("{}:{}", s.key.agent, s.key.id));
                Ok(())
            })
            .unwrap();
        keys
    }

    fn staged(index: &IndexSession, agent: Agent) -> Box<dyn Source> {
        sources::registry_in(&index.context, &index.staging.path)
            .into_iter()
            .find(|s| s.agent() == agent)
            .unwrap()
    }

    fn stable_key(agent: Agent) -> SessionKey {
        SessionKey {
            agent,
            id: "stable".into(),
        }
    }

    fn replace(path: &Path, contents: &str) {
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, contents).unwrap();
        fs::rename(replacement, path).unwrap();
    }

    #[test]
    fn codex_key_changed_after_discovery_retries_through_the_title_aware_drift_check() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.codex_sessions.join("rollout-stable.jsonl");
        fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
        fs::write(&path, rollout("original text")).unwrap();
        fs::write(
            &ctx.roots.codex_session_index,
            "{\"id\":\"stable\",\"thread_name\":\"Named\"}\n",
        )
        .unwrap();
        let mut index = IndexSession::open(&ctx).unwrap();
        let reader = sources::source_for(&ctx, Agent::Codex).unwrap();
        let discovered = reader.discover(&ctx, &DiscoveryCache::default()).unwrap();
        assert!(!reader.drifted(&discovered.sessions[0]));
        let edits = Cell::new(0);
        let reader = EditedDiscovery {
            reader,
            edit: Box::new(move |discovery| {
                edits.set(edits.get() + 1);
                if edits.get() == 1 {
                    fs::write(
                        &path,
                        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"renamed\"}}\n",
                    )
                    .unwrap();
                }
                discovery
            }),
        };
        let agents = BTreeSet::from([Agent::Codex]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert_eq!(codes(&result), ["source_changed"]);
        assert_eq!(result.data.unwrap().failed, 0);
        assert_eq!(keys(&index), ["codex:renamed"]);
    }

    #[test]
    fn transcript_appended_during_parse_is_stored_stable() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/source.jsonl");
        write(&path, "original");
        let mut index = IndexSession::open(&ctx).unwrap();
        let (result, calls) = hooked(
            &mut index,
            sources::source_for(&ctx, Agent::Claude).unwrap(),
            Box::new(move || {
                let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
                std::io::Write::write_all(&mut file, line("stable", "appended").as_bytes())
                    .unwrap();
            }),
        );
        assert_eq!(codes(&result), [] as [&str; 0]);
        assert_eq!(calls, 1);
        let key = stable_key(Agent::Claude);
        assert_eq!(texts(&index, &key), ["original"]);
        let status: String = index
            .connection
            .query_row("SELECT status FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "fresh");
        let next = index
            .refresh(&BTreeSet::from([Agent::Claude]), &RefreshOptions::QUERY)
            .unwrap();
        assert_eq!(next.data.unwrap().reparsed, 1);
        assert_eq!(texts(&index, &key), ["original", "appended"]);
    }

    #[test]
    fn transcript_replaced_during_parse_retries() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/source.jsonl");
        write(&path, "original");
        let mut index = IndexSession::open(&ctx).unwrap();
        let (result, calls) = hooked(
            &mut index,
            sources::source_for(&ctx, Agent::Claude).unwrap(),
            Box::new(move || replace(&path, &line("stable", "original"))),
        );
        assert_eq!(codes(&result), ["source_changed"]);
        assert_eq!(calls, 2);
        assert_eq!(result.data.unwrap().failed, 0);
        assert_eq!(texts(&index, &stable_key(Agent::Claude)), ["original"]);
    }

    #[test]
    fn cursor_transcript_replaced_during_parse_retries_despite_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let transcript = ctx
            .roots
            .cursor_projects
            .join("lossy-slug/agent-transcripts/stable/stable.jsonl");
        let contents = include_str!("../../tests/fixtures/jsonl/cursor.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, contents).unwrap();
        let meta = ctx.roots.cursor_chats.join("hash/stable/meta.json");
        fs::create_dir_all(meta.parent().unwrap()).unwrap();
        fs::write(&meta, r#"{"cwd":"/fixture","title":"Cursor title"}"#).unwrap();
        let store = rusqlite::Connection::open(meta.with_file_name("store.db")).unwrap();
        let value = crate::hex::hex(serde_json::json!({"name":"Store title"}).to_string());
        store
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);",
            )
            .unwrap();
        store
            .execute("INSERT INTO meta VALUES('0', ?)", [value])
            .unwrap();
        let mut index = IndexSession::open(&ctx).unwrap();
        let reader = staged(&index, Agent::Cursor);
        let empty = DiscoveryCache::default();
        let discovery = reader.discover(&index.context, &empty).unwrap();
        assert!(discovery.sessions[0].snapshot.is_some());
        drop(discovery);
        let (result, calls) = hooked(
            &mut index,
            reader,
            Box::new(move || replace(&transcript, contents)),
        );
        assert_eq!(codes(&result), ["source_changed"]);
        assert_eq!(calls, 2);
        assert_eq!(result.data.unwrap().failed, 0);
        assert_eq!(
            texts(&index, &stable_key(Agent::Cursor))
                .first()
                .map(String::as_str),
            Some("needle cursor café 🦀")
        );
    }

    #[test]
    fn devin_database_written_after_snapshot_ingests_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        fs::create_dir_all(ctx.roots.devin_sessions_db.parent().unwrap()).unwrap();
        let store = rusqlite::Connection::open(&ctx.roots.devin_sessions_db).unwrap();
        store
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                CREATE TABLE sessions(id TEXT PRIMARY KEY, working_directory TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER);
                CREATE TABLE message_nodes(row_id INTEGER PRIMARY KEY, session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, created_at INTEGER, metadata TEXT);
                CREATE TABLE subagent_heads(session_id TEXT, agent_id TEXT, chain_node_id INTEGER, updated_at INTEGER, PRIMARY KEY(session_id, agent_id)) WITHOUT ROWID;
                INSERT INTO sessions VALUES('stable', '/fixture', 'Devin title', 1786356000, 1786356300, 1, 0);",
            )
            .unwrap();
        let node = |id: i64, parent: Option<i64>, text: &str| {
            let chat = serde_json::json!({"role":"user","content":text,"metadata":{"created_at":"2026-08-10T10:00:00Z","is_user_input":true,"telemetry":{"source":"user"}}});
            (id, parent, chat.to_string())
        };
        let insert = "INSERT INTO message_nodes(session_id, node_id, parent_node_id, chat_message, created_at, metadata) VALUES('stable',?1,?2,?3,1786356000,'{}')";
        store.execute(insert, node(1, None, "first")).unwrap();
        let mut index = IndexSession::open(&ctx).unwrap();
        let reader = EditedDiscovery {
            reader: staged(&index, Agent::Devin),
            edit: Box::new(move |discovery| {
                store.execute(insert, node(2, Some(1), "second")).unwrap();
                store
                    .execute("UPDATE sessions SET main_chain_id=2", [])
                    .unwrap();
                discovery
            }),
        };
        let agents = BTreeSet::from([Agent::Devin]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        assert_eq!(result.data.unwrap().reparsed, 1);
        let key = stable_key(Agent::Devin);
        assert_eq!(texts(&index, &key), ["first"]);
        index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert_eq!(texts(&index, &key), ["first", "second"]);
    }

    struct StagingProbe {
        reader: Box<dyn Source>,
        staging: PathBuf,
        held: Rc<std::cell::RefCell<Vec<usize>>>,
    }

    impl Source for StagingProbe {
        fn agent(&self) -> Agent {
            self.reader.agent()
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            let snapshots = fs::read_dir(&self.staging)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("slimetrail-snapshot-")
                })
                .count();
            self.held.borrow_mut().push(snapshots);
            self.reader.parse(raw, emit)
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
    }

    #[test]
    fn later_agents_release_their_snapshots_until_their_own_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        write(
            &ctx.roots.claude_projects.join("project/source.jsonl"),
            "claude text",
        );
        fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
        rusqlite::Connection::open(&ctx.roots.opencode_db)
            .unwrap()
            .execute_batch("CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('stable','/fixture','OpenCode title',1,2);")
            .unwrap();
        let mut index = IndexSession::open(&ctx).unwrap();
        let held = Rc::new(std::cell::RefCell::new(Vec::new()));
        let probe = |agent| -> Box<dyn Source> {
            Box::new(StagingProbe {
                reader: staged(&index, agent),
                staging: index.staging.path.clone(),
                held: Rc::clone(&held),
            })
        };
        let readers = vec![probe(Agent::Claude), probe(Agent::Opencode)];
        let agents = BTreeSet::from([Agent::Claude, Agent::Opencode]);
        let result = run_sources(&mut index, &agents, readers).unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        assert_eq!(result.data.unwrap().reparsed, 2);
        assert_eq!(held.take(), [0, 1]);
        assert_eq!(keys(&index), ["claude:stable", "opencode:stable"]);
    }

    fn two_sessions(ctx: &Context) -> IndexSession {
        for id in ["one", "two"] {
            write_session(
                &ctx.roots
                    .claude_projects
                    .join(format!("project/{id}.jsonl")),
                id,
                &format!("{id} text"),
            );
        }
        let mut index = IndexSession::open(ctx).unwrap();
        let result = index
            .refresh(&BTreeSet::from([Agent::Claude]), &RefreshOptions::QUERY)
            .unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        index
    }

    fn run_row(index: &IndexSession) -> (i64, String) {
        index
            .connection
            .query_row(
                "SELECT generation,status FROM refresh_runs WHERE agent='claude'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn failed_discovery_keeps_the_previous_sessions_visible() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let mut index = two_sessions(&ctx);
        let visible = keys(&index);
        let (generation, _) = run_row(&index);
        let reader = EditedDiscovery {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            edit: Box::new(|_| Err(Error::InvalidArgument("synthetic discovery failure".into()))),
        };
        let agents = BTreeSet::from([Agent::Claude]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert!(result.incomplete);
        assert_eq!(codes(&result), ["source_unavailable"]);
        assert_eq!(keys(&index), visible);
        assert_eq!(visible, ["claude:one", "claude:two"]);
        assert_eq!(run_row(&index), (generation, "incomplete".into()));
    }

    #[test]
    fn query_opened_during_discovery_refreshes_instead_of_coalescing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let project = ctx.roots.claude_projects.join("project");
        write_session(&project.join("one.jsonl"), "one", "one text");
        let mut index = IndexSession::open(&ctx).unwrap();
        let opened = Rc::new(std::cell::RefCell::new(None));
        let (slot, hook_ctx) = (Rc::clone(&opened), ctx.clone());
        let reader = EditedDiscovery {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            edit: Box::new(move |discovery| {
                write_session(&project.join("late.jsonl"), "late", "late text");
                *slot.borrow_mut() = Some(IndexSession::open(&hook_ctx).unwrap());
                discovery
            }),
        };
        let agents = BTreeSet::from([Agent::Claude]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        assert_eq!(keys(&index), ["claude:one"]);
        let mut late = opened.borrow_mut().take().unwrap();
        let status = late.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert!(!status.incomplete, "{:?}", status.diagnostics);
        assert!(status.data.unwrap().discovered > 0);
        assert_eq!(keys(&late), ["claude:late", "claude:one"]);
    }

    #[test]
    fn unverified_refresh_during_a_pending_rebuild_writes_nothing_and_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let mut index = two_sessions(&ctx);
        let (generation, _) = run_row(&index);
        let db = rusqlite::Connection::open(ctx.index_dir.join(crate::index::DATABASE)).unwrap();
        db.execute("UPDATE settings SET coverage_pending='full'", [])
            .unwrap();
        drop(db);
        let agents = BTreeSet::from([Agent::Claude]);
        let result = index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert!(result.incomplete);
        assert_eq!(codes(&result), ["coverage_rebuild_pending"]);
        assert_eq!(run_row(&index).0, generation);
    }

    #[test]
    fn partial_discovery_keeps_an_omitted_session_visible() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let mut index = two_sessions(&ctx);
        let (generation, _) = run_row(&index);
        let reader = EditedDiscovery {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            edit: Box::new(|discovery| {
                discovery.map(|mut d| {
                    d.sessions.retain(|s| s.key.id != "two");
                    d.complete_agents.clear();
                    d
                })
            }),
        };
        let agents = BTreeSet::from([Agent::Claude]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert!(result.incomplete);
        assert_eq!(codes(&result), ["incomplete_inventory"]);
        assert_eq!(run_row(&index), (generation + 1, "incomplete".into()));
        assert_eq!(keys(&index), ["claude:one", "claude:two"]);
        let two = SessionKey {
            agent: Agent::Claude,
            id: "two".into(),
        };
        assert_eq!(texts(&index, &two), ["two text"]);
    }

    #[test]
    fn failed_first_discovery_records_generation_zero_then_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        write_session(
            &ctx.roots.claude_projects.join("project/one.jsonl"),
            "one",
            "one text",
        );
        let mut index = IndexSession::open(&ctx).unwrap();
        let reader = EditedDiscovery {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            edit: Box::new(|_| Err(Error::InvalidArgument("synthetic discovery failure".into()))),
        };
        let agents = BTreeSet::from([Agent::Claude]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert_eq!(codes(&result), ["source_unavailable"]);
        assert_eq!(run_row(&index), (0, "incomplete".into()));
        assert_eq!(keys(&index), [] as [&str; 0]);
        let result = index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        assert_eq!(run_row(&index), (1, "complete".into()));
        assert_eq!(keys(&index), ["claude:one"]);
        index.verify_integrity().unwrap();
    }

    #[test]
    fn incomplete_run_carries_forward_only_unlisted_sources() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        write_session(
            &ctx.roots.claude_projects.join("project/three.jsonl"),
            "three",
            "three text",
        );
        let mut index = two_sessions(&ctx);
        assert_eq!(keys(&index), ["claude:one", "claude:three", "claude:two"]);
        let reader = EditedDiscovery {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            edit: Box::new(|discovery| {
                discovery.map(|mut d| {
                    d.sessions.retain(|s| s.key.id != "three");
                    d.sessions.sort_by(|a, b| a.source.cmp(&b.source));
                    for s in &mut d.sessions {
                        if s.key.id == "two" {
                            s.key.id = "one".into();
                        }
                    }
                    d
                })
            }),
        };
        let agents = BTreeSet::from([Agent::Claude]);
        let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert_eq!(codes(&result), ["duplicate_identity"]);
        assert_eq!(keys(&index), ["claude:one", "claude:three"]);
    }

    struct StallingSource {
        reader: Box<dyn Source>,
        calls: Cell<usize>,
    }

    impl Source for StallingSource {
        fn agent(&self) -> Agent {
            self.reader.agent()
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            let parsed = self.reader.parse(raw, emit)?;
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == 2 {
                println!("STALLED {}", raw.key.id);
                std::io::Write::flush(&mut std::io::stdout()).unwrap();
                std::io::stdin().read_line(&mut String::new()).unwrap();
            }
            Ok(parsed)
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
        fn drifted(&self, raw: &RawSession) -> bool {
            self.reader.drifted(raw)
        }
    }

    struct Killed(std::process::Child);

    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "Subprocess worker invoked explicitly by killed_run_keeps_replaced_sources_visible"]
    fn killed_run_worker() {
        let Ok(base) = std::env::var("SLIMETRAIL_KILLED_RUN_BASE") else {
            return;
        };
        let ctx = context(Path::new(&base));
        let mut index = IndexSession::open(&ctx).unwrap();
        let reader = StallingSource {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            calls: Cell::new(0),
        };
        run_sources(
            &mut index,
            &BTreeSet::from([Agent::Claude]),
            vec![Box::new(reader)],
        )
        .unwrap();
    }

    #[test]
    fn killed_run_keeps_replaced_sources_visible() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let agents = BTreeSet::from([Agent::Claude]);
        drop(two_sessions(&ctx));
        for id in ["one", "two"] {
            let path = ctx
                .roots
                .claude_projects
                .join(format!("project/{id}.jsonl"));
            write_session(&path, id, &format!("{id} replaced"));
        }
        let mut child = Killed(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "index::refresh::tests::killed_run_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("SLIMETRAIL_KILLED_RUN_BASE", dir.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut output = std::io::BufReader::new(child.0.stdout.take().unwrap());
        let stalled = loop {
            let mut line = String::new();
            assert_ne!(
                std::io::BufRead::read_line(&mut output, &mut line).unwrap(),
                0,
                "worker exited before stalling"
            );
            if let Some(id) = line.trim().strip_prefix("STALLED ") {
                break id.to_owned();
            }
        };
        let replaced = if stalled == "one" { "two" } else { "one" };
        let session = |id: &str| SessionKey {
            agent: Agent::Claude,
            id: id.into(),
        };
        let mut stale = IndexSession::open(&ctx).unwrap();
        let result = stale
            .refresh(
                &agents,
                &RefreshOptions {
                    wait: std::time::Duration::from_millis(50),
                    ..RefreshOptions::QUERY
                },
            )
            .unwrap();
        assert_eq!(codes(&result), ["refresh_in_progress"]);
        assert_eq!(keys(&stale), ["claude:one", "claude:two"]);
        assert_eq!(
            texts(&stale, &session(replaced)),
            [format!("{replaced} replaced")]
        );
        assert_eq!(
            texts(&stale, &session(&stalled)),
            [format!("{stalled} text")]
        );
        drop(stale);
        drop(child);
        let mut next = IndexSession::open(&ctx).unwrap();
        let result = next.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        let stats = result.data.unwrap();
        assert_eq!((stats.reused, stats.reparsed), (1, 1));
        for id in ["one", "two"] {
            assert_eq!(texts(&next, &session(id)), [format!("{id} replaced")]);
        }
        next.verify_integrity().unwrap();
    }

    fn planned_raw(id: &str, dependencies: &[&str], signature: &str) -> RawSession {
        RawSession {
            key: stable_key(Agent::Claude),
            source: id.into(),
            dependencies: dependencies.iter().map(PathBuf::from).collect(),
            fingerprint: crate::Fingerprint {
                signature: signature.into(),
                title_mark: None,
            },
            snapshot: None,
            resume: None,
        }
    }

    fn append(consumed: u64) -> AppendState {
        AppendState {
            consumed,
            chunks: Vec::new(),
            spot_checked: false,
            lines: 0,
            entries: 0,
            malformed: 0,
            dev: 0,
            ino: 0,
            others: String::new(),
            session: crate::Session {
                key: stable_key(Agent::Claude),
                title: None,
                title_origin: None,
                source_updated_at: None,
                cwd: None,
                branch: None,
                started_at: None,
                updated_at: None,
                parent: None,
                source: "t".into(),
                archived: false,
            },
        }
    }

    fn cached(raw: &RawSession, state: Option<SourceState>) -> CachedSource {
        CachedSource {
            key: raw.key.clone(),
            source: raw.source.clone(),
            dependencies: raw.dependencies.clone(),
            fingerprint: raw.fingerprint.clone(),
            state,
        }
    }

    fn by_source(sources: Vec<CachedSource>) -> BTreeMap<String, CachedSource> {
        sources
            .into_iter()
            .map(|cached| (cached.source.clone(), cached))
            .collect()
    }

    #[test]
    fn only_unchanged_unsnapshotted_sources_are_reused() {
        let raw = planned_raw("t", &["t"], "same");
        let previous = DiscoveryCache {
            by_source: by_source(vec![cached(&raw, None)]),
            verify: false,
        };
        assert!(reusable(&raw, &previous));
        assert!(!reusable(&planned_raw("t", &["t"], "changed"), &previous));
        assert!(!reusable(
            &planned_raw("t", &["t", "settings"], "same"),
            &previous
        ));
        let snapshot = crate::PrivateSnapshot {
            directory: tempfile::tempdir().unwrap(),
            database: PathBuf::from("db"),
        };
        let snapshotted = RawSession {
            snapshot: Some(std::sync::Arc::new(snapshot)),
            ..planned_raw("t", &["t"], "same")
        };
        assert!(!reusable(&snapshotted, &previous));
    }

    #[test]
    fn resume_uses_a_fresh_then_partial_append_state_with_the_same_key_and_dependencies() {
        let raw = planned_raw("t", &["t"], "grown");
        let fresh = DiscoveryCache {
            by_source: by_source(vec![cached(&raw, Some(SourceState::Append(append(10))))]),
            verify: false,
        };
        let partial = by_source(vec![cached(&raw, Some(SourceState::Append(append(20))))]);
        assert_eq!(resume_state(&raw, &fresh, &partial), Some(append(10)));
        assert_eq!(
            resume_state(&raw, &DiscoveryCache::default(), &partial),
            Some(append(20))
        );
        let moved = planned_raw("t", &["t", "settings"], "grown");
        assert_eq!(resume_state(&moved, &fresh, &BTreeMap::new()), None);
        let content = DiscoveryCache {
            by_source: by_source(vec![cached(
                &raw,
                Some(SourceState::Content {
                    sha256: String::new(),
                }),
            )]),
            verify: false,
        };
        assert_eq!(resume_state(&raw, &content, &partial), None);
    }

    struct ResumeRecorder {
        reader: Box<dyn Source>,
        resumed: Rc<std::cell::RefCell<Vec<bool>>>,
    }

    impl Source for ResumeRecorder {
        fn agent(&self) -> Agent {
            self.reader.agent()
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            self.resumed.borrow_mut().push(raw.resume.is_some());
            self.reader.parse(raw, emit)
        }
        fn transcript<'a>(&self, raw: &'a RawSession) -> Option<&'a Path> {
            self.reader.transcript(raw)
        }
        fn drifted(&self, raw: &RawSession) -> bool {
            self.reader.drifted(raw)
        }
    }

    fn append_line(path: &Path, text: &str) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut file, line("stable", text).as_bytes()).unwrap();
    }

    #[test]
    fn undecodable_append_state_falls_back_to_a_full_reparse() {
        for stored in ["state", "json_extract(state,'$.state')", "'not json'"] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            let path = ctx.roots.claude_projects.join("project/source.jsonl");
            write(&path, "original");
            let agents = BTreeSet::from([Agent::Claude]);
            let mut index = IndexSession::open(&ctx).unwrap();
            index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
            rusqlite::Connection::open(ctx.index_dir.join(crate::index::DATABASE))
                .unwrap()
                .execute(&format!("UPDATE sources SET state={stored}"), [])
                .unwrap();
            append_line(&path, "appended");
            let resumed = Rc::new(std::cell::RefCell::new(Vec::new()));
            let recorder = |resumed: &Rc<std::cell::RefCell<Vec<bool>>>| {
                Box::new(ResumeRecorder {
                    reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
                    resumed: Rc::clone(resumed),
                })
            };
            let result = run_sources(&mut index, &agents, vec![recorder(&resumed)]).unwrap();
            assert_eq!(codes(&result), [] as [&str; 0]);
            assert_eq!(result.data.unwrap().reparsed, 1);
            assert_eq!(resumed.take(), [stored == "state"], "{stored}");
            let key = stable_key(Agent::Claude);
            assert_eq!(texts(&index, &key), ["original", "appended"]);
            append_line(&path, "third");
            run_sources(&mut index, &agents, vec![recorder(&resumed)]).unwrap();
            assert_eq!(resumed.take(), [true]);
            assert_eq!(texts(&index, &key), ["original", "appended", "third"]);
        }
    }

    mod codex_titles {
        use crate::{
            Agent, Context, DiscoveryCache, SessionKey, TitleOrigin,
            sources::jsonl::{FAILING_TITLE_READS, FileReader, hashed},
            testing::context,
        };
        use std::{
            collections::BTreeSet,
            fs::{self, File},
            path::PathBuf,
            time::{Duration, SystemTime},
        };

        #[test]
        fn title_read_failures_keep_each_sessions_cached_title() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
            for id in ["one", "two"] {
                fs::write(
                    ctx.roots.codex_sessions.join(format!("{id}.jsonl")),
                    format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n"),
                )
                .unwrap();
            }
            let names = &ctx.roots.codex_session_index;
            fs::write(names, "{\"id\":\"one\",\"thread_name\":\"Named\"}\n").unwrap();
            let agents = BTreeSet::from([Agent::Codex]);
            let refresh = || crate::refresh(&ctx, &agents, &crate::RefreshOptions::QUERY).unwrap();
            let counts = |result: &crate::Envelope<crate::IndexStats>| {
                let stats = result.data.as_ref().unwrap();
                (stats.reused, stats.reparsed)
            };
            let codes = |result: &crate::Envelope<crate::IndexStats>| -> Vec<String> {
                result.diagnostics.iter().map(|d| d.code.clone()).collect()
            };
            assert_eq!(counts(&refresh()), (0, 2));
            let empty = DiscoveryCache::default();
            let reader = FileReader::new(Agent::Codex, &ctx);
            let readable = DiscoveryCache {
                by_source: reader
                    .discover(&ctx, &empty)
                    .unwrap()
                    .sessions
                    .into_iter()
                    .map(|raw| {
                        let cached = crate::CachedSource {
                            key: raw.key,
                            source: raw.source.clone(),
                            dependencies: raw.dependencies,
                            fingerprint: raw.fingerprint,
                            state: None,
                        };
                        (raw.source, cached)
                    })
                    .collect(),
                verify: false,
            };
            FAILING_TITLE_READS.set(1);
            let retried = refresh();
            assert_eq!(FAILING_TITLE_READS.get(), 0);
            assert!(!retried.incomplete, "{:?}", retried.diagnostics);
            assert_eq!(counts(&retried), (2, 0));
            fs::remove_file(names).unwrap();
            fs::create_dir(names).unwrap();
            let mut index = crate::index::IndexSession::open(&ctx).unwrap();
            let failing = index
                .refresh(&agents, &crate::RefreshOptions::QUERY)
                .unwrap();
            assert!(failing.incomplete);
            assert_eq!(codes(&failing), ["source_unavailable"]);
            assert_eq!(failing.diagnostics[0].source.as_deref(), names.to_str());
            assert_eq!(counts(&failing), (2, 0));
            let one = SessionKey {
                agent: Agent::Codex,
                id: "one".into(),
            };
            assert_eq!(
                index.session(&one).unwrap().unwrap().title.as_deref(),
                Some("Named")
            );
            let three = ctx.roots.codex_sessions.join("three.jsonl");
            fs::write(
                &three,
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"three\"}}\n",
            )
            .unwrap();
            let outage = FileReader::new(Agent::Codex, &ctx)
                .discover(&ctx, &readable)
                .unwrap();
            fs::remove_file(&three).unwrap();
            assert_eq!(outage.sessions.len(), 3);
            for raw in &outage.sessions {
                let mark = raw.fingerprint.title_mark.as_deref().unwrap();
                match readable.by_source.get(&raw.source) {
                    Some(cached) => assert_eq!(cached.fingerprint, raw.fingerprint),
                    None => assert_eq!(
                        (raw.key.id.as_str(), mark),
                        ("three", hashed("unavailable").as_str())
                    ),
                }
            }
            drop(index);
            fs::remove_dir(names).unwrap();
            fs::write(names, "{\"id\":\"one\",\"thread_name\":\"Named\"}\n").unwrap();
            let recovered = refresh();
            assert!(!recovered.incomplete, "{:?}", recovered.diagnostics);
            assert_eq!(counts(&recovered), (2, 0));
        }

        fn write_codex(ctx: &Context, id: &str, lines: &[&str]) -> PathBuf {
            let path = ctx.roots.codex_sessions.join(format!("{id}.jsonl"));
            let meta = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}");
            let text: String = std::iter::once(meta.as_str())
                .chain(lines.iter().copied())
                .flat_map(|line| [line, "\n"])
                .collect();
            fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
            fs::write(&path, text).unwrap();
            path
        }

        fn codes(result: &crate::Envelope<crate::IndexStats>) -> Vec<&str> {
            result.diagnostics.iter().map(|d| d.code.as_str()).collect()
        }

        fn key(id: &str) -> SessionKey {
            SessionKey {
                agent: Agent::Codex,
                id: id.into(),
            }
        }

        #[test]
        fn malformed_title_lines_do_not_keep_deleted_sessions() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            write_codex(&ctx, "one", &[]);
            let two = write_codex(&ctx, "two", &[]);
            let names = &ctx.roots.codex_session_index;
            fs::write(
                names,
                "{\"id\":\"one\",\"thread_name\":\"Named\"}\nmalformed\n",
            )
            .unwrap();
            File::options()
                .write(true)
                .open(names)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
                .unwrap();
            let agents = BTreeSet::from([Agent::Codex]);
            let first = crate::refresh(&ctx, &agents, &crate::RefreshOptions::QUERY).unwrap();
            assert_eq!(codes(&first), ["malformed_metadata"]);
            fs::remove_file(two).unwrap();
            let mut index = crate::index::IndexSession::open(&ctx).unwrap();
            let removed = index
                .refresh(&agents, &crate::RefreshOptions::QUERY)
                .unwrap();
            assert!(removed.incomplete);
            assert_eq!(codes(&removed), ["malformed_metadata"]);
            assert_eq!(removed.data.as_ref().unwrap().removed, 1);
            assert_eq!(index.session(&key("two")).unwrap(), None);
            assert_eq!(
                index
                    .session(&key("one"))
                    .unwrap()
                    .unwrap()
                    .title
                    .as_deref(),
                Some("Named")
            );
        }

        #[test]
        fn rollouts_appended_during_a_title_outage_are_indexed_as_partial() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            let reply = |text: &str| {
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":text}}).to_string()
            };
            let first = reply("first message");
            write_codex(&ctx, "one", &[&first]);
            fs::write(
                &ctx.roots.codex_session_index,
                "{\"id\":\"one\",\"thread_name\":\"Named\"}\n",
            )
            .unwrap();
            let agents = BTreeSet::from([Agent::Codex]);
            crate::refresh(&ctx, &agents, &crate::RefreshOptions::QUERY).unwrap();
            FAILING_TITLE_READS.set(usize::MAX);
            let appended = reply("outage needle");
            write_codex(&ctx, "one", &[&first, &appended]);
            let status = || -> String {
                rusqlite::Connection::open(ctx.index_dir.join(crate::index::DATABASE))
                    .unwrap()
                    .query_row("SELECT status FROM sources WHERE id='one'", [], |r| {
                        r.get(0)
                    })
                    .unwrap()
            };
            let mut index = crate::index::IndexSession::open(&ctx).unwrap();
            let outage = index
                .refresh(&agents, &crate::RefreshOptions::QUERY)
                .unwrap();
            assert!(outage.incomplete);
            assert_eq!(codes(&outage), ["source_unavailable"]);
            let counts = outage.data.as_ref().unwrap();
            assert_eq!((counts.reparsed, counts.failed), (1, 1));
            let session = index.session(&key("one")).unwrap().unwrap();
            assert_ne!(session.title_origin, Some(TitleOrigin::Native));
            let mut found = 0;
            index
                .visit_search_documents(Some(("outage needle", false)), &mut |doc| {
                    found += usize::from(doc.text.contains("outage needle"));
                    Ok(())
                })
                .unwrap();
            assert_eq!(found, 1);
            drop(index);
            assert_eq!(status(), "partial");
            FAILING_TITLE_READS.set(0);
            let mut index = crate::index::IndexSession::open(&ctx).unwrap();
            let recovered = index
                .refresh(&agents, &crate::RefreshOptions::QUERY)
                .unwrap();
            assert!(!recovered.incomplete, "{:?}", recovered.diagnostics);
            assert_eq!(
                index
                    .session(&key("one"))
                    .unwrap()
                    .unwrap()
                    .title
                    .as_deref(),
                Some("Named")
            );
            drop(index);
            assert_eq!(status(), "fresh");
        }

        #[test]
        fn a_title_mark_packed_into_a_stored_signature_reparses_once() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            write_codex(&ctx, "one", &[]);
            fs::write(
                &ctx.roots.codex_session_index,
                "{\"id\":\"one\",\"thread_name\":\"Named\"}\n",
            )
            .unwrap();
            let agents = BTreeSet::from([Agent::Codex]);
            let refresh = || crate::refresh(&ctx, &agents, &crate::RefreshOptions::QUERY).unwrap();
            let counts = |result: &crate::Envelope<crate::IndexStats>| {
                let stats = result.data.as_ref().unwrap();
                (stats.reused, stats.reparsed)
            };
            assert_eq!(counts(&refresh()), (0, 1));
            rusqlite::Connection::open(ctx.index_dir.join(crate::index::DATABASE))
                .unwrap()
                .execute(
                    "UPDATE sources SET fingerprint=json_object('signature',json_extract(fingerprint,'$.signature')||char(10)||'title:'||json_extract(fingerprint,'$.title_mark')) WHERE id='one'",
                    [],
                )
                .unwrap();
            let migrated = refresh();
            assert!(!migrated.incomplete, "{:?}", migrated.diagnostics);
            assert_eq!(counts(&migrated), (0, 1));
            let mut index = crate::index::IndexSession::open(&ctx).unwrap();
            let reused = index
                .refresh(&agents, &crate::RefreshOptions::QUERY)
                .unwrap();
            assert_eq!(counts(&reused), (1, 0));
            assert_eq!(
                index
                    .session(&key("one"))
                    .unwrap()
                    .unwrap()
                    .title
                    .as_deref(),
                Some("Named")
            );
        }
    }
}
