use super::{
    Source,
    jsonl::{diagnostic, flatten, string},
    sqlite,
};
use crate::{
    Agent, Context, Discovery, DiscoveryCache, Entry, Kind, ParsedSession, RawSession, Result,
    Session, SessionKey, TitleOrigin,
};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Opencode {
    staging: Option<PathBuf>,
}

impl Opencode {
    pub const fn new(_: &Context) -> Self {
        Self { staging: None }
    }

    pub(crate) fn with_staging(staging: &Path) -> Self {
        Self {
            staging: Some(staging.into()),
        }
    }
}

fn columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>> {
    Ok(connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get(1))?
        .collect::<std::result::Result<_, _>>()?)
}

fn session_query(connection: &Connection) -> Result<String> {
    let columns = columns(connection, "session")?;
    let parent = if columns.contains("parent_id") {
        "parent_id"
    } else {
        "NULL"
    };
    let archived = if columns.contains("time_archived") {
        "time_archived"
    } else {
        "NULL"
    };
    let directory = if columns.contains("project_id")
        && !self::columns(connection, "project")?.is_empty()
    {
        "COALESCE(NULLIF(directory, ''), (SELECT worktree FROM project WHERE project.id = session.project_id))"
    } else {
        "directory"
    };
    Ok(format!(
        "SELECT id, {directory}, title, time_created, time_updated, {parent}, {archived} FROM session"
    ))
}

const PARTS: &str = "SELECT m.id, p.id, m.data, p.data, m.time_created, p.time_created
    FROM message AS m JOIN part AS p ON p.message_id=m.id
    WHERE m.session_id=? ORDER BY m.time_created, p.time_created, p.id";

fn discovery(
    ctx: &Context,
    previous: &DiscoveryCache,
    staging: Option<&Path>,
) -> Result<Vec<RawSession>> {
    let path = &ctx.roots.opencode_db;
    let dependencies = sqlite::dependencies(path);
    let (snapshot, captured) = sqlite::snapshot_at(path, staging)?;
    let connection = sqlite::open(&snapshot)?;
    let transaction = connection.unchecked_transaction()?;
    let query = session_query(&transaction)?;
    transaction.prepare(PARTS)?;
    let mut statement = transaction.prepare(&query)?;
    let mut rows = statement.query([])?;
    let mut sessions = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if id.is_empty() {
            return Err(crate::Error::InvalidArgument(
                "OpenCode session has no identity".into(),
            ));
        }
        sessions.push(RawSession {
            key: SessionKey {
                agent: Agent::Opencode,
                id: id.clone(),
            },
            source: format!("{}#session={id}", path.to_string_lossy()),
            dependencies: dependencies.clone(),
            fingerprint: captured.clone(),
            snapshot: if previous
                .by_source
                .get(&format!("{}#session={id}", path.to_string_lossy()))
                .is_some_and(|cached| {
                    cached.key.agent == Agent::Opencode
                        && cached.key.id == id
                        && cached.dependencies == dependencies
                        && cached.fingerprint == captured
                }) {
                None
            } else {
                Some(Arc::clone(&snapshot))
            },
        });
    }
    Ok(sessions)
}

impl Source for Opencode {
    fn agent(&self) -> Agent {
        Agent::Opencode
    }

    fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
        discovery(ctx, previous, self.staging.as_deref()).map_or_else(|_| {
            let legacy =
                !ctx.roots.opencode_db.exists() && ctx.roots.opencode_legacy_storage.exists();
            Ok(Discovery {
                sessions: Vec::new(),
                complete_agents: BTreeSet::new(),
                diagnostics: vec![diagnostic(
                    Agent::Opencode,
                    &ctx.roots.opencode_db,
                    if legacy {
                        "unsupported_legacy_store"
                    } else {
                        "source_unavailable"
                    },
                    if legacy {
                        "Legacy OpenCode storage is unsupported"
                    } else {
                        "OpenCode database could not be read consistently or has an unsupported schema"
                    },
                )],
            })
        }, |sessions| Ok(Discovery {
            sessions,
            diagnostics: Vec::new(),
            complete_agents: BTreeSet::from([Agent::Opencode]),
        }))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered parsing states together"
    )]
    fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession> {
        let snapshot = match &raw.snapshot {
            Some(snapshot) => Arc::clone(snapshot),
            None => {
                sqlite::snapshot_at(
                    raw.dependencies.first().ok_or_else(|| {
                        crate::Error::InvalidArgument(
                            "OpenCode source has no database dependency".into(),
                        )
                    })?,
                    self.staging.as_deref(),
                )?
                .0
            }
        };
        let connection = sqlite::open(&snapshot)?;
        let transaction = connection.unchecked_transaction()?;
        let query = format!("{} WHERE id=?", session_query(&transaction)?);
        let mut session = transaction.query_row(&query, [&raw.key.id], |row| {
            let title: Option<String> = row.get(2)?;
            let title = title.filter(|s| !s.trim().is_empty());
            Ok(Session {
                key: SessionKey {
                    agent: Agent::Opencode,
                    id: row.get(0)?,
                },
                title_origin: title.as_ref().map(|_| TitleOrigin::Native),
                title,
                cwd: row
                    .get::<_, Option<String>>(1)?
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from),
                branch: None,
                started_at: row.get(3)?,
                updated_at: row.get(4)?,
                source_updated_at: row.get(4)?,
                parent: row
                    .get::<_, Option<String>>(5)?
                    .filter(|s| !s.is_empty())
                    .map(|id| SessionKey {
                        agent: Agent::Opencode,
                        id,
                    }),
                archived: row.get::<_, Option<i64>>(6)?.is_some(),
                source: raw.source.clone(),
            })
        })?;
        let mut statement = transaction.prepare(PARTS)?;
        let mut rows = statement.query([&raw.key.id])?;
        let mut index = 0;
        let mut malformed = 0;
        while let Some(row) = rows.next()? {
            let message: String = row.get(2)?;
            let part: String = row.get(3)?;
            let values = (
                serde_json::from_str::<Value>(&message),
                serde_json::from_str::<Value>(&part),
            );
            let (message, part) = match values {
                (Ok(message), Ok(part)) if message.is_object() && part.is_object() => {
                    (message, part)
                }
                _ => {
                    malformed += 1;
                    continue;
                }
            };
            let subtype = part["type"].as_str().unwrap_or("part");
            let (kind, text) = match subtype {
                "text" => (Kind::Message, string(&part, "text").unwrap_or_default()),
                "reasoning" => (Kind::Meta, string(&part, "text").unwrap_or_default()),
                "tool" => (
                    Kind::Tool,
                    [
                        string(&part, "tool").unwrap_or_default(),
                        flatten(&part["state"]),
                    ]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
                ),
                _ => (Kind::Tool, flatten(&part)),
            };
            if text.trim().is_empty() {
                continue;
            }
            let role = string(&message, "role").unwrap_or_else(|| "message".into());
            let timestamp = row
                .get::<_, Option<i64>>(5)?
                .filter(|ts| *ts != 0)
                .or(row.get::<_, Option<i64>>(4)?);
            super::derive_title(&mut session, &role, kind, &text);
            let entry = Entry {
                session: session.key.clone(),
                index,
                source_id: format!("{}:{}", row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                timestamp,
                timestamp_raw: None,
                role,
                kind,
                subtype: Some(subtype.into()),
                tool_name: string(&part, "tool"),
                automated: session.parent.is_some()
                    || message["automated"].as_bool().unwrap_or(false),
                text,
            };
            index += 1;
            emit(entry)?;
        }
        let diagnostics = if malformed == 0 {
            Vec::new()
        } else {
            vec![diagnostic(
                Agent::Opencode,
                Path::new(&raw.source),
                "malformed_records",
                "OpenCode contains malformed message or part records",
            )]
        };
        Ok(ParsedSession {
            session,
            complete: diagnostics.is_empty(),
            diagnostics,
        })
    }
}
