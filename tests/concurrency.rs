use rusqlite::Connection;
use slimetrail::{
    index::{DATABASE, IndexSession},
    *,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

fn context(base: &Path) -> Context {
    Context {
        roots: resolve_roots(base, &BTreeMap::new()).unwrap(),
        index_dir: base.join("index"),
        now_ms: 0,
        cwd: base.into(),
        time_zone: "UTC".into(),
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

#[test]
#[ignore = "Subprocess worker invoked explicitly by the concurrency tests"]
fn index_worker() {
    let Ok(config) = std::env::var("SLIMETRAIL_INDEX_TEST_CONTEXT") else {
        return;
    };
    let ctx: Context = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
    let mut index = IndexSession::open(&ctx).unwrap();
    if std::env::var("SLIMETRAIL_INDEX_TEST_MODE").unwrap() == "transaction" {
        index
            .refresh_under_lease(&BTreeSet::from([Agent::Claude]))
            .unwrap();
        let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON; UPDATE refresh_runs SET generation=generation+1,status='incomplete'; BEGIN IMMEDIATE; DELETE FROM sessions;").unwrap();
        println!("INDEX_READY");
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        db.execute_batch("ROLLBACK").unwrap();
    } else {
        println!("INDEX_READY");
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
    }
}

#[test]
fn separate_process_holds_nonversioned_lifecycle_lease() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let mut child = worker(&ctx, "lock");
    ready(&mut child);
    assert!(matches!(
        IndexSession::open_with_timeout(&ctx, Duration::ZERO),
        Err(Error::IndexBusy)
    ));
    child
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert!(child.0.wait().unwrap().success());
    drop(IndexSession::open_with_timeout(&ctx, Duration::ZERO).unwrap());
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
