use rusqlite::Connection;
use slimetrail::{
    index::{self, DATABASE, IndexSession},
    *,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn context(base: &Path) -> Context {
    Context {
        roots: resolve_roots(base, &BTreeMap::new()).unwrap(),
        index_dir: base.join("index"),
        now_ms: 0,
        cwd: base.into(),
        time_zone: "UTC".into(),
        coverage: slimetrail::Coverage::Compact,
    }
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn worker(ctx: &Context, mode: &str) -> Worker {
    let config = ctx.cwd.join("worker-context.json");
    fs::write(&config, serde_json::to_vec(ctx).unwrap()).unwrap();
    Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "index_worker", "--ignored", "--nocapture"])
            .env("SLIMETRAIL_INDEX_TEST_CONTEXT", config)
            .env("SLIMETRAIL_INDEX_TEST_MODE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

fn ready(worker: &mut Worker) {
    let output = worker.0.stdout.take().unwrap();
    let mut reader = BufReader::new(output);
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "worker exited before coordination point"
        );
        if line.trim() == "INDEX_READY" {
            worker.0.stdout = Some(reader.into_inner());
            break;
        }
    }
}

fn options() -> SearchOptions {
    SearchOptions {
        filter: Filter {
            agents: BTreeSet::from([Agent::Claude]),
            cwd: "/".into(),
            scope: Scope::All,
            app: None,
            days: None,
            kinds: BTreeSet::new(),
        },
        query: "content".into(),
        regex: false,
        case_sensitive: false,
        sort: Sort::Score,
        max_results: 10,
        max_snippets: 1,
        context_chars: 8,
    }
}

fn session(ctx: &Context, id: &str) {
    fs::create_dir_all(&ctx.roots.claude_projects).unwrap();
    fs::write(
        ctx.roots.claude_projects.join(format!("{id}.jsonl")),
        format!("{{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":\"committed content\"}}\n"),
    )
    .unwrap();
}

fn hits(result: &Envelope<SearchResult>) -> Vec<String> {
    result
        .data
        .as_ref()
        .unwrap()
        .results
        .iter()
        .map(|hit| hit.session.key.id.clone())
        .collect()
}

fn signal_and_wait() {
    println!("INDEX_READY");
    std::io::stdout().flush().unwrap();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
}

#[test]
#[ignore = "Subprocess worker invoked explicitly by the concurrency tests"]
fn index_worker() {
    let Ok(config) = std::env::var("SLIMETRAIL_INDEX_TEST_CONTEXT") else {
        return;
    };
    let ctx: Context = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
    match std::env::var("SLIMETRAIL_INDEX_TEST_MODE")
        .unwrap()
        .as_str()
    {
        "transaction" => {
            let mut index = IndexSession::open(&ctx).unwrap();
            index
                .refresh_under_lease(&BTreeSet::from([Agent::Claude]))
                .unwrap();
            let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
            db.execute_batch("PRAGMA foreign_keys=ON; UPDATE refresh_runs SET generation=generation+1,status='incomplete'; BEGIN IMMEDIATE; DELETE FROM sessions;").unwrap();
            signal_and_wait();
            db.execute_batch("ROLLBACK").unwrap();
        }
        "writer" => {
            let _index = IndexSession::open(&ctx).unwrap();
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(ctx.index_dir.join("writer.lock"))
                .unwrap();
            lock.try_lock().unwrap();
            let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
            db.execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE; DELETE FROM sessions;")
                .unwrap();
            signal_and_wait();
            db.execute_batch("ROLLBACK").unwrap();
        }
        "search" => {
            signal_and_wait();
            let result = api::search(&ctx, &options()).unwrap();
            assert!(!result.incomplete, "{:?}", result.diagnostics);
            assert_eq!(hits(&result), ["one"]);
        }
        _ => {
            let _index = IndexSession::open(&ctx).unwrap();
            signal_and_wait();
        }
    }
}

fn release(worker: &mut Worker) {
    worker
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
}

fn owned_dirs(ctx: &Context, pid: u32) -> Vec<PathBuf> {
    fs::read_dir(ctx.index_dir.join("staging"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("proc-{pid}-"))
        })
        .collect()
}

#[test]
fn separate_process_sessions_share_the_lifecycle_lease() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let mut child = worker(&ctx, "lock");
    ready(&mut child);
    drop(IndexSession::open_with_timeout(&ctx, Duration::ZERO).unwrap());
    release(&mut child);
    assert!(child.0.wait().unwrap().success());
    let legacy = fs::File::open(ctx.index_dir.join("lifecycle.lock")).unwrap();
    legacy.try_lock().unwrap();
    assert!(matches!(
        IndexSession::open_with_timeout(&ctx, Duration::ZERO),
        Err(Error::IndexBusy)
    ));
    drop(legacy);
    drop(IndexSession::open_with_timeout(&ctx, Duration::ZERO).unwrap());
}

