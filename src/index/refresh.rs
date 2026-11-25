use super::{IndexSession, REFRESH_WAIT, Writer, store};
use crate::{
    Agent, Diagnostic, DiscoveryCache, Envelope, Error, IndexStats, RawSession, Result,
    SourceState,
    sources::{self, Source},
};
use rusqlite::params;
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

fn live_transcript(raw: &RawSession) -> Option<&Path> {
    match raw.key.agent {
        Agent::Claude | Agent::Codex | Agent::Cursor | Agent::Droid | Agent::Pi => {
            raw.dependencies.first().map(PathBuf::as_path)
        }
        Agent::Devin | Agent::Opencode => None,
    }
}

enum Outcome {
    Stable {
        complete: bool,
        diagnostics: Vec<Diagnostic>,
    },
    Changed,
    Failed,
}

fn ingest(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    raw: &mut RawSession,
    generation: i64,
) -> Result<Outcome> {
    if raw.resume.is_some() {
        match ingest_once(index, writer, source, raw, generation) {
            Err(Error::ResumeRejected) => raw.resume = None,
            outcome => return outcome,
        }
    }
    ingest_once(index, writer, source, raw, generation)
}

fn ingest_once(
    index: &IndexSession,
    writer: &mut Writer,
    source: &dyn Source,
    raw: &RawSession,
    generation: i64,
) -> Result<Outcome> {
    let before = match live_transcript(raw) {
        Some(path) => match stat(path) {
            Some(before) => Some((path, before)),
            None => return Ok(Outcome::Changed),
        },
        None => None,
    };
    let coverage = writer.coverage;
    let transaction = writer.connection.savepoint()?;
    store::source(
        &transaction,
        &index.namespace,
        raw,
        "pending",
        &[],
        generation,
        None,
    )?;
    let resume = raw.resume.as_ref().map(|state| state.entries);
    if let Some(entries) = resume {
        store::truncate(&transaction, &index.namespace, raw, entries)?;
    } else {
        transaction.execute(
            "DELETE FROM sessions WHERE namespace=?1 AND agent=?2 AND (id=?3 OR source=?4)",
            params![
                index.namespace,
                raw.key.agent.to_string(),
                raw.key.id,
                raw.source
            ],
        )?;
    }
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
    if resume.is_some() && matches!(parsed, Err(Error::ResumeRejected)) {
        return Err(Error::ResumeRejected);
    }
    if let Some((path, before)) = &before
        && !stable(before, stat(path).as_ref())
    {
        return Ok(Outcome::Changed);
    }
    let Ok(mut parsed) = parsed else {
        return Ok(Outcome::Failed);
    };
    if parsed.session.key != raw.key || parsed.session.source != raw.source {
        let drifted = before.is_some()
            && sources::jsonl::fingerprint(&raw.dependencies)
                .map_or(true, |current| current != raw.fingerprint);
        return Ok(if drifted {
            Outcome::Changed
        } else {
            Outcome::Failed
        });
    }
    if !parsed.complete && parsed.diagnostics.is_empty() {
        parsed.diagnostics.push(diagnostic(
            raw.key.agent,
            Some(raw.source.clone()),
            "partial_source",
            "Source parsing was incomplete",
        ));
    }
    let complete = parsed.complete && parsed.diagnostics.is_empty();
    if resume.is_none() {
        store::session(&transaction, &index.namespace, raw, &parsed.session)?;
    } else if !store::update_session(&transaction, &index.namespace, raw, &parsed.session)? {
        return Err(Error::ResumeRejected);
    }
    // Partial sources are never reused, so a recent torn final line is rechecked next refresh.
    let fresh = complete && !parsed.recheck;
    store::source(
        &transaction,
        &index.namespace,
        raw,
        if fresh { "fresh" } else { "partial" },
        &parsed.diagnostics,
        generation,
        parsed.state.as_ref(),
    )?;
    transaction.commit()?;
    Ok(Outcome::Stable {
        complete,
        diagnostics: parsed.diagnostics,
    })
}

pub(super) fn run(
    index: &mut IndexSession,
    agents: &BTreeSet<Agent>,
    notice: bool,
    verify: bool,
) -> Result<Envelope<IndexStats>> {
    let readers = sources::registry_in(&index.context, &index.staging.path);
    run_noting(index, agents, readers, notice, verify)
}

#[cfg(test)]
fn run_sources(
    index: &mut IndexSession,
    agents: &BTreeSet<Agent>,
    readers: Vec<Box<dyn Source>>,
) -> Result<Envelope<IndexStats>> {
    run_noting(index, agents, readers, false, false)
}

