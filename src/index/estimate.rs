use crate::{
    Admission, Agent, AgentEstimate, BuildPolicy, Coverage, Diagnostic, Progress, Projection,
    RawSession, Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
struct Rate {
    bytes_per_ms: u64,
    index_ratio: f64,
}

const fn rate(bytes_per_ms: u64, index_ratio: f64) -> Rate {
    Rate {
        bytes_per_ms,
        index_ratio,
    }
}

const fn calibration(agent: Agent, coverage: Coverage) -> Rate {
    let (compact, full) = match agent {
        Agent::Claude => (rate(31_000, 0.64), rate(14_000, 0.99)),
        Agent::Codex => (rate(17_000, 1.45), rate(4_500, 3.15)),
        Agent::Cursor => (rate(238_000, 0.09), rate(238_000, 0.09)),
        Agent::Devin => (rate(31_000, 0.16), rate(28_000, 0.24)),
        Agent::Droid => (rate(34_000, 1.11), rate(7_200, 2.40)),
        Agent::Opencode => (rate(63_000, 0.36), rate(57_000, 0.40)),
        Agent::Pi => (rate(30_000, 1.26), rate(13_000, 2.06)),
    };
    match coverage {
        Coverage::Compact => compact,
        Coverage::Full => full,
    }
}
const LEARNED_AFTER: u64 = 64 << 20;
const OBSERVE_AFTER: u64 = 1 << 20;
const HASH_RATE: u64 = 1_000_000;
const SESSION_MS: u64 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Work {
    pub(super) bytes: u64,
    pub(super) consumed: u64,
    pub(super) hashed: u64,
}

impl Work {
    pub(super) const fn parsed(self) -> u64 {
        self.bytes.saturating_sub(self.consumed)
    }

    pub(super) const fn reparsed(self) -> Self {
        Self {
            bytes: self.bytes,
            consumed: 0,
            hashed: 0,
        }
    }

    const fn plus(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(other.bytes),
            consumed: self.consumed.saturating_add(other.consumed),
            hashed: self.hashed.saturating_add(other.hashed),
        }
    }
}

impl std::iter::Sum for Work {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), Self::plus)
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Index size projections are coarse estimates"
)]
fn scaled(bytes: u64, ratio: f64) -> u64 {
    (bytes as f64 * ratio).ceil().min(u64::MAX as f64) as u64
}

fn millis(bytes: u64, numerator: u128, denominator: u128) -> u64 {
    (u128::from(bytes) * numerator)
        .div_ceil(denominator.max(1))
        .try_into()
        .unwrap_or(u64::MAX)
}

fn hashing_micros(hashed: u64) -> u64 {
    millis(hashed, 1_000, HASH_RATE.into())
}

pub(super) fn observation(parsed: u64, hashed: u64, micros: u64) -> Option<(u64, u64)> {
    let spent = micros.saturating_sub(hashing_micros(hashed));
    (parsed >= OBSERVE_AFTER && spent > 0).then_some((parsed, spent))
}

pub(super) fn agent(
    agent: Agent,
    sessions: u64,
    work: Work,
    observed: impl Fn(Coverage) -> Option<(u64, u64)>,
) -> AgentEstimate {
    let parsed = work.parsed();
    let projection = |coverage| {
        let rate = calibration(agent, coverage);
        let parse = observed(coverage)
            .filter(|(observed, _)| *observed >= LEARNED_AFTER)
            .map_or_else(
                || millis(parsed, 1, rate.bytes_per_ms.into()),
                |(observed_bytes, observed_micros)| {
                    millis(
                        parsed,
                        observed_micros.into(),
                        u128::from(observed_bytes) * 1_000,
                    )
                },
            );
        Projection {
            index_bytes: scaled(parsed, rate.index_ratio),
            millis: parse
                .saturating_add(work.hashed.div_ceil(HASH_RATE))
                .saturating_add(sessions.saturating_mul(SESSION_MS)),
        }
    };
    AgentEstimate {
        agent,
        sessions,
        bytes: parsed,
        compact: projection(Coverage::Compact),
        full: projection(Coverage::Full),
    }
}

fn unique(raw: &RawSession) -> BTreeSet<&Path> {
    raw.dependencies.iter().map(PathBuf::as_path).collect()
}

pub(super) struct Sizes<'a> {
    references: BTreeMap<&'a Path, u64>,
}

impl<'a> Sizes<'a> {
    pub(super) fn new(sessions: &'a [RawSession]) -> Self {
        let mut references = BTreeMap::new();
        for path in sessions.iter().flat_map(unique) {
            *references.entry(path).or_insert(0) += 1;
        }
        Self { references }
    }

