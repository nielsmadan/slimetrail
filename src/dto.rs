use crate::{
    Result,
    model::{
        Agent, Coverage, Entry, Kind, ReadMode, ReadPage, SearchResult, Session, SessionKey, Sort,
        TimelineResult, Timestamp,
    },
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDto {
    pub agent: Agent,
    pub id: String,
    pub r#ref: String,
    pub source: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    pub parent_ref: Option<String>,
    pub archived: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHitDto {
    #[serde(flatten)]
    pub session: SessionDto,
    pub score: u64,
    pub matches: u64,
    pub by_kind: BTreeMap<String, u64>,
    pub snippets: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchDataDto {
    pub query: String,
    pub agents: Vec<Agent>,
    pub sort: Sort,
    pub count: usize,
    pub results: Vec<SearchHitDto>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadDataDto {
    #[serde(flatten)]
    pub session: SessionDto,
    pub mode: ReadMode,
    pub body: String,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub total_chars: usize,
    pub returned_chars: usize,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpanDto {
    pub start: String,
    pub end: String,
    pub minutes: f64,
    pub prompts: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineSessionDto {
    #[serde(flatten)]
    pub session: SessionDto,
    pub spans: Vec<SpanDto>,
    pub active_minutes: f64,
    pub start: Option<String>,
    pub end: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineDataDto {
    pub sessions: Vec<TimelineSessionDto>,
    pub daily_active_minutes: BTreeMap<String, f64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntryDto {
    pub index: u64,
    pub source_id: String,
    pub timestamp: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub role: String,
    pub kind: Kind,
    pub subtype: Option<String>,
    pub tool_name: Option<String>,
    pub automated: bool,
    pub text: String,
}

impl SessionKey {
    pub fn reference(&self) -> String {
        format!("{}:{}", self.agent, self.id)
    }
}

impl Agent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Devin => "devin",
            Self::Droid => "droid",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
        }
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Tool => "tool",
            Self::Meta => "meta",
        }
    }
}

impl std::str::FromStr for Agent {
    type Err = crate::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "cursor" => Ok(Self::Cursor),
            "devin" => Ok(Self::Devin),
            "droid" => Ok(Self::Droid),
            "opencode" => Ok(Self::Opencode),
            "pi" => Ok(Self::Pi),
            _ => Err(crate::Error::InvalidArgument("unknown agent".into())),
        }
    }
}

impl Coverage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }
}

impl std::fmt::Display for Coverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

impl std::str::FromStr for Coverage {
    type Err = crate::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "compact" => Ok(Self::Compact),
            "full" => Ok(Self::Full),
            _ => Err(crate::Error::InvalidArgument("coverage is invalid".into())),
        }
    }
}

impl std::str::FromStr for SessionKey {
    type Err = crate::Error;
    fn from_str(value: &str) -> Result<Self> {
        let (agent, id) = value
            .split_once(':')
            .filter(|(_, id)| !id.is_empty())
            .ok_or_else(|| crate::Error::InvalidArgument("expected agent:id".into()))?;
        Ok(Self {
            agent: agent.parse()?,
            id: id.into(),
        })
    }
}

pub fn format_timestamp(value: Timestamp) -> Result<String> {
    Ok(jiff::Timestamp::from_millisecond(value)?.to_string())
}

impl TryFrom<&Session> for SessionDto {
    type Error = crate::Error;
    fn try_from(session: &Session) -> Result<Self> {
        Ok(Self {
            agent: session.key.agent,
            id: session.key.id.clone(),
            r#ref: session.key.reference(),
            source: session.source.clone(),
            title: session.title.clone(),
            cwd: session
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            branch: session.branch.clone(),
            first_ts: session.started_at.map(format_timestamp).transpose()?,
            last_ts: session.updated_at.map(format_timestamp).transpose()?,
            parent_ref: session.parent.as_ref().map(SessionKey::reference),
            archived: session.archived,
        })
    }
}

impl TryFrom<&Entry> for EntryDto {
    type Error = crate::Error;
    fn try_from(entry: &Entry) -> Result<Self> {
        let timestamp = match &entry.timestamp_raw {
            Some(raw) => Some(raw.clone()),
            None => entry.timestamp.map(format_timestamp).transpose()?,
        };
        Ok(Self {
            index: entry.index,
            source_id: entry.source_id.clone(),
            timestamp,
            timestamp_ms: entry.timestamp,
            role: entry.role.clone(),
            kind: entry.kind,
            subtype: entry.subtype.clone(),
            tool_name: entry.tool_name.clone(),
            automated: entry.automated,
            text: entry.text.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionsDataDto {
    pub count: usize,
    pub sessions: Vec<SessionDto>,
}

impl TryFrom<&SearchResult> for SearchDataDto {
    type Error = crate::Error;
    fn try_from(result: &SearchResult) -> Result<Self> {
        Ok(Self {
            query: result.query.clone(),
            agents: result.agents.clone(),
            sort: result.sort,
            count: result.count,
            results: result
                .results
                .iter()
                .map(|hit| {
                    Ok(SearchHitDto {
                        session: SessionDto::try_from(&hit.session)?,
                        score: hit.score,
                        matches: hit.matches,
                        by_kind: hit.by_kind.clone(),
                        snippets: hit.snippets.clone(),
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

impl TryFrom<&ReadPage> for ReadDataDto {
    type Error = crate::Error;
    fn try_from(page: &ReadPage) -> Result<Self> {
        Ok(Self {
            session: SessionDto::try_from(&page.session)?,
            mode: page.mode,
            body: page.text.clone(),
            offset: page.offset,
            next_offset: page.next_offset,
            total_chars: page.total_chars,
            returned_chars: page.returned_chars,
            truncated: page.truncated,
        })
    }
}

impl TryFrom<&TimelineResult> for TimelineDataDto {
    type Error = crate::Error;
    fn try_from(result: &TimelineResult) -> Result<Self> {
        Ok(Self {
            sessions: result
                .sessions
                .iter()
                .map(|session| {
                    Ok(TimelineSessionDto {
                        session: SessionDto::try_from(&session.session)?,
                        spans: session
                            .spans
                            .iter()
                            .map(|span| {
                                Ok(SpanDto {
                                    start: format_timestamp(span.start)?,
                                    end: format_timestamp(span.end)?,
                                    minutes: minutes(span.end.abs_diff(span.start)),
                                    prompts: span.human_prompts,
                                })
                            })
                            .collect::<Result<_>>()?,
                        active_minutes: minutes(session.active_ms),
                        start: session
                            .spans
                            .first()
                            .map(|span| format_timestamp(span.start))
                            .transpose()?,
                        end: session
                            .spans
                            .last()
                            .map(|span| format_timestamp(span.end))
                            .transpose()?,
                    })
                })
                .collect::<Result<_>>()?,
            daily_active_minutes: result
                .daily_active_ms
                .iter()
                .map(|(day, time)| (day.clone(), minutes(*time)))
                .collect(),
        })
    }
}

impl TryFrom<&[Session]> for SessionsDataDto {
    type Error = crate::Error;
    fn try_from(sessions: &[Session]) -> Result<Self> {
        Ok(Self {
            count: sessions.len(),
            sessions: sessions
                .iter()
                .map(SessionDto::try_from)
                .collect::<Result<_>>()?,
        })
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "The JSON protocol represents elapsed minutes as floating point"
)]
const fn minutes(milliseconds: u64) -> f64 {
    milliseconds as f64 / 60_000.0
}