fn run_noting(
    index: &mut IndexSession,
    agents: &BTreeSet<Agent>,
    readers: Vec<Box<dyn Source>>,
    notice: bool,
    verify: bool,
) -> Result<Envelope<IndexStats>> {
    let mut writer = index
        .writer(REFRESH_WAIT, notice)?
        .ok_or(Error::IndexBusy)?;
    let target = index.context.coverage;
    if !verify && writer.pending.is_some() {
        let started = Instant::now();
        drop(writer);
        index.reopen()?;
        return Ok(super::skipped(
            vec![super::refresh_in_progress(true)],
            started,
        ));
    }
    if verify && (writer.coverage != target || writer.pending.is_some()) {
        store::rebuild_documents(&mut writer.connection, target)?;
        writer.coverage = target;
        writer.pending = None;
    }
    let result = write(index, &mut writer, agents, readers, verify)?;
    index.publish(writer, false)?;
    Ok(result)
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the transaction lifecycle visible"
)]
pub(super) fn write(
    index: &mut IndexSession,
    writer: &mut Writer,
    agents: &BTreeSet<Agent>,
    readers: Vec<Box<dyn Source>>,
    verify: bool,
) -> Result<Envelope<IndexStats>> {
    let started = Instant::now();
    let seq = store::begin_run(&writer.connection)?;
    let mut stats = IndexStats {
        discovered: 0,
        reused: 0,
        reparsed: 0,
        removed: 0,
        failed: 0,
        elapsed_ms: 0,
    };
    let mut diagnostics = Vec::new();
    let selected = readers
        .into_iter()
        .filter(|s| agents.is_empty() || agents.contains(&s.agent()));
    for source in selected {
        let agent = source.agent();
        index.refreshed.remove(&agent);
        let previous = DiscoveryCache {
            verify,
            ..store::cache(&writer.connection, &index.namespace, agent)?
        };
        let partial = store::partial(&writer.connection, &index.namespace, agent)?;
        let Ok(discovery) = source.discover(&index.context, &previous) else {
            let failed = [diagnostic(
                agent,
                None,
                "source_unavailable",
                "Selected source could not be enumerated",
            )];
            store::discovery_failed(&writer.connection, &index.namespace, agent, &failed, seq)?;
            diagnostics.extend(failed);
            index.refreshed.insert(agent);
            continue;
        };
        let generation = store::next_generation(&writer.connection, &index.namespace, agent)?;
        let mut enumeration_complete =
            discovery.complete_agents.contains(&agent) && discovery.diagnostics.is_empty();
        let mut agent_diagnostics = discovery.diagnostics;
        let sessions = discovery.sessions;
        if !enumeration_complete && agent_diagnostics.is_empty() {
            agent_diagnostics.push(diagnostic(
                agent,
                None,
                "incomplete_inventory",
                "Selected source inventory is incomplete",
            ));
        }
        stats.discovered += sessions.len() as u64;
        let mut identities = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for mut raw in sessions {
            seen.insert(raw.source.clone());
            if raw.key.agent != agent || !identities.insert(raw.key.id.clone()) {
                enumeration_complete = false;
                stats.failed += 1;
                agent_diagnostics.push(diagnostic(
                    agent,
                    Some(raw.source),
                    "duplicate_identity",
                    "Source inventory contains an invalid or duplicate identity",
                ));
                continue;
            }
            let reusable = raw.snapshot.is_none()
                && previous.by_source.get(&raw.source).is_some_and(|cached| {
                    cached.key == raw.key
                        && cached.dependencies == raw.dependencies
                        && cached.fingerprint == raw.fingerprint
                });
            if reusable {
                writer.connection.execute("UPDATE sources SET generation=?1 WHERE namespace=?2 AND agent=?3 AND source=?4 AND status='fresh'",params![generation,index.namespace,agent.to_string(),raw.source])?;
                stats.reused += 1;
                continue;
            }
            if matches!(
                agent,
                Agent::Claude | Agent::Codex | Agent::Droid | Agent::Pi
            ) {
                raw.resume = previous
                    .by_source
                    .get(&raw.source)
                    .or_else(|| partial.get(&raw.source))
                    .and_then(|cached| match &cached.state {
                        Some(SourceState::Append(append))
                            if cached.key == raw.key && cached.dependencies == raw.dependencies =>
                        {
                            Some(append.clone())
                        }
                        _ => None,
                    });
            }
            let mut outcome = ingest(index, writer, source.as_ref(), &mut raw, generation)?;
            if matches!(outcome, Outcome::Changed) {
                enumeration_complete = false;
                agent_diagnostics.push(diagnostic(
                    agent,
                    Some(raw.source.clone()),
                    "source_changed",
                    "Source changed during refresh; inventory will be reconciled again",
                ));
                let empty = DiscoveryCache {
                    by_source: BTreeMap::new(),
                    verify: false,
                };
                if let Ok(mut discovery) = source.discover(&index.context, &empty)
                    && let Some(found) = discovery
                        .sessions
                        .iter()
                        .position(|s| s.source == raw.source)
                        .or_else(|| discovery.sessions.iter().position(|s| s.key == raw.key))
                {
                    raw = discovery.sessions.swap_remove(found);
                    seen.insert(raw.source.clone());
                    outcome = ingest(index, writer, source.as_ref(), &mut raw, generation)?;
                }
            }
            match outcome {
                Outcome::Stable {
                    complete,
                    diagnostics: found,
                } => {
                    stats.reparsed += 1;
                    if !complete {
                        stats.failed += 1;
                    }
                    agent_diagnostics.extend(found);
                }
                Outcome::Changed | Outcome::Failed => {
                    stats.failed += 1;
                    let failed = diagnostic(
                        agent,
                        Some(raw.source.clone()),
                        "source_unavailable",
                        "Source could not be parsed consistently; previous entries are excluded",
                    );
                    store::source(
                        &writer.connection,
                        &index.namespace,
                        &raw,
                        "failed",
                        std::slice::from_ref(&failed),
                        generation,
                        None,
                    )?;
                    agent_diagnostics.push(failed);
                }
            }
        }
        let transaction = writer.connection.transaction()?;
        if enumeration_complete {
            stats.removed += transaction.execute(
                "DELETE FROM sources WHERE namespace=?1 AND agent=?2 AND generation<>?3",
                params![index.namespace, agent.to_string(), generation],
            )? as u64;
        } else {
            store::carry_forward(&transaction, &index.namespace, agent, generation, &seen)?;
        }
        let complete = enumeration_complete && agent_diagnostics.is_empty();
        transaction.execute(
            "UPDATE refresh_runs SET status=?1,diagnostics=?2,published=generation,published_seq=?3 WHERE namespace=?4 AND agent=?5",
            params![
                if complete { "complete" } else { "incomplete" },
                store::encode(&agent_diagnostics)?,
                seq,
                index.namespace,
                agent.to_string()
            ],
        )?;
        transaction.commit()?;
        diagnostics.extend(agent_diagnostics);
        index.refreshed.insert(agent);
    }
    stats.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    Ok(Envelope {
        version: 1,
        incomplete: !diagnostics.is_empty(),
        diagnostics,
        data: Some(stats),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Discovery, Entry, ParsedSession, SessionKey};
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

    fn context(base: &Path) -> Context {
        Context {
            roots: crate::resolve_roots(base, &BTreeMap::new()).unwrap(),
            index_dir: base.join("index"),
            now_ms: 0,
            cwd: base.into(),
            time_zone: "UTC".into(),
            coverage: crate::Coverage::Compact,
        }
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
            index.refresh_under_lease(&agents).unwrap();
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
            assert!(!index.refresh_under_lease(&agents).unwrap().incomplete);
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
        index.refresh_under_lease(&agents).unwrap();
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
                return Err(Error::ResumeRejected);
            }
            if self.fallback {
                self.reader.parse(raw, emit)
            } else {
                Err(Error::InvalidArgument("synthetic parse failure".into()))
            }
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
            index.refresh_under_lease(&agents).unwrap();
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
                let cold = Context {
                    index_dir: dir.path().join("cold"),
                    ..context(dir.path())
                };
                let mut other = IndexSession::open(&cold).unwrap();
                other.refresh_under_lease(&agents).unwrap();
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
        assert!(!index.refresh_under_lease(&agents).unwrap().incomplete);
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
            .refresh_under_lease(&BTreeSet::from([Agent::Claude]))
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
        let value = serde_json::json!({"name":"Store title"})
            .to_string()
            .bytes()
            .fold(String::new(), |mut hex, byte| {
                std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}")).unwrap();
                hex
            });
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
        let empty = DiscoveryCache {
            by_source: BTreeMap::new(),
            verify: false,
        };
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
        index.refresh_under_lease(&agents).unwrap();
        assert_eq!(texts(&index, &key), ["first", "second"]);
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
            .refresh_under_lease(&BTreeSet::from([Agent::Claude]))
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
        let result = index.refresh_under_lease(&agents).unwrap();
        assert!(result.incomplete);
        assert_eq!(codes(&result), ["refresh_in_progress"]);
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
        let result = index.refresh_under_lease(&agents).unwrap();
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
            .prepare(&agents, std::time::Duration::from_millis(50))
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
        let result = next.refresh_for_query(&agents).unwrap();
        assert_eq!(codes(&result), [] as [&str; 0]);
        let stats = result.data.unwrap();
        assert_eq!((stats.reused, stats.reparsed), (1, 1));
        for id in ["one", "two"] {
            assert_eq!(texts(&next, &session(id)), [format!("{id} replaced")]);
        }
        next.verify_integrity().unwrap();
    }
}
