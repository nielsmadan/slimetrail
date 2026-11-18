mod lock;
mod refresh;
mod schema;
mod store;

pub use schema::{DATABASE, VERSION};
pub use store::Document;

use crate::{Agent, Context, Envelope, IndexStats, Result};
use rusqlite::Connection;
use std::{collections::BTreeSet, path::PathBuf, time::Duration};

pub struct IndexSession {
    connection: Connection,
    namespace: String,
    context: Context,
    staging: PathBuf,
    refreshed: BTreeSet<Agent>,
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
        let lease = lock::Lease::acquire(&directory, timeout)?;
        lock::cleanup(&staging)?;
        let connection = schema::open(&directory, &namespace, &roots)?;
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
            context,
            staging,
            refreshed: BTreeSet::new(),
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
        refresh::run(self, agents)
    }
}

pub fn refresh(ctx: &Context, agents: &BTreeSet<Agent>) -> Result<Envelope<IndexStats>> {
    IndexSession::open(ctx)?.refresh_under_lease(agents)
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
        let reader = sources::registry_in(&index.context, &index.staging)
            .into_iter()
            .find(|s| s.agent() == Agent::Opencode)
            .unwrap();
        let discovery = reader
            .discover(
                &index.context,
                &DiscoveryCache {
                    by_source: BTreeMap::new(),
                },
            )
            .unwrap();
        let snapshot = discovery.sessions[0].snapshot.as_ref().unwrap();
        assert_eq!(
            snapshot.directory.path().parent(),
            Some(index.staging.as_path())
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
        assert_eq!(fs::read_dir(&index.staging).unwrap().count(), 0);
    }
}