#[test]
fn query_during_another_write_answers_from_the_published_index() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    session(&ctx, "one");
    index::refresh(&ctx, &BTreeSet::from([Agent::Claude]), false).unwrap();
    let mut child = worker(&ctx, "writer");
    ready(&mut child);
    let started = Instant::now();
    let result = api::search(&ctx, &options()).unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_secs(5), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(7), "{elapsed:?}");
    assert!(result.incomplete);
    assert_eq!(
        result
            .diagnostics
            .iter()
            .map(|d| (d.code.as_str(), d.agent, d.source.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("refresh_in_progress", None, None),
            ("search_coverage", None, None)
        ]
    );
    assert_eq!(hits(&result), ["one"]);
    release(&mut child);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn concurrent_searches_on_a_built_index_both_complete() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    session(&ctx, "one");
    index::refresh(&ctx, &BTreeSet::from([Agent::Claude]), false).unwrap();
    let mut first = worker(&ctx, "search");
    let mut second = worker(&ctx, "search");
    ready(&mut first);
    ready(&mut second);
    release(&mut first);
    release(&mut second);
    assert!(first.0.wait().unwrap().success());
    assert!(second.0.wait().unwrap().success());
}

#[test]
fn cold_searches_racing_on_an_empty_index_both_complete() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    session(&ctx, "one");
    let mut first = worker(&ctx, "search");
    let mut second = worker(&ctx, "search");
    ready(&mut first);
    ready(&mut second);
    assert!(!ctx.index_dir.exists());
    release(&mut first);
    release(&mut second);
    assert!(first.0.wait().unwrap().success());
    assert!(second.0.wait().unwrap().success());
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn dead_owner_staging_is_swept_and_live_staging_survives() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let mut dead = worker(&ctx, "lock");
    ready(&mut dead);
    let dead_pid = dead.0.id();
    assert_eq!(owned_dirs(&ctx, dead_pid).len(), 1);
    dead.0.kill().unwrap();
    dead.0.wait().unwrap();
    assert_eq!(owned_dirs(&ctx, dead_pid).len(), 1);
    let mut live = worker(&ctx, "lock");
    ready(&mut live);
    let live_pid = live.0.id();
    assert_eq!(owned_dirs(&ctx, dead_pid), [] as [PathBuf; 0]);
    let pending = ctx.index_dir.join("staging/tmp-concurrent");
    fs::create_dir(&pending).unwrap();
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o700)).unwrap();
    let unowned = ctx.index_dir.join("staging/proc-1-unowned");
    fs::create_dir(&unowned).unwrap();
    fs::set_permissions(&unowned, fs::Permissions::from_mode(0o700)).unwrap();
    drop(IndexSession::open(&ctx).unwrap());
    assert_eq!(owned_dirs(&ctx, live_pid).len(), 1);
    assert!(pending.exists());
    assert!(unowned.exists());
    release(&mut live);
    assert!(live.0.wait().unwrap().success());
    assert_eq!(owned_dirs(&ctx, live_pid), [] as [PathBuf; 0]);
    drop(IndexSession::open(&ctx).unwrap());
    assert!(!pending.exists());
    assert!(unowned.exists());
}

#[test]
fn killed_transaction_recovers_and_requires_refresh_before_queries() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    fs::create_dir_all(&ctx.roots.claude_projects).unwrap();
    fs::write(
        ctx.roots.claude_projects.join("one.jsonl"),
        "{\"type\":\"user\",\"sessionId\":\"one\",\"message\":\"committed content\"}\n",
    )
    .unwrap();
    let mut child = worker(&ctx, "transaction");
    ready(&mut child);
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let mut index = IndexSession::open(&ctx).unwrap();
    let key = SessionKey {
        agent: Agent::Claude,
        id: "one".into(),
    };
    assert!(index.session(&key).is_err());
    let result = index
        .refresh_under_lease(&BTreeSet::from([Agent::Claude]))
        .unwrap();
    assert!(!result.incomplete);
    assert_eq!(result.data.unwrap().reused, 1);
    let mut entries = Vec::new();
    index
        .visit_entries(&key, &mut |entry| {
            entries.push(entry.text);
            Ok(())
        })
        .unwrap();
    assert_eq!(entries, ["committed content"]);
    index.verify_integrity().unwrap();
}
