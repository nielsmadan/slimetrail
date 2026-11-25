use crate::{Agent, Diagnostic, Session, SessionKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_mark: Option<String>,
}
#[derive(Debug)]
pub struct PrivateSnapshot {
    pub directory: tempfile::TempDir,
    pub database: PathBuf,
}
#[derive(Debug)]
pub struct RawSession {
    pub key: SessionKey,
    pub source: String,
    pub dependencies: Vec<PathBuf>,
    pub fingerprint: Fingerprint,
    pub snapshot: Option<Arc<PrivateSnapshot>>,
    pub resume: Option<Resume>,
}
impl RawSession {
    pub(crate) fn without_resume(&self) -> Self {
        Self {
            key: self.key.clone(),
            source: self.source.clone(),
            dependencies: self.dependencies.clone(),
            fingerprint: self.fingerprint.clone(),
            snapshot: self.snapshot.clone(),
            resume: None,
        }
    }
}
pub const PREFIX_CHUNK: u64 = 1 << 20;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixCheck {
    Ends,
    Whole,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub state: AppendState,
    pub check: PrefixCheck,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendState {
    pub consumed: u64,
    pub chunks: Vec<String>,
    pub spot_checked: bool,
    pub lines: u64,
    pub entries: u64,
    pub malformed: u64,
    pub dev: u64,
    pub ino: u64,
    pub others: String,
    pub session: Session,
}
impl AppendState {
    pub const fn chunk_count(&self) -> u64 {
        self.consumed.div_ceil(PREFIX_CHUNK)
    }
    pub fn chunk_len(&self, chunk: u64) -> u64 {
        self.consumed
            .saturating_sub(chunk.saturating_mul(PREFIX_CHUNK))
            .min(PREFIX_CHUNK)
    }
    pub fn checked_chunks(&self, check: PrefixCheck) -> Vec<u64> {
        let count = self.chunk_count();
        match check {
            PrefixCheck::Whole => (0..count).collect(),
            PrefixCheck::Ends => {
                let tail = self.consumed.saturating_sub(PREFIX_CHUNK) / PREFIX_CHUNK;
                (0..count.min(1)).chain(tail.max(1)..count).collect()
            }
        }
    }
    pub fn spot_checked_after(&self, check: PrefixCheck) -> bool {
        check == PrefixCheck::Ends
            && (self.spot_checked || (self.checked_chunks(check).len() as u64) < self.chunk_count())
    }
    pub fn checked_bytes(&self, check: PrefixCheck) -> u64 {
        self.checked_chunks(check)
            .into_iter()
            .map(|chunk| self.chunk_len(chunk))
            .sum()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "Stored once per source; the shared interface keeps AppendState inline"
)]
pub enum SourceState {
    Append(AppendState),
    Content { sha256: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSource {
    pub key: SessionKey,
    pub source: String,
    pub dependencies: Vec<PathBuf>,
    pub fingerprint: Fingerprint,
    pub state: Option<SourceState>,
}
#[derive(Debug, Default)]
pub struct Discovery {
    pub sessions: Vec<RawSession>,
    pub diagnostics: Vec<Diagnostic>,
    pub advisories: Vec<Diagnostic>,
    pub complete_agents: BTreeSet<Agent>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryCache {
    pub by_source: BTreeMap<String, CachedSource>,
    pub verify: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSession {
    pub session: Session,
    pub diagnostics: Vec<Diagnostic>,
    pub complete: bool,
    pub state: Option<SourceState>,
    pub recheck: bool,
}
