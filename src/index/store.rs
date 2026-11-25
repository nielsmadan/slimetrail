use super::{
    IndexSession,
    coverage::{coverage_name, parse_searchable, searchable_name, searchable_start},
};
use crate::{
    Agent, CachedSource, Coverage, Diagnostic, DiscoveryCache, Entry, Error, Kind, RawSession,
    Result, Searchable, Session, SessionKey, SourceState, TitleOrigin,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::{BTreeMap, BTreeSet};

pub struct Document {
    pub session: SessionKey,
    pub role: Option<String>,
    pub entry_index: Option<u64>,
    pub kind: String,
    pub text: String,
}

pub(crate) struct Event {
    pub timestamp: Option<i64>,
    pub human_prompt: bool,
}

fn document_sql(candidate: bool) -> String {
    let candidates = if candidate {
        ", candidates AS MATERIALIZED (SELECT rowid AS document_id FROM search_fts WHERE search_fts MATCH ?3 UNION SELECT document_id FROM search_documents WHERE namespace=?1 AND has_nul=1 UNION SELECT document_id FROM search_documents WHERE namespace=?1 AND has_non_ascii=1 AND ?4)"
    } else {
        ""
    };
    let documents = if candidate {
        "candidates c CROSS JOIN search_documents x ON x.document_id=c.document_id"
    } else {
        "search_documents x INDEXED BY documents_session"
    };
    let eligible = " AND x.agent||':'||x.id IN (SELECT agent||':'||id FROM eligible)";
    format!(
        "WITH eligible AS MATERIALIZED (SELECT json_extract(value,'$.agent') AS agent,json_extract(value,'$.id') AS id FROM json_each(?2)){candidates} SELECT x.agent,x.id,x.entry_index,x.kind,e.text,CASE WHEN x.entry_index IS NULL THEN t.metadata END,e.role,x.text_start FROM {documents} JOIN sessions t ON t.namespace=x.namespace AND t.agent=x.agent AND t.id=x.id AND t.source=x.source LEFT JOIN entries e ON e.namespace=x.namespace AND e.agent=x.agent AND e.id=x.id AND e.source=x.source AND e.entry_index=x.entry_index {VISIBLE}{eligible} AND (x.kind='title' OR ?5='[]' OR x.kind IN (SELECT value FROM json_each(?5))) ORDER BY x.agent,x.id,x.entry_index IS NOT NULL,x.document_id"
    )
}

pub(super) fn encode<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    Ok(serde_json::from_str(value)?)
}

pub(super) fn cache(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
) -> Result<DiscoveryCache> {
    Ok(DiscoveryCache {
        by_source: cached(connection, namespace, agent, "fresh")?,
        verify: false,
    })
}

pub(super) fn partial(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
) -> Result<BTreeMap<String, CachedSource>> {
    cached(connection, namespace, agent, "partial")
}

fn cached(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
    status: &str,
) -> Result<BTreeMap<String, CachedSource>> {
    let mut statement = connection.prepare("SELECT s.id,s.source,s.dependencies,s.fingerprint,s.state FROM sources s JOIN sessions t ON t.namespace=s.namespace AND t.agent=s.agent AND t.id=s.id AND t.source=s.source WHERE s.namespace=?1 AND s.agent=?2 AND s.status=?3")?;
    let mut rows = statement.query((namespace, agent.to_string(), status))?;
    let mut by_source = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let source: String = row.get(1)?;
        by_source.insert(
            source.clone(),
            CachedSource {
                key: SessionKey {
                    agent,
                    id: row.get(0)?,
                },
                source,
                dependencies: decode(&row.get::<_, String>(2)?)?,
                fingerprint: decode(&row.get::<_, String>(3)?)?,
                state: row
                    .get::<_, Option<String>>(4)?
                    .as_deref()
                    .map(decode)
                    .transpose()?,
            },
        );
    }
    Ok(by_source)
}

pub(super) fn next_generation(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
) -> Result<i64> {
    connection.execute("INSERT INTO refresh_runs(namespace,agent,generation,status,diagnostics) VALUES(?1,?2,1,'incomplete','[]') ON CONFLICT(namespace,agent) DO UPDATE SET generation=generation+1,status='incomplete',diagnostics='[]'", (namespace, agent.to_string()))?;
    Ok(connection.query_row(
        "SELECT generation FROM refresh_runs WHERE namespace=?1 AND agent=?2",
        (namespace, agent.to_string()),
        |r| r.get(0),
    )?)
}

