use super::{
    Source,
    jsonl::{diagnostic, flatten, string},
    sqlite,
};
use crate::{
    Agent, Context, Discovery, DiscoveryCache, Entry, Fingerprint, Kind, ParsedSession, RawSession,
    Result, Session, SessionKey, SourceState, Timestamp, TitleOrigin,
};
use rusqlite::{Connection, Row, types::ValueRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

fn subagent_heads(connection: &Connection) -> Result<Vec<(String, String, Value)>> {
    let heads = columns(connection, "subagent_heads")?;
    if !["session_id", "agent_id", "chain_node_id"]
        .iter()
        .all(|name| heads.contains(*name))
    {
        return Ok(Vec::new());
    }
    Ok(connection
        .prepare(&format!(
            "SELECT session_id, agent_id, chain_node_id, {} FROM subagent_heads ORDER BY session_id, agent_id",
            optional(&heads, "updated_at"),
        ))?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                json!([key_value(row.get_ref(2)?), key_value(row.get_ref(3)?)]),
            ))
        })?
        .collect::<std::result::Result<_, _>>()?)
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"))
                .expect("writing to a String cannot fail");
            hex
        })
}

fn key_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => number.into(),
        ValueRef::Real(number) => {
            serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
        }
        ValueRef::Text(text) => String::from_utf8_lossy(text).into(),
        ValueRef::Blob(blob) => json!({ "blob": sha256_hex(blob) }),
    }
}

fn aggregates(connection: &Connection) -> Result<BTreeMap<String, Value>> {
    let mut statement = connection.prepare(
        "SELECT session_id, count(*), max(node_id), max(rowid) FROM message_nodes GROUP BY session_id",
    )?;
    let mut rows = statement.query([])?;
    let mut found = BTreeMap::new();
    while let Some(row) = rows.next()? {
        if let ValueRef::Text(session) = row.get_ref(0)? {
            found.insert(
                String::from_utf8_lossy(session).into_owned(),
                json!([
                    key_value(row.get_ref(1)?),
                    key_value(row.get_ref(2)?),
                    key_value(row.get_ref(3)?),
                ]),
            );
        }
    }
    Ok(found)
}

#[derive(Default)]
struct Content {
    front: Sha256,
    nodes: Sha256,
}

fn hash_row(hasher: &mut Sha256, tag: u8, row: &Row<'_>, count: usize) -> Result<()> {
    hasher.update([tag]);
    for column in 0..count {
        match row.get_ref(column)? {
            ValueRef::Null => hasher.update([0]),
            ValueRef::Integer(number) => {
                hasher.update([1]);
                hasher.update(number.to_le_bytes());
            }
            ValueRef::Real(number) => {
                hasher.update([2]);
                hasher.update(number.to_bits().to_le_bytes());
            }
            ValueRef::Text(bytes) => hash_bytes(hasher, 3, bytes),
            ValueRef::Blob(bytes) => hash_bytes(hasher, 4, bytes),
        }
    }
    Ok(())
}

