use super::lock::{check_file, io};
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
const LAYOUT: i32 = 2;
const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

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
 status TEXT NOT NULL, recheck INTEGER NOT NULL DEFAULT 0, diagnostics TEXT NOT NULL, generation INTEGER NOT NULL, state TEXT,
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
 searchable TEXT NOT NULL, tail_start INTEGER NOT NULL, text_tail TEXT NOT NULL, text TEXT NOT NULL,
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
CREATE TABLE throughput(agent TEXT NOT NULL, coverage TEXT NOT NULL, bytes INTEGER NOT NULL, micros INTEGER NOT NULL, PRIMARY KEY(agent,coverage));
PRAGMA user_version=3;
";

pub(super) const DOCUMENTS_DELETE: &str = "CREATE TRIGGER documents_delete AFTER DELETE ON search_documents BEGIN DELETE FROM search_fts WHERE rowid=old.document_id; END;";

fn create(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    connection.execute_batch(DOCUMENTS_DELETE)?;
    connection.pragma_update(None, "application_id", LAYOUT)?;
    Ok(())
}

fn layout(connection: &Connection) -> Result<i32> {
    Ok(connection.query_row("PRAGMA application_id", [], |r| r.get(0))?)
}

fn check_layout(connection: &Connection) -> Result<()> {
    match layout(connection)? {
        LAYOUT => Ok(()),
        older if older < LAYOUT => Err(Error::InvalidArgument(
            "index layout is outdated; it is rebuilt once no other slimetrail process has it open"
                .into(),
        )),
        _ => Err(version_mismatch()),
    }
}

pub(super) fn preflight(directory: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        check_file(&directory.join(format!("{DATABASE}{suffix}")))?;
    }
    Ok(())
}

fn configure(connection: &Connection) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(30))?;
    connection.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA journal_size_limit=67108864; PRAGMA secure_delete=FAST;",
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
        0 => create(&transaction)?,
        VERSION => check_layout(&transaction)?,
        _ => {
            return Err(version_mismatch());
        }
    }
    transaction.execute("INSERT INTO meta(namespace,version,roots) VALUES(?1,?2,?3) ON CONFLICT(namespace) DO NOTHING", (namespace,VERSION,roots))?;
    transaction.execute(
        "INSERT INTO settings(id,refresh_seq,coverage) VALUES(1,0,?1) ON CONFLICT(id) DO NOTHING",
        [coverage.as_str()],
    )?;
    check_namespace(&transaction, namespace, roots)?;
    transaction.commit()?;
    Ok(connection)
}

fn version_mismatch() -> Error {
    Error::InvalidArgument("index schema version does not match its filename".into())
}

fn unbuilt() -> Result<Connection> {
    let connection = Connection::open_in_memory()?;
    create(&connection)?;
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
            check_layout(&connection)?;
            check_namespace(&connection, namespace, roots)?;
            Ok(connection)
        }
        _ => Err(version_mismatch()),
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
                coverage.parse()?,
                pending.as_deref().map(str::parse).transpose()?,
            ))
        })
        .transpose()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Effective {
    pub coverage: Coverage,
    pub stored: Option<Coverage>,
    pub pending: Option<Coverage>,
    pub locked: bool,
}

pub(super) const fn effective(
    stored: Option<(Coverage, Option<Coverage>)>,
    used: bool,
    configured: Coverage,
) -> Effective {
    let Some((stored, pending)) = stored else {
        return Effective {
            coverage: configured,
            stored: None,
            pending: None,
            locked: false,
        };
    };
    let locked = pending.is_some() || used;
    Effective {
        coverage: if locked { stored } else { configured },
        stored: Some(stored),
        pending,
        locked,
    }
}

pub(super) fn effective_coverage(
    connection: &Connection,
    configured: Coverage,
) -> Result<Effective> {
    let stored = coverage(connection)?;
    let used = match stored {
        Some((_, None)) => connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM refresh_runs WHERE published>=1) OR EXISTS(SELECT 1 FROM sources)",
            [],
            |r| r.get(0),
        )?,
        _ => false,
    };
    Ok(effective(stored, used, configured))
}