pub(super) fn discovery_failed(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
    diagnostics: &[Diagnostic],
    seq: i64,
) -> Result<()> {
    connection.execute("INSERT INTO refresh_runs(namespace,agent,generation,status,diagnostics,published,published_seq) VALUES(?1,?2,0,'incomplete',?3,0,?4) ON CONFLICT(namespace,agent) DO UPDATE SET status='incomplete',diagnostics=excluded.diagnostics,published_seq=excluded.published_seq", (namespace, agent.to_string(), encode(&diagnostics)?, seq))?;
    Ok(())
}

pub(super) fn begin_run(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        "UPDATE settings SET refresh_seq=refresh_seq+1 WHERE id=1 RETURNING refresh_seq",
        [],
        |r| r.get(0),
    )?)
}

pub(super) fn published(
    connection: &Connection,
    namespace: &str,
    agents: &BTreeSet<Agent>,
    since: i64,
) -> Result<Option<Vec<Diagnostic>>> {
    let mut statement = connection.prepare("SELECT diagnostics FROM refresh_runs WHERE namespace=?1 AND agent=?2 AND published=generation AND published_seq>?3")?;
    let mut diagnostics = Vec::new();
    for agent in agents {
        let mut rows = statement.query(params![namespace, agent.to_string(), since])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        diagnostics.extend(decode::<Vec<Diagnostic>>(&row.get::<_, String>(0)?)?);
    }
    Ok(Some(diagnostics))
}

pub(super) fn has_published(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM refresh_runs WHERE namespace=?1 AND agent=?2 AND published>=1)",
        (namespace, agent.to_string()),
        |r| r.get(0),
    )?)
}