fn hash_bytes(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    hasher.update([tag]);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

impl Content {
    fn row(&mut self, tag: u8, row: &Row<'_>) -> Result<()> {
        let count = row.as_ref().column_count();
        hash_row(&mut self.front, tag, row, count)
    }

    fn finish(self) -> SourceState {
        let nodes = self.nodes.clone().finalize().to_vec();
        self.finish_with(&nodes)
    }

    fn finish_with(mut self, nodes: &[u8]) -> SourceState {
        self.front.update(nodes);
        SourceState::Content {
            sha256: sha256_hex(self.front.finalize()),
        }
    }
}

fn split(id: &str) -> (&str, Option<&str>) {
    id.split_once('/')
        .map_or((id, None), |(parent, agent)| (parent, Some(agent)))
}

fn node_select(connection: &Connection) -> Result<String> {
    let node_columns = columns(connection, "message_nodes")?;
    Ok(format!(
        "SELECT node_id, parent_node_id, chat_message, {}, {}, session_id FROM message_nodes",
        optional(&node_columns, "metadata"),
        optional(&node_columns, "created_at"),
    ))
}

fn node_digests(connection: &Connection) -> Result<BTreeMap<String, Vec<u8>>> {
    let query = format!("{} ORDER BY session_id, node_id", node_select(connection)?);
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([])?;
    let mut digests = BTreeMap::new();
    let mut current: Option<(String, Sha256)> = None;
    while let Some(row) = rows.next()? {
        let Some(session) = row.get::<_, Option<String>>(5)? else {
            continue;
        };
        if current.as_ref().is_none_or(|(id, _)| *id != session) {
            if let Some((id, hasher)) = current.take() {
                digests.insert(id, hasher.finalize().to_vec());
            }
            current = Some((session, Sha256::default()));
        }
        if let Some((_, hasher)) = current.as_mut() {
            hash_row(hasher, b'n', row, row.as_ref().column_count() - 1)?;
        }
    }
    if let Some((id, hasher)) = current {
        digests.insert(id, hasher.finalize().to_vec());
    }
    Ok(digests)
}

fn content(
    connection: &Connection,
    id: &str,
    digests: &BTreeMap<String, Vec<u8>>,
) -> Result<SourceState> {
    let (parent, agent) = split(id);
    let mut content = Content::default();
    let header = load_header(connection, parent, &mut content)?;
    chain_head(connection, parent, agent, &header, &mut content)?;
    let nodes = digests
        .get(parent)
        .cloned()
        .unwrap_or_else(|| Sha256::default().finalize().to_vec());
    Ok(content.finish_with(&nodes))
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
    let (snapshot, _) = sqlite::snapshot_at(path, staging)?;
    let connection = sqlite::open(&snapshot)?;
    let transaction = connection.unchecked_transaction()?;
    let query = session_query(&transaction)?;
    let rows = transaction
        .prepare(&query)?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (1..=5)
                    .map(|column| row.get_ref(column).map(key_value))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            ))
        })?
        .collect::<std::result::Result<Vec<(String, Vec<Value>)>, _>>()?;
    let mut by_id = BTreeMap::new();
    for (id, row) in &rows {
        by_id.entry(id.as_str()).or_insert(row);
    }
    let nodes = aggregates(&transaction)?;
    let digests = if previous.verify {
        node_digests(&transaction)?
    } else {
        BTreeMap::new()
    };
    let aggregate = |id: &str| nodes.get(id).cloned().unwrap_or(Value::Null);
    let parents = rows
        .iter()
        .map(|(id, row)| (id.clone(), json!(["session", row, aggregate(id)])));
    let children = subagent_heads(&transaction)?
        .into_iter()
        .filter_map(|(session, agent, head)| {
            let row = by_id.get(session.as_str())?;
            Some((
                format!("{session}/{agent}"),
                json!(["subagent", row, aggregate(&session), head]),
            ))
        })
        .collect::<Vec<_>>();
    let mut sessions = Vec::new();
    for (id, key) in parents.chain(children) {
        if id.is_empty() {
            return Err(crate::Error::InvalidArgument(
                "Devin session has no identity".into(),
            ));
        }
        let source = source_name(path, &id);
        let fingerprint = Fingerprint {
            signature: sha256_hex(serde_json::to_string(&key)?),
        };
        let cached = previous.by_source.get(&source).filter(|cached| {
            cached.key.agent == Agent::Devin
                && cached.key.id == id
                && cached.dependencies == dependencies
                && cached.fingerprint == fingerprint
        });
        let reusable = cached.is_some_and(|cached| {
            !previous.verify
                || cached.state.as_ref().is_some_and(|state| {
                    content(&transaction, &id, &digests).is_ok_and(|current| current == *state)
                })
        });
        sessions.push(RawSession {
            key: SessionKey {
                agent: Agent::Devin,
                id,
            },
            source,
            dependencies: dependencies.clone(),
            fingerprint,
            snapshot: (!reusable).then(|| Arc::clone(&snapshot)),
            resume: None,
        });
    }
    Ok(sessions)
}

fn seconds(value: Option<i64>) -> Option<Timestamp> {
    value.filter(|s| *s > 0).map(|s| s.saturating_mul(1000))
}

fn node_rows(
    connection: &Connection,
    session: &str,
    content: &mut Content,
    mut visit: impl FnMut(&Row<'_>) -> Result<()>,
) -> Result<()> {
    let query = format!(
        "{} WHERE session_id=? ORDER BY node_id",
        node_select(connection)?
    );
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([session])?;
    while let Some(row) = rows.next()? {
        hash_row(
            &mut content.nodes,
            b'n',
            row,
            row.as_ref().column_count() - 1,
        )?;
        visit(row)?;
    }
    Ok(())
}

fn load_nodes(
    connection: &Connection,
    session: &str,
    content: &mut Content,
) -> Result<(BTreeMap<i64, Node>, u64)> {
    let mut nodes = BTreeMap::new();
    let mut malformed = 0;
    node_rows(connection, session, content, |row| {
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
        Ok(())
    })?;
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

fn load_header(connection: &Connection, id: &str, content: &mut Content) -> Result<Header> {
    let query = format!("{} WHERE id=?", session_query(connection)?);
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([id])?;
    let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    content.row(b'h', row)?;
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
}

fn chain_head(
    connection: &Connection,
    session: &str,
    agent: Option<&str>,
    header: &Header,
    content: &mut Content,
) -> Result<(Option<i64>, Option<Timestamp>)> {
    let Some(agent) = agent else {
        return Ok((header.main_chain_id, seconds(header.last_activity_at)));
    };
    let heads = columns(connection, "subagent_heads")?;
    let query = format!(
        "SELECT chain_node_id, {} FROM subagent_heads WHERE session_id=? AND agent_id=?",
        optional(&heads, "updated_at"),
    );
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([session, agent])?;
    let row = rows
        .next()?
        .ok_or_else(|| crate::Error::InvalidArgument("Devin subagent head is missing".into()))?;
    content.row(b's', row)?;
    let (head, updated): (i64, Option<i64>) = (row.get(0)?, row.get(1)?);
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
        let (parent_id, agent_id) = split(&raw.key.id);
        let mut content = Content::default();
        let header = load_header(&transaction, parent_id, &mut content)?;
        let (head, freshness) =
            chain_head(&transaction, parent_id, agent_id, &header, &mut content)?;
        let (nodes, mut malformed) = load_nodes(&transaction, parent_id, &mut content)?;
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
            state: Some(content.finish()),
            recheck: false,
        })
    }
}
