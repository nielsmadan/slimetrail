mod lock;
mod refresh;
mod schema;
mod store;

pub use schema::{DATABASE, VERSION};
pub use store::Document;

use crate::{Agent, Context, Diagnostic, Envelope, IndexStats, Result};
use rusqlite::Connection;
use std::{
    collections::BTreeSet,
    fs::File,
    time::{Duration, Instant},
};

const QUERY_WAIT: Duration = Duration::from_secs(5);
const REFRESH_WAIT: Duration = Duration::from_secs(3600);

struct Writer {
    connection: Connection,
    _lock: File,
}

pub struct IndexSession {
    connection: Connection,
    namespace: String,
    roots: String,
    context: Context,
    staging: lock::Staging,
    refreshed: BTreeSet<Agent>,
    stale: BTreeSet<Agent>,
    seen_seq: i64,
    _lease: lock::Lease,
}

impl IndexSession {
    pub fn open(ctx: &Context) -> Result<Self> {
        Self::open_with_timeout(ctx, Duration::from_secs(5))
    }

    pub fn open_with_timeout(ctx: &Context, timeout: Duration) -> Result<Self> {
        let (namespace, roots) = lock::namespace(ctx)?;
        let directory = lock::validate(ctx)?;
        schema::preflight(&directory)?;
        let staging = directory.join("staging");
        if std::fs::symlink_metadata(&staging).is_ok() {
            lock::directory(&staging)?;
        }
        let lease = lock::Lease::acquire(&directory, timeout, || {
            schema::prune(&directory)?;
            lock::cleanup(&staging)
        })?;
        let owned = lock::Staging::create(&staging)?;
        lock::sweep(&staging, &owned.path)?;
        let connection = schema::read(&directory, &namespace, &roots)?;
        let seen_seq = schema::refresh_seq(&connection)?;
        let mut context = ctx.clone();
        let mut reader_roots = serde_json::to_value(&ctx.roots)?;
        for value in reader_roots
            .as_object_mut()
            .ok_or_else(|| {
                crate::Error::InvalidArgument("Roots must serialize as an object".into())
            })?
            .values_mut()
        {
            if let Some(path) = value.as_str() {
                let path = std::path::Path::new(path);
                if path.is_relative() {
                    *value = serde_json::to_value(ctx.cwd.join(path))?;
                }
            }
        }
        context.roots = serde_json::from_value(reader_roots)?;
        context.index_dir = directory;
        Ok(Self {
            connection,
            namespace,
            roots,
            context,
            staging: owned,
            refreshed: BTreeSet::new(),
            stale: BTreeSet::new(),
            seen_seq,
            _lease: lease,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn refresh_under_lease(
        &mut self,
        agents: &BTreeSet<Agent>,
    ) -> Result<Envelope<IndexStats>> {
        refresh::run(self, agents, false, false)
    }

    pub fn refresh_for_query(&mut self, agents: &BTreeSet<Agent>) -> Result<Envelope<IndexStats>> {
        self.prepare(agents, QUERY_WAIT)
    }

    fn prepare(
        &mut self,
        agents: &BTreeSet<Agent>,
        wait: Duration,
    ) -> Result<Envelope<IndexStats>> {
        let started = Instant::now();
        let s0 = self.seen_seq;
        let Some(mut writer) = self.writer(wait, false)? else {
            self.reopen()?;
            self.pin()?;
            self.seen_seq = schema::refresh_seq(&self.connection)?;
            self.stale.extend(agents.iter().copied());
            let busy = Diagnostic {
                agent: None,
                source: None,
                code: "refresh_in_progress".into(),
                message: "Another slimetrail process is refreshing the index; results come from the last published index".into(),
            };
            return Ok(skipped(vec![busy], started));
        };
        let published = store::published(&writer.connection, &self.namespace, agents, s0)?;
        let result = if let Some(diagnostics) = published {
            self.refreshed.extend(agents.iter().copied());
            skipped(diagnostics, started)
        } else {
            let readers = crate::sources::registry_in(&self.context, &self.staging.path);
            refresh::write(self, &mut writer, agents, readers, false)?
        };
        self.publish(writer, true)?;
        self.seen_seq = schema::refresh_seq(&self.connection)?;
        Ok(result)
    }

    fn writer(&self, wait: Duration, notice: bool) -> Result<Option<Writer>> {
        if !self.connection.is_autocommit() {
            self.connection.execute_batch("ROLLBACK")?;
        }
        let Some(lock) = lock::writer(&self.context.index_dir, wait, notice)? else {
            return Ok(None);
        };
        let connection = schema::open(&self.context.index_dir, &self.namespace, &self.roots)?;
        Ok(Some(Writer {
            connection,
            _lock: lock,
        }))
    }

    fn publish(&mut self, writer: Writer, pin: bool) -> Result<()> {
        let _ = writer.connection.busy_timeout(Duration::ZERO);
        let _ = writer
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        self.reopen()?;
        if pin {
            self.pin()?;
        }
        drop(writer);
        Ok(())
    }

    fn reopen(&mut self) -> Result<()> {
        self.connection = schema::read(&self.context.index_dir, &self.namespace, &self.roots)?;
        Ok(())
    }

    fn pin(&self) -> Result<()> {
        self.connection.execute_batch("BEGIN")?;
        self.connection
            .query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
        Ok(())
    }
}

fn skipped(diagnostics: Vec<Diagnostic>, started: Instant) -> Envelope<IndexStats> {
    Envelope {
        version: 1,
        incomplete: !diagnostics.is_empty(),
        diagnostics,
        data: Some(IndexStats {
            discovered: 0,
            reused: 0,
            reparsed: 0,
            removed: 0,
            failed: 0,
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        }),
    }
}

pub fn refresh(
    ctx: &Context,
    agents: &BTreeSet<Agent>,
    verify: bool,
) -> Result<Envelope<IndexStats>> {
    refresh::run(&mut IndexSession::open(ctx)?, agents, false, verify)
}

pub(crate) fn refresh_noting(
    ctx: &Context,
    agents: &BTreeSet<Agent>,
    notice: bool,
    verify: bool,
) -> Result<Envelope<IndexStats>> {
    refresh::run(&mut IndexSession::open(ctx)?, agents, notice, verify)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiscoveryCache, sources};
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

    #[test]
    fn indexed_database_discovery_uses_owned_staging_and_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            roots: crate::resolve_roots(dir.path(), &BTreeMap::new()).unwrap(),
            index_dir: dir.path().join("index"),
            cwd: dir.path().into(),
            now_ms: 0,
            time_zone: "UTC".into(),
        };
        fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
        let db = Connection::open(&ctx.roots.opencode_db).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('one','/fixture','Title',1,2);").unwrap();
        let index = IndexSession::open(&ctx).unwrap();
        let reader = sources::registry_in(&index.context, &index.staging.path)
            .into_iter()
            .find(|s| s.agent() == Agent::Opencode)
            .unwrap();
        let discovery = reader
            .discover(
                &index.context,
                &DiscoveryCache {
                    by_source: BTreeMap::new(),
                    verify: false,
                },
            )
            .unwrap();
        let snapshot = discovery.sessions[0].snapshot.as_ref().unwrap();
        assert_eq!(
            snapshot.directory.path().parent(),
            Some(index.staging.path.as_path())
        );
        for entry in fs::read_dir(snapshot.directory.path()).unwrap() {
            assert_eq!(
                entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let path = snapshot.database.clone();
        let mut other = ctx;
        other.index_dir = dir.path().join("other-index");
        drop(IndexSession::open(&other).unwrap());
        assert!(path.exists());
        drop(discovery);
        assert!(!path.exists());
        assert_eq!(
            fs::read_dir(&index.staging.path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            ["owner.lock"]
        );
    }

    #[test]
    fn repeated_query_refreshes_see_changes_and_pin_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            roots: crate::resolve_roots(dir.path(), &BTreeMap::new()).unwrap(),
            index_dir: dir.path().join("index"),
            cwd: dir.path().into(),
            now_ms: 0,
            time_zone: "UTC".into(),
        };
        fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
        let db = Connection::open(&ctx.roots.opencode_db).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('one','/fixture','Title',1,2);").unwrap();
        let agents = BTreeSet::from([Agent::Opencode]);
        let mut index = IndexSession::open(&ctx).unwrap();
        let first = index.refresh_for_query(&agents).unwrap();
        assert!(first.data.unwrap().discovered > 0);
        assert!(!index.connection.is_autocommit());
        db.execute_batch("INSERT INTO session VALUES('two','/fixture','Other',3,4);")
            .unwrap();
        let second = index.refresh_for_query(&agents).unwrap();
        assert!(second.data.unwrap().discovered > 0);
        assert!(!index.connection.is_autocommit());
    }
}
