use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

pub type Timestamp = i64;
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Claude,
    Codex,
    Cursor,
    Devin,
    Droid,
    Opencode,
    Pi,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent: Agent,
    pub id: String,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Message,
    Tool,
    Meta,
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    #[default]
    Compact,
    Full,
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Searchable {
    #[default]
    Text,
    ToolResult,
    Duplicate,
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum Admission {
    #[default]
    PerAgent,
    AllOrNothing,
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum BuildPolicy {
    #[default]
    Inline,
    Limit {
        max_ms: u64,
        admission: Admission,
    },
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum RefreshMode {
    #[default]
    Query,
    Full,
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub sessions: u64,
    pub total_sessions: u64,
    pub bytes: u64,
    pub total_bytes: u64,
}
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct Rebuild {
    pub entries: u64,
    pub total_entries: u64,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RefreshEvent {
    Waiting,
    Rebuilding(Rebuild),
    Started(Progress),
    Advanced(Progress),
    Finished(Progress),
}
pub trait Observer {
    fn observe(&self, event: RefreshEvent);
}
#[derive(Copy, Clone)]
pub struct RefreshOptions<'a> {
    pub mode: RefreshMode,
    pub wait: Duration,
    pub build: BuildPolicy,
    pub observer: Option<&'a dyn Observer>,
}
impl RefreshOptions<'static> {
    pub const QUERY: Self = Self {
        mode: RefreshMode::Query,
        wait: Duration::from_secs(5),
        build: BuildPolicy::Inline,
        observer: None,
    };
    pub const FULL: Self = Self {
        mode: RefreshMode::Full,
        wait: Duration::from_secs(3600),
        build: BuildPolicy::Inline,
        observer: None,
    };
}
impl Default for RefreshOptions<'_> {
    fn default() -> Self {
        RefreshOptions::QUERY
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub codex_archived_sessions: PathBuf,
    pub codex_session_index: PathBuf,
    pub cursor_projects: PathBuf,
    pub cursor_chats: PathBuf,
    pub devin_sessions_db: PathBuf,
    pub droid_sessions: PathBuf,
    pub pi_sessions: PathBuf,
    pub opencode_db: PathBuf,
    pub opencode_legacy_storage: PathBuf,
    pub archive: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Context {
    pub roots: Roots,
    pub index_dir: PathBuf,
    pub now_ms: Timestamp,
    pub cwd: PathBuf,
    pub time_zone: String,
    #[serde(default)]
    pub coverage: Coverage,
    #[serde(default)]
    pub coverage_explicit: bool,
}
impl Context {
    pub fn new(
        roots: Roots,
        index_dir: PathBuf,
        cwd: PathBuf,
        now_ms: Timestamp,
        time_zone: String,
    ) -> Self {
        Self {
            roots,
            index_dir,
            now_ms,
            cwd,
            time_zone,
            coverage: Coverage::default(),
            coverage_explicit: false,
        }
    }
    #[must_use]
    pub fn with_coverage(self, coverage: Coverage, explicit: bool) -> Self {
        Self {
            coverage,
            coverage_explicit: explicit,
            ..self
        }
    }
}
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TitleOrigin {
    Native,
    Derived,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub key: SessionKey,
    pub title: Option<String>,
    pub title_origin: Option<TitleOrigin>,
    pub source_updated_at: Option<Timestamp>,
    pub cwd: Option<PathBuf>,
    pub branch: Option<String>,
    pub started_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub parent: Option<SessionKey>,
    pub source: String,
    pub archived: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub session: SessionKey,
    pub index: u64,
    pub source_id: String,
    pub timestamp: Option<Timestamp>,
    pub timestamp_raw: Option<String>,
    pub role: String,
    pub kind: Kind,
    pub subtype: Option<String>,
    pub tool_name: Option<String>,
    pub automated: bool,
    pub text: String,
    pub searchable: Searchable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub agent: Option<Agent>,
    pub source: Option<String>,
    pub code: String,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub incomplete: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub data: Option<T>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub discovered: u64,
    pub reused: u64,
    pub reparsed: u64,
    pub removed: u64,
    pub failed: u64,
    pub elapsed_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    pub index_bytes: u64,
    pub millis: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEstimate {
    pub agent: Agent,
    pub sessions: u64,
    pub bytes: u64,
    pub compact: Projection,
    pub full: Projection,
}
impl AgentEstimate {
    pub const fn projection(&self, coverage: Coverage) -> &Projection {
        match coverage {
            Coverage::Compact => &self.compact,
            Coverage::Full => &self.full,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEstimate {
    pub agents: Vec<AgentEstimate>,
    pub coverage: Coverage,
    pub coverage_locked: bool,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Current,
    Siblings,
    All,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub agents: BTreeSet<Agent>,
    pub cwd: PathBuf,
    pub scope: Scope,
    pub app: Option<String>,
    pub days: Option<u32>,
    pub kinds: BTreeSet<Kind>,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Score,
    Recent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub filter: Filter,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    #[serde(default)]
    pub word: bool,
    pub sort: Sort,
    pub max_results: usize,
    pub max_snippets: usize,
    pub context_chars: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub session: Session,
    pub score: u64,
    pub matches: u64,
    pub by_kind: BTreeMap<String, u64>,
    pub snippets: Vec<String>,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ReadMode {
    Lite,
    Full,
    Log,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOptions {
    pub mode: ReadMode,
    pub offset: usize,
    pub max_chars: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadPage {
    pub session: Session,
    pub mode: ReadMode,
    pub text: String,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub total_chars: usize,
    pub returned_chars: usize,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineOptions {
    pub filter: Filter,
    pub since: Timestamp,
    pub until: Timestamp,
    pub idle_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: Timestamp,
    pub end: Timestamp,
    pub human_prompts: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSession {
    pub session: Session,
    pub spans: Vec<Span>,
    pub active_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineResult {
    pub sessions: Vec<TimelineSession>,
    pub daily_active_ms: BTreeMap<String, u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: String,
    pub agents: Vec<Agent>,
    pub sort: Sort,
    pub count: usize,
    pub results: Vec<SearchHit>,
}
