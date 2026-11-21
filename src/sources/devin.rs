use super::{
    Source,
    jsonl::{diagnostic, flatten, string},
    sqlite,
};
use crate::{
    Agent, Context, Discovery, DiscoveryCache, Entry, Kind, ParsedSession, RawSession, Result,
    Session, SessionKey, Timestamp, TitleOrigin,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Devin {
    staging: Option<PathBuf>,
}

impl Devin {
    fn snapshot_for(&self, raw: &RawSession) -> Result<Arc<crate::PrivateSnapshot>> {
        Ok(match &raw.snapshot {
            Some(snapshot) => Arc::clone(snapshot),
            None => {
                sqlite::snapshot_at(
                    raw.dependencies.first().ok_or_else(|| {
                        crate::Error::InvalidArgument(
                            "Devin source has no database dependency".into(),
                        )
                    })?,
                    self.staging.as_deref(),
                )?
                .0
            }
        })
    }

    pub const fn new(_: &Context) -> Self {
        Self { staging: None }
    }

    pub(crate) fn with_staging(staging: &Path) -> Self {
        Self {
            staging: Some(staging.into()),
        }
    }
}

struct Node {
    parent: Option<i64>,
    message: Option<Value>,
    metadata: Value,
    created_at: Option<i64>,
}

struct Header {
    title: Option<String>,
    cwd: Option<PathBuf>,
    created_at: Option<i64>,
    last_activity_at: Option<i64>,
    main_chain_id: Option<i64>,
}

fn columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>> {
    Ok(connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get(1))?
        .collect::<std::result::Result<_, _>>()?)
}

fn optional(columns: &BTreeSet<String>, name: &str) -> String {
    if columns.contains(name) {
        name.into()
    } else {
        "NULL".into()
    }
}

fn require(columns: &BTreeSet<String>, names: &[&str]) -> Result<()> {
    if names.iter().all(|name| columns.contains(*name)) {
        Ok(())
    } else {
        Err(crate::Error::InvalidArgument(
            "Devin sessions database has an unsupported schema".into(),
        ))
    }
}

fn session_query(connection: &Connection) -> Result<String> {
    let session = columns(connection, "sessions")?;
    require(&session, &["id", "main_chain_id"])?;
    require(
        &columns(connection, "message_nodes")?,
        &["session_id", "node_id", "parent_node_id", "chat_message"],
    )?;
    Ok(format!(
        "SELECT id, {}, {}, {}, {}, main_chain_id FROM sessions",
        optional(&session, "title"),
        optional(&session, "working_directory"),
        optional(&session, "created_at"),
        optional(&session, "last_activity_at"),
    ))
}

