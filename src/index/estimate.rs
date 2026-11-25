use crate::{Agent, AgentEstimate, Coverage, Diagnostic, Projection, RawSession};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const CALIBRATION: [(Agent, u64, f64, u64, f64); 7] = [
    (Agent::Claude, 31_000, 0.64, 14_000, 0.99),
    (Agent::Codex, 17_000, 1.45, 4_500, 3.15),
    (Agent::Cursor, 238_000, 0.09, 238_000, 0.09),
    (Agent::Devin, 31_000, 0.16, 28_000, 0.24),
    (Agent::Droid, 34_000, 1.11, 7_200, 2.40),
    (Agent::Opencode, 63_000, 0.36, 57_000, 0.40),
    (Agent::Pi, 30_000, 1.26, 13_000, 2.06),
];
const LEARNED_AFTER: u64 = 64 << 20;
const OBSERVE_AFTER: u64 = 1 << 20;
const HASH_RATE: u64 = 1_000_000;
const SESSION_MS: u64 = 2;

fn calibration(agent: Agent, coverage: Coverage) -> (u64, f64) {
    let (_, compact_rate, compact_ratio, full_rate, full_ratio) = CALIBRATION
        .into_iter()
        .find(|row| row.0 == agent)
        .unwrap_or(CALIBRATION[0]);
    match coverage {
        Coverage::Compact => (compact_rate, compact_ratio),
        Coverage::Full => (full_rate, full_ratio),
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
    (bytes, hashed): (u64, u64),
    observed: impl Fn(Coverage) -> Option<(u64, u64)>,
) -> AgentEstimate {
    let parsed = bytes.saturating_sub(hashed);
    let projection = |coverage| {
        let (rate, ratio) = calibration(agent, coverage);
        let parse = observed(coverage)
            .filter(|(observed, _)| *observed >= LEARNED_AFTER)
            .map_or_else(
                || millis(parsed, 1, rate.into()),
                |(observed_bytes, observed_micros)| {
                    millis(
                        parsed,
                        observed_micros.into(),
                        u128::from(observed_bytes) * 1_000,
                    )
                },
            );
        Projection {
            index_bytes: scaled(bytes, ratio),
            millis: parse
                .saturating_add(hashed.div_ceil(HASH_RATE))
                .saturating_add(sessions.saturating_mul(SESSION_MS)),
        }
    };
    AgentEstimate {
        agent,
        sessions,
        bytes,
        compact: projection(Coverage::Compact),
        full: projection(Coverage::Full),
    }
}

pub(crate) const fn projection(estimate: &AgentEstimate, coverage: Coverage) -> &Projection {
    match coverage {
        Coverage::Compact => &estimate.compact,
        Coverage::Full => &estimate.full,
    }
}

fn unique(raw: &RawSession) -> BTreeSet<&Path> {
    raw.dependencies.iter().map(PathBuf::as_path).collect()
}

pub(super) struct Sizes<'a> {
    references: BTreeMap<&'a Path, u64>,
    sizes: BTreeMap<PathBuf, u64>,
}

impl<'a> Sizes<'a> {
    pub(super) fn new(sessions: &'a [RawSession]) -> Self {
        let mut references = BTreeMap::new();
        for raw in sessions {
            for path in unique(raw) {
                *references.entry(path).or_insert(0) += 1;
            }
        }
        Self {
            references,
            sizes: BTreeMap::new(),
        }
    }

    fn size(&mut self, path: &Path) -> u64 {
        *self
            .sizes
            .entry(path.to_path_buf())
            .or_insert_with(|| fs::metadata(path).map_or(0, |m| m.len()))
    }