pub(super) fn throughput(
    connection: &Connection,
    agent: Agent,
    coverage: Coverage,
) -> Result<Option<(u64, u64)>> {
    Ok(connection
        .query_row(
            "SELECT bytes,micros FROM throughput WHERE agent=?1 AND coverage=?2",
            [agent.to_string(), coverage_name(coverage).to_owned()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub(super) fn observe(
    connection: &Connection,
    agent: Agent,
    coverage: Coverage,
    bytes: u64,
    micros: u64,
) -> Result<()> {
    connection.execute("INSERT INTO throughput(agent,coverage,bytes,micros) VALUES(?1,?2,?3,?4) ON CONFLICT(agent,coverage) DO UPDATE SET bytes=bytes+excluded.bytes,micros=micros+excluded.micros", params![agent.to_string(), coverage_name(coverage), bytes, micros])?;
    Ok(())
}

pub(super) fn carry_forward(
    connection: &Connection,
    namespace: &str,
    agent: Agent,
    generation: i64,
    seen: &BTreeSet<String>,
) -> Result<()> {
    connection.execute("UPDATE sources SET generation=?1 WHERE namespace=?2 AND agent=?3 AND status IN ('fresh','partial') AND generation>=COALESCE((SELECT published FROM refresh_runs WHERE namespace=?2 AND agent=?3),0) AND source NOT IN (SELECT value FROM json_each(?4))", params![generation, namespace, agent.to_string(), encode(seen)?])?;
    Ok(())
}

pub(super) fn source(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    status: &str,
    diagnostics: &[Diagnostic],
    generation: i64,
    state: Option<&SourceState>,
) -> Result<()> {
    connection.execute("INSERT INTO sources(namespace,agent,source,id,dependencies,fingerprint,status,diagnostics,generation,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(namespace,agent,source) DO UPDATE SET id=excluded.id,dependencies=excluded.dependencies,fingerprint=excluded.fingerprint,status=excluded.status,diagnostics=excluded.diagnostics,generation=excluded.generation,state=excluded.state",
        params![namespace,raw.key.agent.to_string(),raw.source,raw.key.id,encode(&raw.dependencies)?,encode(&raw.fingerprint)?,status,encode(&diagnostics)?,generation,state.map(encode).transpose()?])?;
    Ok(())
}

#[derive(Clone, Copy)]
struct Owner<'a> {
    namespace: &'a str,
    agent: &'a str,
    id: &'a str,
    source: &'a str,
}

pub(super) fn entry(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    entry: &Entry,
    coverage: Coverage,
) -> Result<()> {
    if entry.session != raw.key {
        return Err(Error::InvalidArgument(
            "source changed its session identity during parsing".into(),
        ));
    }
    let index = i64::try_from(entry.index)
        .map_err(|_| Error::InvalidArgument("entry index exceeds SQLite range".into()))?;
    let agent = entry.session.agent.to_string();
    connection.prepare_cached("INSERT INTO entries(namespace,agent,id,source,entry_index,source_id,timestamp,timestamp_raw,role,kind,subtype,tool_name,automated,searchable,text) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)")?
        .execute(params![namespace,agent,entry.session.id,raw.source,index,entry.source_id,entry.timestamp,entry.timestamp_raw,entry.role,kind_name(entry.kind),entry.subtype,entry.tool_name,entry.automated,searchable_name(entry.searchable),entry.text])?;
    let owner = Owner {
        namespace,
        agent: &agent,
        id: &raw.key.id,
        source: &raw.source,
    };
    entry_document(
        connection,
        owner,
        index,
        coverage,
        (entry.kind, entry.subtype.as_deref(), entry.searchable),
        &entry.text,
    )
}

fn entry_document(
    connection: &Connection,
    owner: Owner<'_>,
    index: i64,
    coverage: Coverage,
    (kind, subtype, searchable): (Kind, Option<&str>, Searchable),
    text: &str,
) -> Result<()> {
    searchable_start(coverage, kind, subtype, searchable, text).map_or(Ok(()), |start| {
        document(connection, owner, Some(index), kind_name(kind), text, start)
    })
}

fn document(
    connection: &Connection,
    owner: Owner<'_>,
    index: Option<i64>,
    kind: &str,
    text: &str,
    start: usize,
) -> Result<()> {
    let slice = &text[start..];
    let text_start = i64::try_from(start)
        .map_err(|_| Error::InvalidArgument("document offset exceeds SQLite range".into()))?;
    connection.prepare_cached("INSERT INTO search_documents(namespace,agent,id,source,entry_index,kind,text_start,has_non_ascii,has_nul) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?
        .execute(params![owner.namespace,owner.agent,owner.id,owner.source,index,kind,text_start,!slice.is_ascii(),slice.contains('\0')])?;
    connection
        .prepare_cached("INSERT INTO search_fts(rowid,ascii_lower) VALUES(?1,?2)")?
        .execute(params![
            connection.last_insert_rowid(),
            slice.to_ascii_lowercase()
        ])?;
    Ok(())
}

pub(super) fn session(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    session: &Session,
) -> Result<()> {
    if session.key != raw.key || session.source != raw.source {
        return Err(Error::InvalidArgument(
            "source changed its session identity during parsing".into(),
        ));
    }
    connection.execute(
        "INSERT INTO sessions(namespace,agent,id,source,metadata) VALUES(?1,?2,?3,?4,?5)",
        params![
            namespace,
            raw.key.agent.to_string(),
            raw.key.id,
            raw.source,
            encode(session)?
        ],
    )?;
    title(connection, namespace, raw, session)
}

pub(super) fn truncate(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    from: u64,
) -> Result<()> {
    let from = i64::try_from(from)
        .map_err(|_| Error::InvalidArgument("entry index exceeds SQLite range".into()))?;
    let key = params![
        namespace,
        raw.key.agent.to_string(),
        raw.key.id,
        raw.source,
        from
    ];
    connection.execute("DELETE FROM search_documents WHERE namespace=?1 AND agent=?2 AND id=?3 AND source=?4 AND (entry_index IS NULL OR entry_index>=?5)", key)?;
    connection.execute("DELETE FROM entries WHERE namespace=?1 AND agent=?2 AND id=?3 AND source=?4 AND entry_index>=?5", key)?;
    Ok(())
}

pub(super) fn update_session(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    session: &Session,
) -> Result<bool> {
    if session.key != raw.key || session.source != raw.source {
        return Err(Error::InvalidArgument(
            "source changed its session identity during parsing".into(),
        ));
    }
    let updated = connection.execute(
        "UPDATE sessions SET metadata=?5 WHERE namespace=?1 AND agent=?2 AND id=?3 AND source=?4",
        params![
            namespace,
            raw.key.agent.to_string(),
            raw.key.id,
            raw.source,
            encode(session)?
        ],
    )?;
    if updated == 1 {
        title(connection, namespace, raw, session)?;
    }
    Ok(updated == 1)
}

fn title(
    connection: &Connection,
    namespace: &str,
    raw: &RawSession,
    session: &Session,
) -> Result<()> {
    let agent = raw.key.agent.to_string();
    title_document(
        connection,
        Owner {
            namespace,
            agent: &agent,
            id: &raw.key.id,
            source: &raw.source,
        },
        session,
    )
}

fn title_document(connection: &Connection, owner: Owner<'_>, session: &Session) -> Result<()> {
    if session.title_origin == Some(TitleOrigin::Native)
        && let Some(title) = &session.title
    {
        document(connection, owner, None, "title", title, 0)?;
    }
    Ok(())
}

const REBUILD_ROWS: usize = 10_000;
const REBUILD_BYTES: usize = 64 << 20;

fn batched(
    connection: &mut Connection,
    sql: &str,
    write: &mut dyn FnMut(&Connection, &Row<'_>) -> Result<usize>,
) -> Result<()> {
    let mut after = i64::MIN;
    loop {
        let transaction = connection.transaction()?;
        let mut more = false;
        {
            let mut statement = transaction.prepare_cached(sql)?;
            let mut rows = statement.query([after])?;
            let (mut count, mut bytes) = (0, 0);
            while let Some(row) = rows.next()? {
                after = row.get(0)?;
                bytes += write(&transaction, row)?;
                count += 1;
                if count >= REBUILD_ROWS || bytes >= REBUILD_BYTES {
                    more = true;
                    break;
                }
            }
        }
        transaction.commit()?;
        if !more {
            return Ok(());
        }
    }
}

pub(super) fn rebuild_documents(connection: &mut Connection, target: Coverage) -> Result<()> {
    connection.execute(
        "UPDATE settings SET coverage_pending=?1 WHERE id=1",
        [coverage_name(target)],
    )?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO search_fts(search_fts) VALUES('delete-all')",
        [],
    )?;
    transaction.execute("DELETE FROM search_documents", [])?;
    transaction.commit()?;
    batched(
        connection,
        "SELECT rowid,namespace,agent,id,source,entry_index,kind,subtype,searchable,text FROM entries WHERE rowid>?1 ORDER BY rowid",
        &mut |connection, row| {
            let (agent, id, source): (String, String, String) =
                (row.get(2)?, row.get(3)?, row.get(4)?);
            let namespace: String = row.get(1)?;
            let kind = parse_kind(row.get(6)?)?;
            let subtype: Option<String> = row.get(7)?;
            let searchable = parse_searchable(&row.get::<_, String>(8)?)?;
            let text: String = row.get(9)?;
            entry_document(
                connection,
                Owner {
                    namespace: &namespace,
                    agent: &agent,
                    id: &id,
                    source: &source,
                },
                row.get(5)?,
                target,
                (kind, subtype.as_deref(), searchable),
                &text,
            )?;
            Ok(text.len())
        },
    )?;
    batched(
        connection,
        "SELECT rowid,namespace,agent,id,source,metadata FROM sessions WHERE rowid>?1 ORDER BY rowid",
        &mut |connection, row| {
            let (namespace, agent, id, source): (String, String, String, String) =
                (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
            let metadata: String = row.get(5)?;
            title_document(
                connection,
                Owner {
                    namespace: &namespace,
                    agent: &agent,
                    id: &id,
                    source: &source,
                },
                &decode(&metadata)?,
            )?;
            Ok(metadata.len())
        },
    )?;
    connection.execute(
        "UPDATE settings SET coverage=?1,coverage_pending=NULL WHERE id=1",
        [coverage_name(target)],
    )?;
    let mut statement = connection.prepare("PRAGMA incremental_vacuum")?;
    let mut rows = statement.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

fn parse_kind(kind: String) -> Result<Kind> {
    Ok(serde_json::from_value(serde_json::Value::String(kind))?)
}

const VISIBLE: &str = "JOIN sources s ON s.namespace=x.namespace AND s.agent=x.agent AND s.source=x.source JOIN refresh_runs r ON r.namespace=s.namespace AND r.agent=s.agent WHERE x.namespace=?1 AND s.status IN ('fresh','partial') AND s.generation>=COALESCE(r.published,0)";

impl IndexSession {
    fn readable(&self, agent: Agent) -> bool {
        self.refreshed.contains(&agent) || self.stale.contains(&agent)
    }

    fn ready(&self, agent: Agent) -> Result<()> {
        if !self.readable(agent) {
            return Err(Error::InvalidArgument(
                "selected agent must be refreshed by this index session before querying".into(),
            ));
        }
        Ok(())
    }

    pub fn visit_sessions(&self, emit: &mut dyn FnMut(Session) -> Result<()>) -> Result<()> {
        if self.refreshed.is_empty() && self.stale.is_empty() {
            return Err(Error::InvalidArgument(
                "index must be refreshed by this index session before querying".into(),
            ));
        }
        let sql = format!("SELECT x.metadata FROM sessions x {VISIBLE} ORDER BY x.agent,x.id");
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query([&self.namespace])?;
        while let Some(row) = rows.next()? {
            let session: Session = decode(&row.get::<_, String>(0)?)?;
            if self.readable(session.key.agent) {
                emit(session)?;
            }
        }
        Ok(())
    }

    pub fn session(&self, key: &SessionKey) -> Result<Option<Session>> {
        self.ready(key.agent)?;
        let sql = format!("SELECT x.metadata FROM sessions x {VISIBLE} AND x.agent=?2 AND x.id=?3");
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query((&self.namespace, key.agent.to_string(), &key.id))?;
        rows.next()?
            .map(|r| decode(&r.get::<_, String>(0)?))
            .transpose()
    }

    pub fn visit_entries(
        &self,
        key: &SessionKey,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<()> {
        self.ready(key.agent)?;
        let sql = format!(
            "SELECT x.entry_index,x.source_id,x.timestamp,x.timestamp_raw,x.role,x.kind,x.subtype,x.tool_name,x.automated,x.text,x.searchable FROM entries x {VISIBLE} AND x.agent=?2 AND x.id=?3 ORDER BY x.entry_index"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query((&self.namespace, key.agent.to_string(), &key.id))?;
        while let Some(row) = rows.next()? {
            emit(read_entry(row, key)?)?;
        }
        Ok(())
    }

    pub(crate) fn visit_events(
        &self,
        key: &SessionKey,
        since: i64,
        until: i64,
        emit: &mut dyn FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        self.ready(key.agent)?;
        let sql = format!(
            "SELECT x.timestamp,x.role='user' AND x.kind='message' AND NOT x.automated FROM entries x {VISIBLE} AND x.agent=?2 AND x.id=?3 AND (x.timestamp IS NULL OR (x.timestamp>=?4 AND x.timestamp<?5)) ORDER BY x.entry_index"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![
            self.namespace,
            key.agent.to_string(),
            key.id,
            since,
            until
        ])?;
        while let Some(row) = rows.next()? {
            emit(Event {
                timestamp: row.get(0)?,
                human_prompt: row.get(1)?,
            })?;
        }
        Ok(())
    }

    pub fn visit_documents(
        &self,
        key: &SessionKey,
        literal_candidate: Option<(&str, bool)>,
        emit: &mut dyn FnMut(Document) -> Result<()>,
    ) -> Result<()> {
        self.ready(key.agent)?;
        self.documents(
            std::slice::from_ref(key),
            &BTreeSet::new(),
            literal_candidate,
            emit,
        )
    }

    pub fn visit_search_documents(
        &self,
        literal_candidate: Option<(&str, bool)>,
        emit: &mut dyn FnMut(Document) -> Result<()>,
    ) -> Result<()> {
        if self.refreshed.is_empty() && self.stale.is_empty() {
            return Err(Error::InvalidArgument(
                "index must be refreshed by this index session before querying".into(),
            ));
        }
        let mut keys = Vec::new();
        self.visit_sessions(&mut |session| {
            keys.push(session.key);
            Ok(())
        })?;
        self.documents(&keys, &BTreeSet::new(), literal_candidate, emit)
    }

    pub(crate) fn documents(
        &self,
        keys: &[SessionKey],
        kinds: &BTreeSet<Kind>,
        literal_candidate: Option<(&str, bool)>,
        emit: &mut dyn FnMut(Document) -> Result<()>,
    ) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        for key in keys {
            self.ready(key.agent)?;
        }
        let candidate = literal_candidate
            .filter(|(text, _)| text.is_ascii() && text.len() >= 3 && !text.contains('\0'));
        let quoted = candidate.map(|(literal, _)| {
            format!("\"{}\"", literal.to_ascii_lowercase().replace('"', "\"\""))
        });
        let unicode_fallback = candidate.is_some_and(|(_, case_sensitive)| !case_sensitive);
        let sql = document_sql(candidate.is_some());
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![
            self.namespace,
            encode(&keys)?,
            quoted,
            unicode_fallback,
            encode(&kinds)?
        ])?;
        while let Some(row) = rows.next()? {
            let agent = row.get::<_, String>(0)?.parse()?;
            if !self.readable(agent) {
                continue;
            }
            let entry_index: Option<u64> = row.get(2)?;
            let mut text: String = if entry_index.is_some() {
                row.get(4)?
            } else {
                decode::<Session>(&row.get::<_, String>(5)?)?
                    .title
                    .unwrap_or_default()
            };
            let start: usize = row.get(7)?;
            if !text.is_char_boundary(start) {
                return Err(Error::InvalidArgument(
                    "search document offset is not a character boundary".into(),
                ));
            }
            text.drain(..start);
            emit(Document {
                role: row.get(6)?,
                session: SessionKey {
                    agent,
                    id: row.get(1)?,
                },
                entry_index,
                kind: row.get(3)?,
                text,
            })?;
        }
        Ok(())
    }

    pub fn verify_integrity(&self) -> Result<()> {
        let _lock = super::lock::writer(&self.context.index_dir, super::REFRESH_WAIT, false)?
            .ok_or(Error::IndexBusy)?;
        let connection = super::schema::open(
            &self.context.index_dir,
            &self.namespace,
            &self.roots,
            self.context.coverage,
        )?;
        let failed = || Error::InvalidArgument("index integrity check failed".into());
        let integrity: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if integrity != "ok" || connection.prepare("PRAGMA foreign_key_check")?.exists([])? {
            return Err(failed());
        }
        connection.execute(
            "INSERT INTO search_fts(search_fts,rank) VALUES('integrity-check',1)",
            [],
        )?;
        let counts: (i64, i64) = connection.query_row(
            "SELECT (SELECT count(*) FROM search_documents),(SELECT count(*) FROM search_fts)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if counts.0 != counts.1 {
            return Err(failed());
        }
        let mut sample = connection.prepare("SELECT x.document_id,x.text_start,x.has_non_ascii,x.has_nul,CASE WHEN x.entry_index IS NULL THEN t.metadata ELSE e.text END,x.entry_index IS NULL FROM search_documents x JOIN sessions t ON t.namespace=x.namespace AND t.agent=x.agent AND t.id=x.id AND t.source=x.source LEFT JOIN entries e ON e.namespace=x.namespace AND e.agent=x.agent AND e.id=x.id AND e.source=x.source AND e.entry_index=x.entry_index ORDER BY random() LIMIT 100")?;
        let mut matched = connection
            .prepare("SELECT count(*) FROM search_fts WHERE search_fts MATCH ?1 AND rowid=?2")?;
        let mut rows = sample.query([])?;
        while let Some(row) = rows.next()? {
            let document: i64 = row.get(0)?;
            let start: usize = row.get(1)?;
            let stored: String = row.get(4)?;
            let text = if row.get(5)? {
                decode::<Session>(&stored)?.title.unwrap_or_default()
            } else {
                stored
            };
            let slice = text.get(start..).ok_or_else(failed)?;
            if row.get::<_, bool>(2)? == slice.is_ascii()
                || row.get::<_, bool>(3)? != slice.contains('\0')
            {
                return Err(failed());
            }
            let trigram: String = slice.to_ascii_lowercase().chars().take(3).collect();
            if trigram.chars().count() < 3 || trigram.contains('\0') {
                continue;
            }
            let phrase = format!("\"{}\"", trigram.replace('"', "\"\""));
            let found: i64 = matched.query_row(params![phrase, document], |r| r.get(0))?;
            if found != 1 {
                return Err(failed());
            }
        }
        Ok(())
    }
}

fn read_entry(row: &Row<'_>, key: &SessionKey) -> Result<Entry> {
    let kind = parse_kind(row.get(5)?)?;
    Ok(Entry {
        session: key.clone(),
        index: row.get(0)?,
        source_id: row.get(1)?,
        timestamp: row.get(2)?,
        timestamp_raw: row.get(3)?,
        role: row.get(4)?,
        kind,
        subtype: row.get(6)?,
        tool_name: row.get(7)?,
        automated: row.get(8)?,
        text: row.get(9)?,
        searchable: parse_searchable(&row.get::<_, String>(10)?)?,
    })
}

const fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Message => "message",
        Kind::Meta => "meta",
        Kind::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Context;
    use std::{fs, time::Instant};

    fn fixture(count: usize) -> (tempfile::TempDir, IndexSession, Vec<SessionKey>) {
        text_fixture(count, &["chosen".into(), "other".into()], "ordinary text")
    }

    fn text_fixture(
        count: usize,
        ids: &[String],
        text: &str,
    ) -> (tempfile::TempDir, IndexSession, Vec<SessionKey>) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            roots: crate::resolve_roots(dir.path(), &BTreeMap::new()).unwrap(),
            index_dir: dir.path().join("index"),
            cwd: dir.path().into(),
            now_ms: 0,
            time_zone: "UTC".into(),
            coverage: crate::Coverage::Compact,
            build: crate::BuildPolicy::Inline,
            progress: false,
        };
        fs::create_dir_all(&ctx.roots.claude_projects).unwrap();
        for id in ids {
            let records = (0..count)
                .map(|i| {
                    serde_json::json!({"type":"assistant", "sessionId":id,
                "timestamp":"2026-11-16T00:00:00Z", "message":if i == 0 {"needle"} else {text}})
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(
                ctx.roots.claude_projects.join(format!("{id}.jsonl")),
                records,
            )
            .unwrap();
        }
        let mut index = IndexSession::open(&ctx).unwrap();
        index
            .refresh_under_lease(&BTreeSet::from([Agent::Claude]))
            .unwrap();
        let keys = ids
            .iter()
            .map(|id| SessionKey {
                agent: Agent::Claude,
                id: id.into(),
            })
            .collect();
        (dir, index, keys)
    }

    fn legacy_sql() -> String {
        format!(
            "SELECT x.agent,x.id,x.entry_index,x.kind,e.text,CASE WHEN x.entry_index IS NULL THEN t.metadata END,e.role FROM search_documents x JOIN sessions t ON t.namespace=x.namespace AND t.agent=x.agent AND t.id=x.id AND t.source=x.source LEFT JOIN entries e ON e.namespace=x.namespace AND e.agent=x.agent AND e.id=x.id AND e.source=x.source AND e.entry_index=x.entry_index {VISIBLE} AND (?2 IS NULL OR (x.agent=?2 AND x.id=?3)) AND (?4 IS NULL OR x.document_id IN (SELECT rowid FROM search_fts WHERE search_fts MATCH ?4) OR x.has_nul=1 OR (?5 AND x.has_non_ascii=1)) ORDER BY x.agent,x.id,x.entry_index IS NOT NULL,x.document_id"
        )
    }

    fn measure(
        index: &IndexSession,
        keys: &[SessionKey],
        query: &str,
        legacy: bool,
    ) -> (usize, i32, u128) {
        let sql = if legacy {
            legacy_sql()
        } else {
            document_sql(true)
        };
        let mut statement = index.connection.prepare(&sql).unwrap();
        let encoded = encode(&keys).unwrap();
        let quoted = format!("\"{query}\"");
        let started = Instant::now();
        let mut rows = if legacy {
            statement
                .query(params![
                    index.namespace,
                    Option::<String>::None,
                    Option::<String>::None,
                    quoted,
                    true
                ])
                .unwrap()
        } else {
            statement
                .query(params![index.namespace, encoded, quoted, true, "[]"])
                .unwrap()
        };
        let mut count = 0;
        while let Some(row) = rows.next().unwrap() {
            let _: Option<String> = row.get(4).unwrap();
            count += 1;
        }
        drop(rows);
        (
            count,
            statement.get_status(rusqlite::StatementStatus::VmStep),
            started.elapsed().as_micros(),
        )
    }

    #[test]
    fn bundled_candidate_plan_uses_rowids_and_indexed_fallbacks() {
        let (_dir, index, keys) = fixture(128);
        let sql = format!("EXPLAIN QUERY PLAN {}", document_sql(true));
        let plan: Vec<String> = index
            .connection
            .prepare(&sql)
            .unwrap()
            .query_map(
                params![
                    index.namespace,
                    encode(&keys).unwrap(),
                    "\"missing\"",
                    true,
                    "[]"
                ],
                |row| row.get(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        println!(
            "bundled SQLite {} candidate plan:\n{}",
            rusqlite::version(),
            plan.join("\n")
        );
        for expected in [
            "SEARCH x USING INTEGER PRIMARY KEY",
            "documents_nul",
            "documents_non_ascii",
        ] {
            assert!(plan.iter().any(|line| line.contains(expected)), "{plan:?}");
        }
        for query in ["missing", "needle"] {
            let old = measure(&index, &keys, query, true);
            let new = measure(&index, &keys, query, false);
            assert_eq!(old.0, new.0);
            assert!(new.1 < old.1, "{old:?} {new:?}");
        }
    }

    #[test]
    fn bundled_fallback_sorts_only_one_session_before_first_row() {
        for sessions in [10, 100] {
            let ids: Vec<_> = (0..sessions)
                .rev()
                .map(|i| format!("session-{i:03}"))
                .collect();
            let payload = "x".repeat(8190);
            let (_dir, index, keys) = text_fixture(10, &ids, &payload);
            let sql = document_sql(false);
            let bindings = params![
                index.namespace,
                encode(&keys).unwrap(),
                Option::<String>::None,
                false,
                "[]"
            ];
            let plan: Vec<String> = index
                .connection
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(bindings, |row| row.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|line| line == "USE TEMP B-TREE FOR LAST 2 TERMS OF ORDER BY"),
                "{plan:?}"
            );
            let sorters: Vec<String> = index
                .connection
                .prepare(&format!("EXPLAIN {sql}"))
                .unwrap()
                .query_map(bindings, |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, Option<String>>(5)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .into_iter()
                .filter_map(|(opcode, key)| {
                    (opcode == "SorterOpen").then_some(key.unwrap_or_default())
                })
                .collect();
            assert_eq!(sorters, ["k(2,B,B)"]);
            let mut statement = index.connection.prepare(&sql).unwrap();
            let started = Instant::now();
            let mut rows = statement.query(bindings).unwrap();
            let first = rows.next().unwrap().unwrap();
            assert_eq!(first.get::<_, String>(1).unwrap(), "session-000");
            assert_eq!(first.get::<_, String>(4).unwrap(), "needle");
            drop(rows);
            let first_steps = statement.get_status(rusqlite::StatementStatus::VmStep);
            assert!(
                first_steps < 2000 + sessions * 40,
                "sessions={sessions} steps={first_steps}"
            );
            println!(
                "bundled SQLite {} fallback sessions={sessions} entries={} payload_bytes={} first_row_vm_steps={first_steps} first_row_us={}",
                rusqlite::version(),
                sessions * 10,
                payload.len(),
                started.elapsed().as_micros()
            );
            let mut observed = Vec::new();
            index
                .documents(&keys, &BTreeSet::new(), None, &mut |doc| {
                    assert_eq!(
                        doc.text,
                        if doc.entry_index == Some(0) {
                            "needle"
                        } else {
                            &payload
                        }
                    );
                    observed.push((doc.session.id, doc.entry_index));
                    Ok(())
                })
                .unwrap();
            let expected: Vec<_> = (0..sessions)
                .flat_map(|i| (0..10).map(move |entry| (format!("session-{i:03}"), Some(entry))))
                .collect();
            assert_eq!(observed, expected);
        }
    }

    #[test]
    fn filtered_documents_and_timeline_do_not_hydrate_rejected_text() {
        let (_dir, index, keys) = fixture(2);
        let writer =
            Connection::open(index.context.index_dir.join(super::super::DATABASE)).unwrap();
        writer
            .execute(
                "UPDATE entries SET text=x'ff' WHERE id='other' OR entry_index=1",
                [],
            )
            .unwrap();
        writer
            .execute(
                "UPDATE search_documents SET kind='tool' WHERE id='chosen' AND entry_index=1",
                [],
            )
            .unwrap();
        for candidate in [None, Some(("needle", false))] {
            let mut found = Vec::new();
            index
                .documents(
                    &keys[..1],
                    &BTreeSet::from([Kind::Message]),
                    candidate,
                    &mut |doc| {
                        found.push(doc.text);
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(found, ["needle"]);
        }
        let mut empty_count = 0;
        index
            .documents(&[], &BTreeSet::new(), None, &mut |_| {
                empty_count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(empty_count, 0);
        let mut events = Vec::new();
        index
            .visit_events(&keys[1], 0, i64::MAX, &mut |event| {
                events.push(event.timestamp);
                Ok(())
            })
            .unwrap();
        assert_eq!(events, [Some(1_794_787_200_000), Some(1_794_787_200_000)]);
    }

    #[test]
    #[ignore = "bounded release measurement; run explicitly with --release --ignored --nocapture"]
    fn candidate_query_release_scaling() {
        for count in [1000, 2000, 4000] {
            let (_dir, index, keys) = fixture(count);
            for query in ["missing", "needle"] {
                let old = measure(&index, &keys, query, true);
                let new = measure(&index, &keys, query, false);
                assert_eq!(old.0, new.0);
                println!(
                    "documents={} query={query} legacy_rows={} legacy_vm_steps={} legacy_us={} corrected_rows={} corrected_vm_steps={} corrected_us={}",
                    count * 2,
                    old.0,
                    old.1,
                    old.2,
                    new.0,
                    new.1,
                    new.2
                );
            }
            index.verify_integrity().unwrap();
        }
    }
}