fn subagent_heads(connection: &Connection) -> Result<Vec<(String, String)>> {
    let heads = columns(connection, "subagent_heads")?;
    if !["session_id", "agent_id", "chain_node_id"]
        .iter()
        .all(|name| heads.contains(*name))
    {
        return Ok(Vec::new());
    }
    Ok(connection
        .prepare("SELECT session_id, agent_id FROM subagent_heads ORDER BY session_id, agent_id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?)
}

fn source_name(path: &Path, id: &str) -> String {
    format!("{}#session={id}", path.to_string_lossy())
}

fn discovery(
    ctx: &Context,
    previous: &DiscoveryCache,
    staging: Option<&Path>,
) -> Result<Vec<RawSession>> {
    let path = &ctx.roots.devin_sessions_db;
    let dependencies = sqlite::dependencies(path);
    let (snapshot, captured) = sqlite::snapshot_at(path, staging)?;
    let connection = sqlite::open(&snapshot)?;
    let transaction = connection.unchecked_transaction()?;
    let query = session_query(&transaction)?;
    let ids = transaction
        .prepare(&query)?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let children = subagent_heads(&transaction)?
        .into_iter()
        .filter(|(session, _)| ids.contains(session))
        .map(|(session, agent)| format!("{session}/{agent}"));
    let mut sessions = Vec::new();
    for id in ids.iter().cloned().chain(children) {
        if id.is_empty() {
            return Err(crate::Error::InvalidArgument(
                "Devin session has no identity".into(),
            ));
        }
        let source = source_name(path, &id);
        let reusable = previous.by_source.get(&source).is_some_and(|cached| {
            cached.key.agent == Agent::Devin
                && cached.key.id == id
                && cached.dependencies == dependencies
                && cached.fingerprint == captured
        });
        sessions.push(RawSession {
            key: SessionKey {
                agent: Agent::Devin,
                id,
            },
            source,
            dependencies: dependencies.clone(),
            fingerprint: captured.clone(),
            snapshot: (!reusable).then(|| Arc::clone(&snapshot)),
        });
    }
    Ok(sessions)
}

fn seconds(value: Option<i64>) -> Option<Timestamp> {
    value.filter(|s| *s > 0).map(|s| s.saturating_mul(1000))
}

fn load_nodes(connection: &Connection, session: &str) -> Result<(BTreeMap<i64, Node>, u64)> {
    let node_columns = columns(connection, "message_nodes")?;
    let query = format!(
        "SELECT node_id, parent_node_id, chat_message, {}, {} FROM message_nodes WHERE session_id=? ORDER BY node_id",
        optional(&node_columns, "metadata"),
        optional(&node_columns, "created_at"),
    );
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([session])?;
    let mut nodes = BTreeMap::new();
    let mut malformed = 0;
    while let Some(row) = rows.next()? {
        let message = row
            .get::<_, Option<String>>(2)?
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object);
        if message.is_none() {
            malformed += 1;
        }
        let metadata = row
            .get::<_, Option<String>>(3)?
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        nodes.insert(
            row.get(0)?,
            Node {
                parent: row.get(1)?,
                message,
                metadata,
                created_at: row.get(4)?,
            },
        );
    }
    Ok((nodes, malformed))
}

fn chain(nodes: &BTreeMap<i64, Node>, head: i64) -> (Vec<i64>, bool) {
    let mut ids = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = Some(head);
    while let Some(id) = current {
        if !seen.insert(id) {
            return (ids.into_iter().rev().collect(), false);
        }
        let Some(node) = nodes.get(&id) else {
            return (ids.into_iter().rev().collect(), false);
        };
        ids.push(id);
        current = node.parent;
    }
    (ids.into_iter().rev().collect(), true)
}

fn telemetry_source(message: &Value) -> &str {
    message["metadata"]["telemetry"]["source"]
        .as_str()
        .unwrap_or("")
}

fn summarized_from(node: &Node) -> Option<i64> {
    node.metadata["summarized_from"].as_i64()
}

fn prior_ids(node: &Node) -> Vec<i64> {
    node.metadata["extensions"]["compact/prior_node_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

// Compaction moves earlier turns off the live chain; `summarized_from` points at their head.
fn expand(
    nodes: &BTreeMap<i64, Node>,
    head: i64,
    expanded: &mut BTreeSet<i64>,
    order: &mut Vec<i64>,
) -> bool {
    let (ids, intact) = chain(nodes, head);
    let mut complete = intact;
    for id in ids {
        if let Some(previous) = summarized_from(&nodes[&id]).filter(|_| expanded.insert(id)) {
            complete &= expand(nodes, previous, expanded, order);
        }
        order.push(id);
    }
    complete
}

fn timestamp(message: &Value, node: &Node) -> (Option<Timestamp>, Option<String>) {
    let raw = string(&message["metadata"], "created_at");
    let parsed = raw
        .as_deref()
        .and_then(|text| text.parse::<jiff::Timestamp>().ok())
        .map(jiff::Timestamp::as_millisecond);
    (parsed.or_else(|| seconds(node.created_at)), raw)
}

struct Item {
    role: &'static str,
    kind: Kind,
    subtype: Option<&'static str>,
    tool_name: Option<String>,
    automated: bool,
    text: String,
}

const fn item(role: &'static str, kind: Kind, subtype: Option<&'static str>, text: String) -> Item {
    Item {
        role,
        kind,
        subtype,
        tool_name: None,
        automated: false,
        text,
    }
}

fn items(message: &Value) -> Vec<Item> {
    let content = flatten(&message["content"]);
    match message["role"].as_str().unwrap_or("") {
        "system" => match telemetry_source(message) {
            "compaction" => vec![Item {
                automated: true,
                ..item("meta", Kind::Meta, Some("compaction"), content)
            }],
            "response_interrupted" => vec![Item {
                automated: true,
                ..item("meta", Kind::Meta, Some("interrupted"), content)
            }],
            _ => Vec::new(),
        },
        "user" => vec![Item {
            automated: !(telemetry_source(message) == "user"
                && message["metadata"]["is_user_input"].as_bool() == Some(true)),
            ..item("user", Kind::Message, None, content)
        }],
        "assistant" => {
            let reasoning = flatten(&message["thinking"]["thinking"]);
            let calls = message["tool_calls"]
                .as_array()
                .map_or(&[][..], Vec::as_slice);
            [
                item("assistant", Kind::Meta, Some("reasoning"), reasoning),
                item("assistant", Kind::Message, None, content),
            ]
            .into_iter()
            .chain(calls.iter().map(|call| Item {
                tool_name: string(call, "name"),
                ..item(
                    "assistant",
                    Kind::Tool,
                    Some("tool_call"),
                    flatten(&serde_json::json!({
                        "name": call["name"],
                        "arguments": call["arguments"],
                    })),
                )
            }))
            .collect()
        }
        "tool" => vec![item("tool", Kind::Tool, Some("tool_result"), content)],
        _ => Vec::new(),
    }
}

fn load_header(connection: &Connection, id: &str) -> Result<Header> {
    let query = format!("{} WHERE id=?", session_query(connection)?);
    Ok(connection.query_row(&query, [id], |row| {
        Ok(Header {
            title: row
                .get::<_, Option<String>>(1)?
                .filter(|s| !s.trim().is_empty()),
            cwd: row
                .get::<_, Option<String>>(2)?
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            created_at: row.get(3)?,
            last_activity_at: row.get(4)?,
            main_chain_id: row.get(5)?,
        })
    })?)
}

fn chain_head(
    connection: &Connection,
    session: &str,
    agent: Option<&str>,
    header: &Header,
) -> Result<(Option<i64>, Option<Timestamp>)> {
    let Some(agent) = agent else {
        return Ok((header.main_chain_id, seconds(header.last_activity_at)));
    };
    let (head, updated): (i64, Option<i64>) = connection
        .query_row(
            "SELECT chain_node_id, updated_at FROM subagent_heads WHERE session_id=? AND agent_id=?",
            [session, agent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| crate::Error::InvalidArgument("Devin subagent head is missing".into()))?;
    Ok((
        Some(head),
        seconds(updated).or_else(|| seconds(header.last_activity_at)),
    ))
}

impl Source for Devin {
    fn agent(&self) -> Agent {
        Agent::Devin
    }

    fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
        discovery(ctx, previous, self.staging.as_deref()).map_or_else(
            |_| {
                Ok(Discovery {
                    sessions: Vec::new(),
                    complete_agents: BTreeSet::new(),
                    diagnostics: vec![diagnostic(
                        Agent::Devin,
                        &ctx.roots.devin_sessions_db,
                        "source_unavailable",
                        "Devin sessions database could not be read consistently or has an unsupported schema",
                    )],
                })
            },
            |sessions| {
                Ok(Discovery {
                    sessions,
                    diagnostics: Vec::new(),
                    complete_agents: BTreeSet::from([Agent::Devin]),
                })
            },
        )
    }

    fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession> {
        let snapshot = self.snapshot_for(raw)?;
        let connection = sqlite::open(&snapshot)?;
        let transaction = connection.unchecked_transaction()?;
        let (parent_id, agent_id) = raw
            .key
            .id
            .split_once('/')
            .map_or((raw.key.id.as_str(), None), |(parent, agent)| {
                (parent, Some(agent))
            });
        let header = load_header(&transaction, parent_id)?;
        let (head, freshness) = chain_head(&transaction, parent_id, agent_id, &header)?;
        let (nodes, mut malformed) = load_nodes(&transaction, parent_id)?;
        let mut order = Vec::new();
        let complete_chain =
            head.is_none_or(|head| expand(&nodes, head, &mut BTreeSet::new(), &mut order));
        let mut session = Session {
            key: raw.key.clone(),
            title_origin: header
                .title
                .as_ref()
                .filter(|_| agent_id.is_none())
                .map(|_| TitleOrigin::Native),
            title: header.title.filter(|_| agent_id.is_none()),
            cwd: header.cwd,
            branch: None,
            started_at: seconds(header.created_at),
            updated_at: freshness,
            source_updated_at: freshness,
            parent: agent_id.map(|_| SessionKey {
                agent: Agent::Devin,
                id: parent_id.into(),
            }),
            archived: false,
            source: raw.source.clone(),
        };
        let mut emitted = BTreeSet::new();
        let mut index = 0;
        let mut first = None;
        for id in order {
            let node = &nodes[&id];
            if prior_ids(node).iter().any(|prior| emitted.contains(prior)) || !emitted.insert(id) {
                continue;
            }
            let Some(message) = &node.message else {
                continue;
            };
            let (timestamp, timestamp_raw) = timestamp(message, node);
            for (ordinal, item) in items(message).into_iter().enumerate() {
                if item.text.trim().is_empty() {
                    continue;
                }
                super::derive_title(&mut session, item.role, item.kind, &item.text);
                first = first.or(timestamp);
                emit(Entry {
                    session: session.key.clone(),
                    index,
                    source_id: format!("node:{id}:{ordinal}"),
                    timestamp,
                    timestamp_raw: timestamp_raw.clone(),
                    role: item.role.into(),
                    kind: item.kind,
                    subtype: item.subtype.map(Into::into),
                    tool_name: item.tool_name,
                    automated: item.automated || session.parent.is_some(),
                    text: item.text,
                })?;
                index += 1;
            }
        }
        if session.parent.is_some() {
            session.started_at = first.or(session.started_at);
        }
        if !complete_chain {
            malformed += 1;
        }
        let diagnostics = if malformed == 0 {
            Vec::new()
        } else {
            vec![diagnostic(
                Agent::Devin,
                Path::new(&raw.source),
                "malformed_records",
                "Devin session contains malformed or unlinked message nodes",
            )]
        };
        Ok(ParsedSession {
            session,
            complete: diagnostics.is_empty(),
            diagnostics,
        })
    }
}