    pub(super) fn work(
        &self,
        raw: &RawSession,
        transcript: Option<&Path>,
        size: &mut dyn FnMut(&Path) -> u64,
    ) -> Work {
        if let (Some(resume), Some(transcript)) = (&raw.resume, transcript) {
            return Work {
                bytes: size(transcript).max(resume.state.consumed),
                consumed: resume.state.consumed,
                hashed: resume.state.checked_bytes(resume.check),
            };
        }
        Work {
            consumed: 0,
            bytes: unique(raw)
                .into_iter()
                .map(|path| {
                    let shared = self.references.get(path).copied().unwrap_or(1);
                    size(path).div_ceil(shared.max(1))
                })
                .fold(0, u64::saturating_add),
            hashed: 0,
        }
    }
}

pub(super) fn admit(estimates: &[(Agent, u64, u64)], limit: Option<u64>) -> BTreeSet<Agent> {
    let Some(limit) = limit else {
        return estimates.iter().map(|(agent, _, _)| *agent).collect();
    };
    let mut ordered = estimates.to_vec();
    ordered.sort_by_key(|(agent, _, millis)| (*millis, *agent));
    let mut admitted = BTreeSet::new();
    let mut total = 0u64;
    for (agent, sessions, millis) in ordered {
        if sessions == 0 {
            admitted.insert(agent);
            continue;
        }
        total = total.saturating_add(millis);
        if total > limit {
            break;
        }
        admitted.insert(agent);
    }
    admitted
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Gate {
    Build(BTreeSet<Agent>),
    Skip(Vec<AgentEstimate>),
    Abort(Vec<AgentEstimate>),
}

pub(super) fn gate(
    estimates: &[AgentEstimate],
    coverage: Coverage,
    build: BuildPolicy,
    published: &mut dyn FnMut(Agent) -> Result<bool>,
) -> Result<Gate> {
    let (limit, admission) = match build {
        BuildPolicy::Inline => (None, Admission::PerAgent),
        BuildPolicy::Limit { max_ms, admission } => (Some(max_ms), admission),
    };
    let admitted = admit(
        &estimates
            .iter()
            .map(|e| (e.agent, e.sessions, e.projection(coverage).millis))
            .collect::<Vec<_>>(),
        limit,
    );
    if estimates.iter().all(|e| admitted.contains(&e.agent)) {
        return Ok(Gate::Build(admitted));
    }
    let usable = admission == Admission::PerAgent
        && (estimates
            .iter()
            .any(|e| admitted.contains(&e.agent) && e.sessions > 0)
            || estimates.iter().try_fold(false, |found, e| {
                Ok::<_, crate::Error>(found || published(e.agent)?)
            })?);
    if usable {
        return Ok(Gate::Build(admitted));
    }
    let mut pending: Vec<_> = estimates
        .iter()
        .filter(|e| e.sessions > 0)
        .cloned()
        .collect();
    pending.sort_by_key(|e| e.agent);
    Ok(match admission {
        Admission::PerAgent => Gate::Skip(pending),
        Admission::AllOrNothing => Gate::Abort(pending),
    })
}

pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

pub fn sessions(value: u64) -> String {
    format!(
        "{} {}",
        count(value),
        if value == 1 { "session" } else { "sessions" }
    )
}

const fn unit(bytes: u64) -> (u64, &'static str) {
    match bytes {
        0..1_000_000 => (1_000, "KB"),
        1_000_000..1_000_000_000 => (1_000_000, "MB"),
        _ => (1_000_000_000, "GB"),
    }
}

fn amount(bytes: u64, divisor: u64, whole: bool) -> String {
    let tenths = (u128::from(bytes) * 10 + u128::from(divisor) / 2) / u128::from(divisor);
    if whole {
        ((tenths + 5) / 10).to_string()
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

const fn whole(bytes: u64, divisor: u64) -> bool {
    divisor < 1_000_000_000 && bytes >= divisor.saturating_mul(100)
}

pub fn size(bytes: u64) -> String {
    let (divisor, name) = unit(bytes);
    format!("{} {name}", amount(bytes, divisor, whole(bytes, divisor)))
}

pub fn duration(millis: u64) -> String {
    let seconds = millis.div_ceil(1000);
    if seconds < 60 {
        format!("{} s", seconds.max(1))
    } else if seconds < 3600 {
        format!("{} min", seconds.div_ceil(60))
    } else {
        let minutes = seconds.div_ceil(60);
        format!("{} h {} min", minutes / 60, minutes % 60)
    }
}

pub fn size_progress(done: u64, total: u64) -> String {
    let (divisor, name) = unit(total);
    let whole = whole(total, divisor);
    format!(
        "{}/{} {name}",
        amount(done, divisor, whole),
        amount(total, divisor, whole)
    )
}

pub(crate) fn required(estimate: &AgentEstimate, coverage: Coverage) -> Diagnostic {
    let projection = estimate.projection(coverage);
    let (verb, pronoun) = if estimate.sessions == 1 {
        ("needs", "it")
    } else {
        ("need", "them")
    };
    Diagnostic {
        agent: Some(estimate.agent),
        source: None,
        code: "index_required".into(),
        message: format!(
            "{} ({} of logs) {verb} indexing, estimated at {} and {} of index at {} coverage; run `slimetrail index` to build {pronoun}",
            sessions(estimate.sessions),
            size(estimate.bytes),
            duration(projection.millis),
            size(projection.index_bytes),
            coverage.as_str()
        ),
    }
}

impl Progress {
    pub(super) const fn new(total_sessions: u64, total_bytes: u64) -> Self {
        Self {
            sessions: 0,
            total_sessions,
            bytes: 0,
            total_bytes,
        }
    }

    pub(super) const fn replanned(self, before: (u64, u64), after: (u64, u64)) -> Self {
        Self {
            total_sessions: self
                .total_sessions
                .saturating_sub(before.0)
                .saturating_add(after.0),
            total_bytes: self
                .total_bytes
                .saturating_sub(before.1)
                .saturating_add(after.1),
            ..self
        }
    }

    pub(super) fn advanced(self, bytes: u64) -> Self {
        Self {
            sessions: (self.sessions + 1).min(self.total_sessions),
            bytes: self.bytes.saturating_add(bytes).min(self.total_bytes),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{PREFIX_CHUNK, PrefixCheck};

    const fn work(bytes: u64, consumed: u64) -> Work {
        Work {
            bytes,
            consumed,
            hashed: consumed,
        }
    }

    #[test]
    fn sizes_counts_and_durations_are_human_readable() {
        assert_eq!(size_progress(800_000_000, 3_100_000_000), "0.8/3.1 GB");
        assert_eq!(size_progress(0, 52_400), "0.0/52.4 KB");
        assert_eq!(size_progress(120_000_000, 950_000_000), "120/950 MB");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000_000), "1,000,000");
        assert_eq!(size(950_000_000), "950 MB");
        assert_eq!(size(12_340_000), "12.3 MB");
        assert_eq!(duration(1), "1 s");
        assert_eq!(duration(59_000), "59 s");
        assert_eq!(duration(3_660_000), "1 h 1 min");
    }

    #[test]
    fn defaults_then_learned_throughput_drive_the_projection() {
        let claude = agent(Agent::Claude, 10, work(31_000_000, 0), |_| None);
        assert_eq!(claude.compact.millis, 1_000 + 20);
        assert_eq!(claude.full.millis, 2_215 + 20);
        assert_eq!(claude.compact.index_bytes, 19_840_000);
        let small = agent(Agent::Claude, 10, work(31_000_000, 0), |_| {
            Some((1 << 20, 1_000))
        });
        assert_eq!(small, claude);
        let learned = agent(Agent::Claude, 10, work(31_000_000, 0), |coverage| {
            (coverage == Coverage::Compact).then_some((LEARNED_AFTER, 64_000 << 10))
        });
        assert_eq!(learned.compact.millis, 30_274 + 20);
        assert_eq!(learned.full.millis, 2_215 + 20);
        let full = agent(Agent::Claude, 10, work(31_000_000, 0), |coverage| {
            (coverage == Coverage::Full).then_some((LEARNED_AFTER, 64_000 << 10))
        });
        assert_eq!((full.compact.millis, full.full.millis), (1_020, 30_294));
    }

    #[test]
    fn resumed_prefixes_cost_hashing_instead_of_parsing() {
        let resumed = agent(Agent::Claude, 1, work(31_000_000, 30_000_000), |_| None);
        assert_eq!(resumed.bytes, 1_000_000);
        assert_eq!(resumed.compact.index_bytes, 640_000);
        assert_eq!(resumed.compact.millis, 33 + 30 + 2);
        assert_eq!(resumed.full.millis, 72 + 30 + 2);
        assert_eq!(hashing_micros(30_000_000), 30_000);
    }

    #[test]
    fn required_message_agrees_with_the_session_count() {
        let one = agent(Agent::Pi, 1, work(60_000, 0), |_| None);
        assert_eq!(
            required(&one, Coverage::Compact).message,
            "1 session (60.0 KB of logs) needs indexing, estimated at 1 s and 75.6 KB of index at compact coverage; run `slimetrail index` to build it"
        );
        let two = agent(Agent::Pi, 2, work(60_000, 0), |_| None);
        assert!(
            required(&two, Coverage::Compact)
                .message
                .starts_with("2 sessions (60.0 KB of logs) need indexing,")
        );
        assert!(
            required(&two, Coverage::Compact)
                .message
                .ends_with("to build them")
        );
    }

    #[test]
    fn a_replanned_agent_swaps_its_share_of_the_progress_totals() {
        let progress = Progress::new(10, 1_000).advanced(100);
        assert_eq!(
            progress.replanned((4, 600), (5, 900)),
            Progress {
                sessions: 1,
                total_sessions: 11,
                bytes: 100,
                total_bytes: 1_300,
            }
        );
    }

    #[test]
    fn agents_are_admitted_smallest_first_within_the_limit() {
        let estimates = [
            (Agent::Claude, 5, 900),
            (Agent::Pi, 1, 100),
            (Agent::Codex, 2, 300),
            (Agent::Devin, 0, 0),
        ];
        assert_eq!(
            admit(&estimates, Some(450)),
            BTreeSet::from([Agent::Pi, Agent::Codex, Agent::Devin])
        );
        assert_eq!(admit(&estimates, Some(0)), BTreeSet::from([Agent::Devin]));
        assert_eq!(admit(&estimates, None).len(), 4);
    }

    fn transcript_session(id: &str, dependencies: &[&str], consumed: Option<u64>) -> RawSession {
        resumed_session(id, dependencies, consumed, PrefixCheck::Ends)
    }

    fn resumed_session(
        id: &str,
        dependencies: &[&str],
        consumed: Option<u64>,
        check: PrefixCheck,
    ) -> RawSession {
        let key = crate::SessionKey {
            agent: Agent::Claude,
            id: id.into(),
        };
        RawSession {
            resume: consumed.map(|consumed| crate::sources::Resume {
                check,
                state: crate::AppendState {
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
                        key: key.clone(),
                        title: None,
                        title_origin: None,
                        source_updated_at: None,
                        cwd: None,
                        branch: None,
                        started_at: None,
                        updated_at: None,
                        parent: None,
                        source: id.into(),
                        archived: false,
                    },
                },
            }),
            key,
            source: id.into(),
            dependencies: dependencies.iter().map(PathBuf::from).collect(),
            fingerprint: crate::Fingerprint {
                signature: String::new(),
                title_mark: None,
            },
            snapshot: None,
        }
    }

    fn lookup(sizes: &[(&str, u64)]) -> impl FnMut(&Path) -> u64 {
        let sizes: BTreeMap<PathBuf, u64> = sizes
            .iter()
            .map(|(path, size)| (PathBuf::from(path), *size))
            .collect();
        move |path| sizes[path]
    }

    #[test]
    fn shared_dependencies_split_their_size_across_the_sessions_that_list_them() {
        let sessions = [
            transcript_session("a", &["a", "db", "db"], None),
            transcript_session("b", &["b", "db"], None),
            transcript_session("c", &["c", "db"], None),
        ];
        let sizes = Sizes::new(&sessions);
        let mut size = lookup(&[("a", 10), ("b", 7), ("c", 0), ("db", 100)]);
        let work: Vec<_> = sessions
            .iter()
            .map(|raw| sizes.work(raw, None, &mut size))
            .collect();
        assert_eq!(
            work,
            [self::work(44, 0), self::work(41, 0), self::work(34, 0)]
        );
    }

    #[test]
    fn a_resumed_transcript_costs_its_consumed_prefix_as_hashing() {
        let grown = [transcript_session("t", &["t", "settings"], Some(40))];
        let sizes = Sizes::new(&grown);
        let mut size = lookup(&[("t", 100), ("settings", 5)]);
        let transcript = Some(Path::new("t"));
        assert_eq!(
            sizes.work(&grown[0], transcript, &mut size),
            self::work(100, 40)
        );
        let mut shrunk = lookup(&[("t", 30), ("settings", 5)]);
        assert_eq!(sizes.work(&grown[0], transcript, &mut shrunk).parsed(), 0);
        assert_eq!(sizes.work(&grown[0], None, &mut size), self::work(105, 0));
    }

    #[test]
    fn a_query_resume_hashes_only_the_prefix_ends_and_a_full_refresh_the_whole_prefix() {
        let consumed = 10 * PREFIX_CHUNK + 300;
        let sizes = [("t", consumed + 50)];
        let mut size = lookup(&sizes);
        let transcript = Some(Path::new("t"));
        let ends = [resumed_session(
            "t",
            &["t"],
            Some(consumed),
            PrefixCheck::Ends,
        )];
        assert_eq!(
            Sizes::new(&ends).work(&ends[0], transcript, &mut size),
            Work {
                bytes: consumed + 50,
                consumed,
                hashed: 2 * PREFIX_CHUNK + 300,
            }
        );
        let whole = [resumed_session(
            "t",
            &["t"],
            Some(consumed),
            PrefixCheck::Whole,
        )];
        let work = Sizes::new(&whole).work(&whole[0], transcript, &mut size);
        assert_eq!((work.parsed(), work.hashed), (50, consumed));
    }

    fn estimate(agent: Agent, sessions: u64, millis: u64) -> AgentEstimate {
        let projection = Projection {
            index_bytes: 0,
            millis,
        };
        AgentEstimate {
            agent,
            sessions,
            bytes: 0,
            compact: projection.clone(),
            full: projection,
        }
    }

    const fn limit(max_ms: u64, admission: Admission) -> BuildPolicy {
        BuildPolicy::Limit { max_ms, admission }
    }

    fn decide(
        estimates: &[AgentEstimate],
        build: BuildPolicy,
        published: &[Agent],
    ) -> (Gate, Vec<Agent>) {
        let mut queried = Vec::new();
        let gate = gate(estimates, Coverage::Compact, build, &mut |agent| {
            queried.push(agent);
            Ok(published.contains(&agent))
        })
        .unwrap();
        (gate, queried)
    }

    #[test]
    fn inline_builds_every_agent() {
        let estimates = [estimate(Agent::Claude, 5, 900), estimate(Agent::Pi, 1, 100)];
        assert_eq!(
            decide(&estimates, BuildPolicy::Inline, &[]).0,
            Gate::Build(BTreeSet::from([Agent::Claude, Agent::Pi]))
        );
    }

    #[test]
    fn per_agent_limit_builds_admitted_work_without_querying_published_runs() {
        let estimates = [estimate(Agent::Claude, 5, 900), estimate(Agent::Pi, 1, 100)];
        let (gate, queried) = decide(&estimates, limit(450, Admission::PerAgent), &[]);
        assert_eq!(gate, Gate::Build(BTreeSet::from([Agent::Pi])));
        assert_eq!(queried, []);
    }

    #[test]
    fn per_agent_limit_builds_when_a_gated_agent_already_has_published_data() {
        let estimates = [
            estimate(Agent::Codex, 0, 0),
            estimate(Agent::Claude, 5, 900),
            estimate(Agent::Pi, 3, 800),
        ];
        let (gate, queried) = decide(
            &estimates,
            limit(450, Admission::PerAgent),
            &[Agent::Claude, Agent::Pi],
        );
        assert_eq!(gate, Gate::Build(BTreeSet::from([Agent::Codex])));
        assert_eq!(queried, [Agent::Codex, Agent::Claude]);
    }

    #[test]
    fn per_agent_limit_without_usable_data_skips_with_pending_work_by_agent() {
        let estimates = [
            estimate(Agent::Pi, 2, 900),
            estimate(Agent::Claude, 5, 900),
            estimate(Agent::Codex, 0, 0),
        ];
        assert_eq!(
            decide(&estimates, limit(100, Admission::PerAgent), &[]).0,
            Gate::Skip(vec![
                estimate(Agent::Claude, 5, 900),
                estimate(Agent::Pi, 2, 900)
            ])
        );
    }

    #[test]
    fn all_or_nothing_aborts_with_every_pending_agent_once_one_is_gated() {
        let estimates = [estimate(Agent::Pi, 1, 100), estimate(Agent::Claude, 5, 900)];
        let (gate, queried) = decide(
            &estimates,
            limit(450, Admission::AllOrNothing),
            &[Agent::Claude],
        );
        assert_eq!(
            gate,
            Gate::Abort(vec![
                estimate(Agent::Claude, 5, 900),
                estimate(Agent::Pi, 1, 100)
            ])
        );
        assert_eq!(queried, []);
        assert_eq!(
            decide(&estimates, limit(1_000, Admission::AllOrNothing), &[]).0,
            Gate::Build(BTreeSet::from([Agent::Claude, Agent::Pi]))
        );
    }
}