pub(super) fn set_coverage(connection: &Connection, coverage: Coverage) -> Result<()> {
    connection.execute(
        "UPDATE settings SET coverage=?1 WHERE id=1",
        [coverage.as_str()],
    )?;
    Ok(())
}

fn read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(30))?;
    Ok(connection)
}

fn outdated(path: &Path) -> Result<bool> {
    if !check_file(path)? {
        return Ok(false);
    }
    let connection = read_only(path)?;
    Ok(version(&connection)? == VERSION && layout(&connection)? < LAYOUT)
}

fn holds_data(path: &Path) -> bool {
    let visible = || -> Result<bool> {
        if !check_file(path)? {
            return Ok(false);
        }
        let connection = read_only(path)?;
        if version(&connection)? != VERSION || layout(&connection)? != LAYOUT {
            return Ok(false);
        }
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions t JOIN sources s ON s.namespace=t.namespace AND s.agent=t.agent AND s.source=t.source JOIN refresh_runs r ON r.namespace=s.namespace AND r.agent=s.agent WHERE r.published>=1 AND s.status IN ('fresh','partial') AND s.generation>=r.published)",
            [],
            |r| r.get(0),
        )?)
    };
    visible().unwrap_or(false)
}

fn remove(path: &Path) -> Result<()> {
    if check_file(path)? {
        fs::remove_file(path).map_err(|e| io(path, e))?;
    }
    Ok(())
}

fn remove_database(directory: &Path, name: &str) -> Result<()> {
    for suffix in SIDECARS {
        remove(&directory.join(format!("{name}{suffix}")))?;
    }
    remove(&directory.join(name))
}

fn older(name: &str) -> bool {
    let Some((digits, suffix)) = name
        .strip_prefix("index-v")
        .and_then(|rest| rest.split_once(".sqlite3"))
    else {
        return false;
    };
    digits.parse::<u32>().is_ok_and(|version| {
        version < VERSION
            && digits == version.to_string()
            && (suffix.is_empty() || SIDECARS.contains(&suffix))
    })
}

pub(super) fn prune(directory: &Path) -> Result<()> {
    let current = directory.join(DATABASE);
    if outdated(&current)? {
        remove_database(directory, DATABASE)?;
    }
    if !holds_data(&current) {
        return Ok(());
    }
    let mut stale = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| io(directory, e))? {
        let entry = entry.map_err(|e| io(directory, e))?;
        if older(&entry.file_name().to_string_lossy()) {
            stale.push(entry.path());
        }
    }
    stale.sort_by_key(|path| path.to_string_lossy().ends_with(".sqlite3"));
    stale.iter().try_for_each(|path| {
        check_file(path)?;
        fs::remove_file(path).map_err(|e| io(path, e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_coverage_applies_until_the_index_holds_data() {
        assert_eq!(
            effective(None, false, Coverage::Full),
            Effective {
                coverage: Coverage::Full,
                stored: None,
                pending: None,
                locked: false,
            }
        );
        assert_eq!(
            effective(Some((Coverage::Compact, None)), false, Coverage::Full),
            Effective {
                coverage: Coverage::Full,
                stored: Some(Coverage::Compact),
                pending: None,
                locked: false,
            }
        );
        assert_eq!(
            effective(Some((Coverage::Compact, None)), true, Coverage::Full),
            Effective {
                coverage: Coverage::Compact,
                stored: Some(Coverage::Compact),
                pending: None,
                locked: true,
            }
        );
    }

    #[test]
    fn a_pending_rebuild_locks_the_stored_coverage() {
        assert_eq!(
            effective(
                Some((Coverage::Compact, Some(Coverage::Full))),
                false,
                Coverage::Full
            ),
            Effective {
                coverage: Coverage::Compact,
                stored: Some(Coverage::Compact),
                pending: Some(Coverage::Full),
                locked: true,
            }
        );
    }
}