    pub(super) fn work(&mut self, raw: &RawSession) -> (u64, u64) {
        if let (Some(state), Some(transcript)) = (&raw.resume, raw.dependencies.first()) {
            let size = self.size(transcript);
            return (size.max(state.consumed), state.consumed);
        }
        let mut total = 0u64;
        for path in unique(raw) {
            let shared = self.references.get(path).copied().unwrap_or(1);
            total = total.saturating_add(self.size(path).div_ceil(shared.max(1)));
        }
        (total, 0)
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

pub fn progress_line(sessions: (u64, u64), bytes: (u64, u64)) -> String {
    let (divisor, name) = unit(bytes.1);
    format!(
        "indexing: {}/{} sessions, {}/{} {name}",
        count(sessions.0),
        count(sessions.1),
        amount(bytes.0, divisor, whole(bytes.1, divisor)),
        amount(bytes.1, divisor, whole(bytes.1, divisor))
    )
}

pub fn summary_line(sessions: u64, bytes: u64, millis: u64) -> String {
    format!(
        "indexed {} ({} of logs) in {}",
        self::sessions(sessions),
        size(bytes),
        duration(millis)
    )
}

pub fn required(estimate: &AgentEstimate, coverage: Coverage) -> Diagnostic {
    let projection = projection(estimate, coverage);
    Diagnostic {
        agent: Some(estimate.agent),
        source: None,
        code: "index_required".into(),
        message: format!(
            "{} ({} of logs) need indexing, estimated at {} and {} of index at {} coverage; run `slimetrail index` to build them",
            sessions(estimate.sessions),
            size(estimate.bytes),
            duration(projection.millis),
            size(projection.index_bytes),
            super::coverage::coverage_name(coverage)
        ),
    }
}

pub(super) struct Progress {
    enabled: bool,
    started: Instant,
    shown: Option<Instant>,
    sessions: (u64, u64),
    bytes: (u64, u64),
}

impl Progress {
    pub(super) fn new(enabled: bool, sessions: u64, bytes: u64) -> Self {
        Self {
            enabled: enabled && sessions > 0,
            started: Instant::now(),
            shown: None,
            sessions: (0, sessions),
            bytes: (0, bytes),
        }
    }

    pub(super) fn advance(&mut self, bytes: u64) {
        self.sessions.0 = (self.sessions.0 + 1).min(self.sessions.1);
        self.bytes.0 = self.bytes.0.saturating_add(bytes).min(self.bytes.1);
        if self.enabled
            && self
                .shown
                .is_none_or(|shown| shown.elapsed() >= Duration::from_millis(250))
        {
            self.shown = Some(Instant::now());
            self.show();
        }
    }

    fn show(&self) {
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r{}", progress_line(self.sessions, self.bytes));
        let _ = stderr.flush();
    }

    pub(super) fn finish(self) {
        if !self.enabled {
            return;
        }
        self.show();
        let elapsed = self
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        eprintln!();
        eprintln!("{}", summary_line(self.sessions.0, self.bytes.0, elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_and_summary_lines_are_human_readable() {
        assert_eq!(
            progress_line((1_204, 4_210), (800_000_000, 3_100_000_000)),
            "indexing: 1,204/4,210 sessions, 0.8/3.1 GB"
        );
        assert_eq!(
            progress_line((0, 3), (0, 52_400)),
            "indexing: 0/3 sessions, 0.0/52.4 KB"
        );
        assert_eq!(
            summary_line(4_210, 3_100_000_000, 125_000),
            "indexed 4,210 sessions (3.1 GB of logs) in 3 min"
        );
        assert_eq!(
            summary_line(1, 200, 5),
            "indexed 1 session (0.2 KB of logs) in 1 s"
        );
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
        let claude = agent(Agent::Claude, 10, (31_000_000, 0), |_| None);
        assert_eq!(claude.compact.millis, 1_000 + 20);
        assert_eq!(claude.full.millis, 2_215 + 20);
        assert_eq!(claude.compact.index_bytes, 19_840_000);
        let small = agent(Agent::Claude, 10, (31_000_000, 0), |_| {
            Some((1 << 20, 1_000))
        });
        assert_eq!(small, claude);
        let learned = agent(Agent::Claude, 10, (31_000_000, 0), |coverage| {
            (coverage == Coverage::Compact).then_some((LEARNED_AFTER, 64_000 << 10))
        });
        assert_eq!(learned.compact.millis, 30_274 + 20);
        assert_eq!(learned.full.millis, 2_215 + 20);
        let full = agent(Agent::Claude, 10, (31_000_000, 0), |coverage| {
            (coverage == Coverage::Full).then_some((LEARNED_AFTER, 64_000 << 10))
        });
        assert_eq!((full.compact.millis, full.full.millis), (1_020, 30_294));
    }

    #[test]
    fn resumed_prefixes_cost_hashing_instead_of_parsing() {
        let resumed = agent(Agent::Claude, 1, (31_000_000, 30_000_000), |_| None);
        assert_eq!(resumed.compact.millis, 33 + 30 + 2);
        assert_eq!(resumed.full.millis, 72 + 30 + 2);
        assert_eq!(hashing_micros(30_000_000), 30_000);
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
}
