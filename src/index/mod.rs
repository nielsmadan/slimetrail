pub mod coverage;
pub mod estimate;
mod lock;
mod refresh;
mod schema;
mod store;

#[cfg(feature = "test-hooks")]
pub use refresh::discoveries;
pub use schema::{DATABASE, VERSION};
pub use store::Document;

use crate::{
    Agent, Context, Coverage, Diagnostic, Envelope, IndexStats, Observer, RefreshEvent,
    RefreshMode, RefreshOptions, Result, sources::Source,
};
use rusqlite::Connection;
use std::{
    collections::BTreeSet,
    fs::File,
    time::{Duration, Instant},
};

struct Writer {
    connection: Connection,
    coverage: Coverage,
    stored: Option<Coverage>,
    pending: Option<Coverage>,
    locked: bool,
    _lock: File,
}

impl Writer {
    fn persist_coverage(&self) -> Result<()> {
        if self.stored != Some(self.coverage) {
            schema::set_coverage(&self.connection, self.coverage)?;
        }
        Ok(())
    }
}

pub struct IndexSession {
    connection: Connection,
    namespace: String,
    roots: String,
    context: Context,
    staging: lock::Staging,
    refreshed: BTreeSet<Agent>,
    stale: BTreeSet<Agent>,
    _lease: lock::Lease,
}

pub(crate) struct Refreshed {
    pub(crate) status: Envelope<IndexStats>,
    pub(crate) available: bool,
}

impl Refreshed {
    const fn available(status: Envelope<IndexStats>) -> Self {
        Self {
            status,
            available: true,
        }
    }
}

