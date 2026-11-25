use super::{
    coverage::{coverage_name, parse_coverage},
    lock::{check_file, io},
};
use crate::{Coverage, Error, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Duration,
};

pub const VERSION: u32 = 3;
pub const DATABASE: &str = "index-v3.sqlite3";

const SCHEMA: &str = "
PRAGMA auto_vacuum=INCREMENTAL;
CREATE TABLE meta(namespace TEXT PRIMARY KEY, version INTEGER NOT NULL, roots TEXT NOT NULL);
CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), refresh_seq INTEGER NOT NULL, coverage TEXT NOT NULL, coverage_pending TEXT);
CREATE TABLE refresh_runs(
 namespace TEXT NOT NULL REFERENCES meta(namespace), agent TEXT NOT NULL,
 generation INTEGER NOT NULL, status TEXT NOT NULL, diagnostics TEXT NOT NULL,
 published INTEGER, published_seq INTEGER,
 PRIMARY KEY(namespace,agent)
);
CREATE TABLE sources(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, source TEXT NOT NULL,
 id TEXT NOT NULL, dependencies TEXT NOT NULL, fingerprint TEXT NOT NULL,
 status TEXT NOT NULL, diagnostics TEXT NOT NULL, generation INTEGER NOT NULL, state TEXT,
 PRIMARY KEY(namespace,agent,source),
 FOREIGN KEY(namespace,agent) REFERENCES refresh_runs(namespace,agent)
);
CREATE TABLE sessions(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, id TEXT NOT NULL, source TEXT NOT NULL,
 metadata TEXT NOT NULL,
 PRIMARY KEY(namespace,agent,id), UNIQUE(namespace,agent,id,source),
 FOREIGN KEY(namespace,agent,source) REFERENCES sources(namespace,agent,source) ON DELETE CASCADE
);
CREATE TABLE entries(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, id TEXT NOT NULL, source TEXT NOT NULL,
 entry_index INTEGER NOT NULL, source_id TEXT NOT NULL, timestamp INTEGER, timestamp_raw TEXT,
 role TEXT NOT NULL, kind TEXT NOT NULL, subtype TEXT, tool_name TEXT, automated INTEGER NOT NULL,
 searchable TEXT NOT NULL, text TEXT NOT NULL,
 PRIMARY KEY(namespace,agent,id,source,entry_index),
 FOREIGN KEY(namespace,agent,id,source) REFERENCES sessions(namespace,agent,id,source) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE search_documents(
 document_id INTEGER PRIMARY KEY,
 namespace TEXT NOT NULL, agent TEXT NOT NULL, id TEXT NOT NULL, source TEXT NOT NULL,
 entry_index INTEGER, kind TEXT NOT NULL, text_start INTEGER NOT NULL,
 has_non_ascii INTEGER NOT NULL, has_nul INTEGER NOT NULL,
 FOREIGN KEY(namespace,agent,id,source) REFERENCES sessions(namespace,agent,id,source) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
 FOREIGN KEY(namespace,agent,id,source,entry_index) REFERENCES entries(namespace,agent,id,source,entry_index) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX documents_session ON search_documents(namespace,agent,id,source,entry_index);
CREATE INDEX documents_non_ascii ON search_documents(namespace,document_id) WHERE has_non_ascii=1;
CREATE INDEX documents_nul ON search_documents(namespace,document_id) WHERE has_nul=1;
CREATE VIRTUAL TABLE search_fts USING fts5(ascii_lower,content='',contentless_delete=1,tokenize='trigram case_sensitive 1',detail=full);
CREATE TRIGGER documents_delete AFTER DELETE ON search_documents BEGIN
 DELETE FROM search_fts WHERE rowid=old.document_id;
END;
PRAGMA user_version=3;
";

pub(super) fn preflight(directory: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        check_file(&directory.join(format!("{DATABASE}{suffix}")))?;
    }
    Ok(())
}

fn configure(connection: &Connection) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(30))?;
    connection.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA journal_size_limit=67108864;",
    )?;
    Ok(())
}

fn version(connection: &Connection) -> Result<u32> {
    Ok(connection.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

fn check_namespace(connection: &Connection, namespace: &str, roots: &str) -> Result<bool> {
    let stored: Option<(u32, String)> = connection
        .query_row(
            "SELECT version,roots FROM meta WHERE namespace=?",
            [namespace],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match stored {
        None => Ok(false),
        Some(stored) if stored == (VERSION, roots.into()) => Ok(true),
        Some(_) => Err(Error::InvalidArgument(
            "index namespace metadata is inconsistent".into(),
        )),
    }
}

pub(super) fn open(
    directory: &Path,
    namespace: &str,
    roots: &str,
    coverage: Coverage,
) -> Result<Connection> {
    let path = directory.join(DATABASE);
    preflight(directory)?;
    if !path.exists() {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| io(&path, e))?;
    }
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&connection)?;
    if version(&connection)? == 0 {
        connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(Error::InvalidArgument(
                "index database could not enter WAL mode".into(),
            ));
        }
    }
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    match version(&transaction)? {
        0 => transaction.execute_batch(SCHEMA)?,
        VERSION => {}
        _ => {
            return Err(Error::InvalidArgument(
                "index schema version does not match its filename".into(),
            ));
        }
    }
    transaction.execute("INSERT INTO meta(namespace,version,roots) VALUES(?1,?2,?3) ON CONFLICT(namespace) DO NOTHING", (namespace,VERSION,roots))?;
    transaction.execute(
        "INSERT INTO settings(id,refresh_seq,coverage) VALUES(1,0,?1) ON CONFLICT(id) DO NOTHING",
        [coverage_name(coverage)],
    )?;
    check_namespace(&transaction, namespace, roots)?;
    transaction.commit()?;
    Ok(connection)
}

fn unbuilt() -> Result<Connection> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(SCHEMA)?;
    Ok(connection)
}

pub(super) fn read(directory: &Path, namespace: &str, roots: &str) -> Result<Connection> {
    let path = directory.join(DATABASE);
    preflight(directory)?;
    if !path.exists() {
        return unbuilt();
    }
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&connection)?;
    match version(&connection)? {
        0 => unbuilt(),
        VERSION => {
            check_namespace(&connection, namespace, roots)?;
            Ok(connection)
        }
        _ => Err(Error::InvalidArgument(
            "index schema version does not match its filename".into(),
        )),
    }
}

pub(super) fn refresh_seq(connection: &Connection) -> Result<i64> {
    Ok(connection
        .query_row("SELECT refresh_seq FROM settings WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0))
}

pub(super) fn coverage(connection: &Connection) -> Result<Option<(Coverage, Option<Coverage>)>> {
    let stored: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT coverage,coverage_pending FROM settings WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    stored
        .map(|(coverage, pending)| {
            Ok((
                parse_coverage(&coverage)?,
                pending.as_deref().map(parse_coverage).transpose()?,
            ))
        })
        .transpose()
}

pub(super) fn prune(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|e| io(directory, e))? {
        let entry = entry.map_err(|e| io(directory, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("index-v") else {
            continue;
        };
        let Some((digits, suffix)) = rest.split_once(".sqlite3") else {
            continue;
        };
        let Ok(version) = digits.parse::<u32>() else {
            continue;
        };
        if version < VERSION
            && digits == version.to_string()
            && matches!(suffix, "" | "-wal" | "-shm" | "-journal")
        {
            check_file(&entry.path())?;
            fs::remove_file(entry.path()).map_err(|e| io(&entry.path(), e))?;
        }
    }
    Ok(())
}
