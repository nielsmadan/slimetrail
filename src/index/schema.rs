use super::lock::{check_file, io};
use crate::{Error, Result};
use rusqlite::Connection;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub const VERSION: u32 = 2;
pub const DATABASE: &str = "index-v2.sqlite3";

const SCHEMA: &str = "
CREATE TABLE meta(namespace TEXT PRIMARY KEY, version INTEGER NOT NULL, roots TEXT NOT NULL);
CREATE TABLE refresh_runs(
 namespace TEXT NOT NULL REFERENCES meta(namespace), agent TEXT NOT NULL,
 generation INTEGER NOT NULL, status TEXT NOT NULL, diagnostics TEXT NOT NULL,
 PRIMARY KEY(namespace,agent)
);
CREATE TABLE sources(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, source TEXT NOT NULL,
 id TEXT NOT NULL, dependencies TEXT NOT NULL, fingerprint TEXT NOT NULL,
 status TEXT NOT NULL, diagnostics TEXT NOT NULL, generation INTEGER NOT NULL,
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
 text TEXT NOT NULL,
 PRIMARY KEY(namespace,agent,id,source,entry_index),
 FOREIGN KEY(namespace,agent,id,source) REFERENCES sessions(namespace,agent,id,source) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE search_documents(
 document_id INTEGER PRIMARY KEY,
 namespace TEXT NOT NULL, agent TEXT NOT NULL, id TEXT NOT NULL, source TEXT NOT NULL,
 entry_index INTEGER, kind TEXT NOT NULL, ascii_lower TEXT NOT NULL,
 has_non_ascii INTEGER NOT NULL, has_nul INTEGER NOT NULL,
 FOREIGN KEY(namespace,agent,id,source) REFERENCES sessions(namespace,agent,id,source) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
 FOREIGN KEY(namespace,agent,id,source,entry_index) REFERENCES entries(namespace,agent,id,source,entry_index) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX documents_session ON search_documents(namespace,agent,id,source,entry_index);
CREATE INDEX documents_non_ascii ON search_documents(namespace,document_id) WHERE has_non_ascii=1;
CREATE INDEX documents_nul ON search_documents(namespace,document_id) WHERE has_nul=1;
CREATE VIRTUAL TABLE search_fts USING fts5(ascii_lower,content='search_documents',content_rowid='document_id',tokenize='trigram case_sensitive 1',detail=full);
CREATE TRIGGER documents_insert AFTER INSERT ON search_documents BEGIN
 INSERT INTO search_fts(rowid,ascii_lower) VALUES(new.document_id,new.ascii_lower);
END;
CREATE TRIGGER documents_delete AFTER DELETE ON search_documents BEGIN
 INSERT INTO search_fts(search_fts,rowid,ascii_lower) VALUES('delete',old.document_id,old.ascii_lower);
END;
CREATE TRIGGER documents_update AFTER UPDATE ON search_documents BEGIN
 INSERT INTO search_fts(search_fts,rowid,ascii_lower) VALUES('delete',old.document_id,old.ascii_lower);
 INSERT INTO search_fts(rowid,ascii_lower) VALUES(new.document_id,new.ascii_lower);
END;
PRAGMA user_version=2;
";

pub(super) fn preflight(directory: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        check_file(&directory.join(format!("{DATABASE}{suffix}")))?;
    }
    Ok(())
}

pub(super) fn open(directory: &Path, namespace: &str, roots: &str) -> Result<Connection> {
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
    let mut connection = Connection::open(&path)?;
    connection.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;",
    )?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.commit()?;
    } else if version != VERSION {
        return Err(Error::InvalidArgument(
            "index schema version does not match its filename".into(),
        ));
    }
    connection.execute("INSERT INTO meta(namespace,version,roots) VALUES(?1,?2,?3) ON CONFLICT(namespace) DO NOTHING", (namespace,VERSION,roots))?;
    let stored: (u32, String) = connection.query_row(
        "SELECT version,roots FROM meta WHERE namespace=?",
        [namespace],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if stored != (VERSION, roots.into()) {
        return Err(Error::InvalidArgument(
            "index namespace metadata is inconsistent".into(),
        ));
    }
    prune(directory)?;
    Ok(connection)
}

fn prune(directory: &Path) -> Result<()> {
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