impl IndexSession {
    pub fn open(ctx: &Context) -> Result<Self> {
        Self::open_with_timeout(ctx, RefreshOptions::QUERY.wait)
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
            _lease: lease,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn coverage(&self) -> Result<Coverage> {
        Ok(schema::effective_coverage(&self.connection, self.context.coverage)?.coverage)
    }

    pub fn refresh(
        &mut self,
        agents: &BTreeSet<Agent>,
        options: &RefreshOptions<'_>,
    ) -> Result<Envelope<IndexStats>> {
        Ok(self.refresh_gated(agents, options)?.status)
    }

    pub(crate) fn refresh_gated(
        &mut self,
        agents: &BTreeSet<Agent>,
        options: &RefreshOptions<'_>,
    ) -> Result<Refreshed> {
        let readers = crate::sources::registry_in(&self.context, &self.staging.path);
        self.refresh_from(agents, options, readers)
    }

    fn refresh_from(
        &mut self,
        agents: &BTreeSet<Agent>,
        options: &RefreshOptions<'_>,
        readers: Vec<Box<dyn Source>>,
    ) -> Result<Refreshed> {
        self.unpin()?;
        match options.mode {
            RefreshMode::Query => {
                let since = self.current_seq()?;
                self.prepare_since(agents, options, readers, since)
            }
            RefreshMode::Full => refresh::full(self, agents, options, readers),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn estimate(&self, agents: &BTreeSet<Agent>) -> Result<crate::JobEstimate> {
        let readers = crate::sources::registry_in(&self.context, &self.staging.path);
        let planned = refresh::plan(self, &self.connection, agents, readers, RefreshMode::Query)?;
        let effective = schema::effective_coverage(&self.connection, self.context.coverage)?;
        Ok(crate::JobEstimate {
            agents: planned
                .into_iter()
                .map(|p| p.estimate)
                .filter(|e| e.sessions > 0)
                .collect(),
            coverage: effective.coverage,
            coverage_locked: effective.locked,
        })
    }

    fn current_seq(&mut self) -> Result<i64> {
        self.reopen()?;
        schema::refresh_seq(&self.connection)
    }

    fn prepare_since(
        &mut self,
        agents: &BTreeSet<Agent>,
        options: &RefreshOptions<'_>,
        readers: Vec<Box<dyn Source>>,
        since: i64,
    ) -> Result<Refreshed> {
        let started = Instant::now();
        let writer = self.writer(options.wait, options.observer)?;
        let rebuilding = writer.as_ref().is_some_and(|w| w.pending.is_some());
        let Some(mut writer) = writer.filter(|_| !rebuilding) else {
            self.reopen()?;
            self.pin()?;
            for &agent in agents {
                self.mark_stale(agent);
            }
            let pending = schema::coverage(&self.connection)?.is_some_and(|(_, p)| p.is_some());
            let busy = if rebuilding || pending {
                rebuild_pending()
            } else {
                refresh_in_progress()
            };
            return Ok(Refreshed::available(skipped(vec![busy], started)));
        };
        writer.persist_coverage()?;
        let published = store::published(&writer.connection, &self.namespace, agents, since)?;
        let result = if let Some(diagnostics) = published {
            for &agent in agents {
                self.mark_refreshed(agent);
            }
            Refreshed::available(skipped(diagnostics, started))
        } else {
            refresh::write(self, &mut writer, agents, readers, options, Instant::now())?
        };
        self.publish(writer, true)?;
        Ok(result)
    }

    fn lock_writer(
        &self,
        wait: Duration,
        observer: Option<&dyn Observer>,
    ) -> Result<Option<(File, Connection)>> {
        let waiting = || {
            if let Some(observer) = observer {
                observer.observe(RefreshEvent::Waiting);
            }
        };
        let Some(lock) = lock::writer(&self.context.index_dir, wait, waiting)? else {
            return Ok(None);
        };
        let connection = schema::open(
            &self.context.index_dir,
            &self.namespace,
            &self.roots,
            self.context.coverage,
        )?;
        Ok(Some((lock, connection)))
    }

    fn writer(&self, wait: Duration, observer: Option<&dyn Observer>) -> Result<Option<Writer>> {
        let Some((lock, connection)) = self.lock_writer(wait, observer)? else {
            return Ok(None);
        };
        let effective = schema::effective_coverage(&connection, self.context.coverage)?;
        Ok(Some(Writer {
            connection,
            coverage: effective.coverage,
            stored: effective.stored,
            pending: effective.pending,
            locked: effective.locked,
            _lock: lock,
        }))
    }

    pub fn verify_integrity(&self) -> Result<()> {
        let (_lock, connection) = self
            .lock_writer(RefreshOptions::FULL.wait, None)?
            .ok_or(crate::Error::IndexBusy)?;
        store::verify_integrity(&connection)
    }

    fn mark_refreshing(&mut self, agent: Agent) {
        self.refreshed.remove(&agent);
    }

    fn mark_refreshed(&mut self, agent: Agent) {
        self.refreshed.insert(agent);
    }

    fn mark_stale(&mut self, agent: Agent) {
        self.refreshed.remove(&agent);
        self.stale.insert(agent);
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

    fn unpin(&self) -> Result<()> {
        if !self.connection.is_autocommit() {
            self.connection.execute_batch("ROLLBACK")?;
        }
        Ok(())
    }
}

fn refresh_in_progress() -> Diagnostic {
    Diagnostic {
        agent: None,
        source: None,
        code: "refresh_in_progress".into(),
        message: "Another slimetrail process is refreshing the index; results come from the last published index".into(),
    }
}

fn rebuild_pending() -> Diagnostic {
    Diagnostic {
        agent: None,
        source: None,
        code: "coverage_rebuild_pending".into(),
        message: "A search coverage rebuild is unfinished; search results may be missing matches until `slimetrail index` completes it".into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiscoveryCache, Progress, sources, testing::context};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    fn opencode_fixture(base: &Path) -> (Context, Connection) {
        let ctx = context(base);
        fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
        let db = Connection::open(&ctx.roots.opencode_db).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('one','/fixture','Title',1,2);").unwrap();
        (ctx, db)
    }

    #[test]
    fn indexed_database_discovery_uses_owned_staging_and_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _db) = opencode_fixture(dir.path());
        let index = IndexSession::open(&ctx).unwrap();
        let reader = sources::registry_in(&index.context, &index.staging.path)
            .into_iter()
            .find(|s| s.agent() == Agent::Opencode)
            .unwrap();
        let discovery = reader
            .discover(&index.context, &DiscoveryCache::default())
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
        let (ctx, db) = opencode_fixture(dir.path());
        let agents = BTreeSet::from([Agent::Opencode]);
        let mut index = IndexSession::open(&ctx).unwrap();
        let first = index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert_eq!(first.data.unwrap().discovered, 1);
        assert!(!index.connection.is_autocommit());
        db.execute_batch("INSERT INTO session VALUES('two','/fixture','Other',3,4);")
            .unwrap();
        let second = index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert_eq!(second.data.unwrap().discovered, 2);
        assert!(!index.connection.is_autocommit());
    }

    #[test]
    fn query_reuses_runs_published_by_a_refresh_that_started_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, db) = opencode_fixture(dir.path());
        let agents = BTreeSet::from([Agent::Opencode]);
        let mut waiting = IndexSession::open(&ctx).unwrap();
        waiting.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        db.execute_batch("INSERT INTO session VALUES('two','/fixture','Other',3,4);")
            .unwrap();
        let since = waiting.current_seq().unwrap();
        let mut other = IndexSession::open(&ctx).unwrap();
        assert_eq!(
            other
                .refresh(&agents, &RefreshOptions::QUERY)
                .unwrap()
                .data
                .unwrap()
                .discovered,
            2
        );
        let readers = sources::registry_in(&waiting.context, &waiting.staging.path);
        let coalesced = waiting
            .prepare_since(&agents, &RefreshOptions::QUERY, readers, since)
            .unwrap()
            .status;
        assert!(!coalesced.incomplete, "{:?}", coalesced.diagnostics);
        assert_eq!(coalesced.data.unwrap().discovered, 0);
        let two = crate::SessionKey {
            agent: Agent::Opencode,
            id: "two".into(),
        };
        assert_eq!(waiting.session(&two).unwrap().unwrap().key, two);
    }

    #[derive(Default)]
    struct Recorder(std::cell::RefCell<Vec<RefreshEvent>>);

    impl Observer for Recorder {
        fn observe(&self, event: RefreshEvent) {
            self.0.borrow_mut().push(event);
        }
    }

    #[test]
    fn observer_hears_progress_for_each_session_and_a_busy_writer() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _db) = opencode_fixture(dir.path());
        let agents = BTreeSet::from([Agent::Opencode]);
        let recorder = Recorder::default();
        let observed = RefreshOptions {
            observer: Some(&recorder),
            ..RefreshOptions::QUERY
        };
        let mut index = IndexSession::open(&ctx).unwrap();
        index.refresh(&agents, &observed).unwrap();
        let events = recorder.0.take();
        let [
            RefreshEvent::Started(start),
            RefreshEvent::Advanced(step),
            RefreshEvent::Finished(end),
        ] = events[..]
        else {
            panic!("unexpected events {events:?}");
        };
        assert_eq!(
            (start.sessions, start.total_sessions, start.bytes),
            (0, 1, 0)
        );
        assert!(start.total_bytes > 0);
        let done = Progress {
            sessions: 1,
            bytes: start.total_bytes,
            ..start
        };
        assert_eq!((step, end), (done, done));
        let lock = File::options()
            .read(true)
            .write(true)
            .open(ctx.index_dir.join("writer.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let immediate = RefreshOptions {
            wait: Duration::ZERO,
            ..observed
        };
        index.refresh(&agents, &immediate).unwrap();
        assert_eq!(recorder.0.take(), [RefreshEvent::Waiting]);
    }

    #[derive(Default)]
    struct Timed(std::cell::RefCell<Vec<(Instant, RefreshEvent)>>);

    impl Observer for Timed {
        fn observe(&self, event: RefreshEvent) {
            self.0.borrow_mut().push((Instant::now(), event));
        }
    }

    #[test]
    fn coverage_rebuild_reports_progress_and_counts_toward_the_refresh_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/many.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines: String = (0..25_000)
            .map(|i| {
                serde_json::json!({"type":"user","sessionId":"many","message":format!("message {i}")})
                    .to_string()
                    + "\n"
            })
            .collect();
        fs::write(&path, lines).unwrap();
        let agents = BTreeSet::from([Agent::Claude]);
        IndexSession::open(&ctx)
            .unwrap()
            .refresh(&agents, &RefreshOptions::QUERY)
            .unwrap();
        let full = ctx.with_coverage(Coverage::Full, false);
        let timed = Timed::default();
        let observed = RefreshOptions {
            observer: Some(&timed),
            ..RefreshOptions::FULL
        };
        let mut index = IndexSession::open(&full).unwrap();
        let result = index.refresh(&agents, &observed).unwrap();
        let events = timed.0.take();
        let rebuilt: Vec<_> = events
            .iter()
            .filter_map(|(_, event)| match event {
                RefreshEvent::Rebuilding(rebuild) => Some((rebuild.entries, rebuild.total_entries)),
                _ => None,
            })
            .collect();
        assert_eq!(
            rebuilt,
            [
                (0, 25_000),
                (10_000, 25_000),
                (20_000, 25_000),
                (25_000, 25_000)
            ]
        );
        let started = events
            .iter()
            .find(|(_, event)| matches!(event, RefreshEvent::Started(_)))
            .unwrap()
            .0;
        let rebuild_ms = started.duration_since(events[0].0).as_millis();
        assert!(rebuild_ms > 0);
        let stats = result.data.unwrap();
        assert_eq!(stats.reused, 1);
        assert!(
            u128::from(stats.elapsed_ms) >= rebuild_ms,
            "{stats:?} {rebuild_ms}"
        );
        fs::remove_file(&path).unwrap();
        let removed = index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        assert_eq!(removed.data.unwrap().removed, 1);
        index.verify_integrity().unwrap();
        let fts: i64 = Connection::open(full.index_dir.join(DATABASE))
            .unwrap()
            .query_row("SELECT count(*) FROM search_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, 0);
    }

    #[test]
    fn busy_writer_during_a_coverage_rebuild_reports_the_pending_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _db) = opencode_fixture(dir.path());
        let agents = BTreeSet::from([Agent::Opencode]);
        let mut index = IndexSession::open(&ctx).unwrap();
        index.refresh(&agents, &RefreshOptions::QUERY).unwrap();
        Connection::open(ctx.index_dir.join(DATABASE))
            .unwrap()
            .execute("UPDATE settings SET coverage_pending='full'", [])
            .unwrap();
        let lock = File::options()
            .read(true)
            .write(true)
            .open(ctx.index_dir.join("writer.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let immediate = RefreshOptions {
            wait: Duration::ZERO,
            ..RefreshOptions::QUERY
        };
        let result = index.refresh(&agents, &immediate).unwrap();
        assert!(result.incomplete);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>(),
            ["coverage_rebuild_pending"]
        );
    }
}
