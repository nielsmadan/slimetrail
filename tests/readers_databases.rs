use rusqlite::{Connection, params};
use serde_json::{Value, json};
use slimetrail::{
    sources::{Source, source_for, sqlite},
    *,
};
use std::{
    collections::BTreeMap,
    fs,
    hash::{Hash, Hasher},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

fn context(base: &Path) -> Context {
    Context {
        roots: resolve_roots(
            base,
            &BTreeMap::from([
                (
                    "CODEX_HOME".into(),
                    base.join("codex").to_string_lossy().into_owned(),
                ),
                (
                    "XDG_DATA_HOME".into(),
                    base.join("data").to_string_lossy().into_owned(),
                ),
            ]),
        )
        .unwrap(),
        index_dir: base.join("index"),
        now_ms: 0,
        cwd: "/fixture/project".into(),
        time_zone: "UTC".into(),
        coverage: slimetrail::Coverage::Compact,
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn cursor(ctx: &Context, id: &str) -> (PathBuf, PathBuf) {
    let transcript = ctx
        .roots
        .cursor_projects
        .join(format!("lossy-slug/agent-transcripts/{id}/{id}.jsonl"));
    write(&transcript, include_str!("fixtures/jsonl/cursor.jsonl"));
    let meta = ctx.roots.cursor_chats.join(format!("hash/{id}/meta.json"));
    write(&meta, &json!({"cwd":"/fixture/project", "title":"Cursor title", "createdAtMs":1_786_356_360_000_i64,"updatedAtMs":1_786_356_480_000_i64}).to_string());
    (transcript, meta)
}

fn cursor_store(meta: &Path, value: &Value) -> Connection {
    let connection = Connection::open(meta.with_file_name("store.db")).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
    connection
        .execute("INSERT INTO meta VALUES('0', ?)", [hex(value)])
        .unwrap();
    connection
}

fn hex(value: &Value) -> String {
    value
        .to_string()
        .bytes()
        .fold(String::new(), |mut hex, byte| {
            use std::fmt::Write;
            write!(hex, "{byte:02x}").unwrap();
            hex
        })
}

fn opencode(ctx: &Context) -> Connection {
    fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
    let connection = Connection::open(&ctx.roots.opencode_db).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE session(id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER, parent_id TEXT, time_archived INTEGER);
        CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, time_created INTEGER, data TEXT);
        INSERT INTO session VALUES('main', '/fixture/project', 'OpenCode title', 1786356000000, 1786356180000, NULL, NULL);
        INSERT INTO message VALUES('m1', 'main', 1786356000000, '{\"role\":\"user\"}');
        INSERT INTO message VALUES('m2', 'main', 1786356120000, '{\"role\":\"assistant\"}');").unwrap();
    for (id, message, time, part) in [
        (
            "p1",
            "m1",
            1_786_356_060_000_i64,
            json!({"type":"text", "text":"needle opencode café 🦀"}),
        ),
        (
            "p2",
            "m2",
            1_786_356_120_000_i64,
            json!({"type":"reasoning", "text":"Reason carefully"}),
        ),
        (
            "p3",
            "m2",
            1_786_356_180_000_i64,
            json!({"type":"tool", "tool":"shell", "state":{"input":{"command":"echo needle"},"output":"needle"}}),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO part VALUES(?,?,?,?)",
                params![id, message, time, part.to_string()],
            )
            .unwrap();
    }
    connection
}

const fn empty_cache() -> DiscoveryCache {
    DiscoveryCache {
        by_source: BTreeMap::new(),
        verify: false,
    }
}

fn cached(discovery: &Discovery) -> DiscoveryCache {
    DiscoveryCache {
        by_source: discovery
            .sessions
            .iter()
            .map(|r| {
                (
                    r.source.clone(),
                    CachedSource {
                        key: r.key.clone(),
                        source: r.source.clone(),
                        dependencies: r.dependencies.clone(),
                        fingerprint: r.fingerprint.clone(),
                        state: None,
                    },
                )
            })
            .collect(),
        verify: false,
    }
}

fn discover(source: &dyn Source, ctx: &Context) -> Discovery {
    source.discover(ctx, &empty_cache()).unwrap()
}

fn parse(source: &dyn Source, raw: &RawSession) -> (ParsedSession, Vec<Entry>) {
    let mut entries = Vec::new();
    let parsed = source
        .parse(raw, &mut |e| {
            entries.push(e);
            Ok(())
        })
        .unwrap();
    (parsed, entries)
}

fn files(database: &Path) -> Vec<Option<(u64, Vec<u8>)>> {
    sqlite::dependencies(database)
        .into_iter()
        .map(|p| {
            fs::read(p).ok().map(|bytes| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hash);
                (hash.finish(), bytes)
            })
        })
        .collect()
}

#[test]
fn checked_in_reference_goldens_cover_cursor_and_opencode() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    cursor(&ctx, "main");
    let _writer = opencode(&ctx);
    for (agent, golden) in [
        (
            Agent::Cursor,
            include_str!("fixtures/jsonl/cursor.golden.json"),
        ),
        (
            Agent::Opencode,
            include_str!("fixtures/jsonl/opencode.golden.json"),
        ),
    ] {
        let source = source_for(&ctx, agent).unwrap();
        let discovery = discover(source.as_ref(), &ctx);
        assert_eq!(discovery.complete_agents, [agent].into());
        let (parsed, entries) = parse(source.as_ref(), &discovery.sessions[0]);
        assert!(parsed.complete, "{:?}", parsed.diagnostics);
        assert_eq!(parsed.session.cwd, Some("/fixture/project".into()));
        assert_eq!(parsed.session.title_origin, Some(TitleOrigin::Native));
        assert!(entries.iter().all(|e| e.session == parsed.session.key));
        assert!(entries.iter().all(|e| e.searchable == Searchable::Text));
        let actual: Vec<_> = entries
            .iter()
            .map(|e| json!({"role":e.role,"kind":e.kind,"text":e.text,"timestamp":e.timestamp}))
            .collect();
        assert_eq!(actual, serde_json::from_str::<Vec<Value>>(golden).unwrap());
        assert_eq!(
            parse(source.as_ref(), &discovery.sessions[0]),
            (parsed, entries)
        );
    }
}

#[test]
fn wal_snapshot_preserves_sources_and_restricts_private_files() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let _writer = opencode(&ctx);
    let original = files(&ctx.roots.opencode_db);
    let (snapshot, _) = sqlite::snapshot(&ctx.roots.opencode_db).unwrap();
    assert_eq!(
        fs::metadata(snapshot.directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        sqlite::dependencies(&snapshot.database)[2]
            .try_exists()
            .is_ok_and(|exists| !exists)
    );
    let private = sqlite::open(&snapshot).unwrap();
    assert_eq!(
        private
            .query_row("SELECT count(*) FROM part", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        private
            .query_row("PRAGMA query_only", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(private.execute("DELETE FROM part", []).is_err());
    for entry in fs::read_dir(snapshot.directory.path()).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(private);
    let private_path = snapshot.directory.path().to_path_buf();
    drop(snapshot);
    assert_eq!(files(&ctx.roots.opencode_db), original);
    assert!(!private_path.exists());
}

#[test]
fn snapshot_copies_are_private_and_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let _writer = opencode(&ctx);
    let sources = sqlite::dependencies(&ctx.roots.opencode_db);
    assert!(fs::metadata(&sources[1]).unwrap().len() > 0);
    let (snapshot, _) = sqlite::snapshot(&ctx.roots.opencode_db).unwrap();
    for (source, copy) in sources
        .iter()
        .zip(sqlite::dependencies(&snapshot.database))
        .take(2)
    {
        assert_eq!(fs::read(&copy).unwrap(), fs::read(source).unwrap());
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn snapshots_support_wal_without_source_shm_and_readonly_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let _writer = opencode(&ctx);
    let dest = dir.path().join("readonly/source.db");
    fs::create_dir(dest.parent().unwrap()).unwrap();
    for (source, target) in sqlite::dependencies(&ctx.roots.opencode_db)
        .iter()
        .zip(sqlite::dependencies(&dest))
        .take(2)
    {
        fs::copy(source, &target).unwrap();
        fs::set_permissions(target, fs::Permissions::from_mode(0o400)).unwrap();
    }
    let before = files(&dest);
    let (snapshot, _) = sqlite::snapshot(&dest).unwrap();
    let private = sqlite::open(&snapshot).unwrap();
    assert_eq!(
        private
            .query_row("SELECT count(*) FROM part", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    drop(private);
    drop(snapshot);
    assert_eq!(files(&dest), before);
}

#[test]
fn clean_rollback_database_creates_no_source_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE events(value); INSERT INTO events VALUES(42);")
        .unwrap();
    let before = files(&path);
    let (snapshot, _) = sqlite::snapshot(&path).unwrap();
    assert_eq!(
        sqlite::open(&snapshot)
            .unwrap()
            .query_row("SELECT value FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        42
    );
    drop(snapshot);
    assert_eq!(files(&path), before);
}

#[test]
fn active_journals_and_symlink_dependencies_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE events(value);")
        .unwrap();
    let journal = sqlite::dependencies(&path)[3].clone();
    fs::write(&journal, b"unknown recovery journal").unwrap();
    let before = files(&path);
    assert!(sqlite::snapshot(&path).is_err());
    assert_eq!(files(&path), before);
    fs::remove_file(journal).unwrap();
    let linked = dir.path().join("alias.db");
    symlink(&path, &linked).unwrap();
    assert!(sqlite::snapshot(&linked).is_err());
    let wal = sqlite::dependencies(&path)[1].clone();
    symlink(&path, wal).unwrap();
    assert!(sqlite::snapshot(&path).is_err());
}

#[test]
fn opencode_reuses_cache_and_shares_one_changed_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let writer = opencode(&ctx);
    writer.execute_batch("INSERT INTO session VALUES('child', '/fixture/project', 'Child', 1786356000000, 1786356180000, 'main', 1786356180000);
        INSERT INTO message VALUES('m3', 'child', 1786356000000, '{\"role\":\"user\"}');
        INSERT INTO part VALUES('p4', 'm3', 1786356060000, '{\"type\":\"text\",\"text\":\"child prompt\"}');").unwrap();
    let source = source_for(&ctx, Agent::Opencode).unwrap();
    let initial = discover(source.as_ref(), &ctx);
    assert_eq!(initial.sessions.len(), 2);
    assert!(Arc::ptr_eq(
        initial.sessions[0].snapshot.as_ref().unwrap(),
        initial.sessions[1].snapshot.as_ref().unwrap()
    ));
    let cache = cached(&initial);
    let unchanged = source.discover(&ctx, &cache).unwrap();
    assert!(unchanged.sessions.iter().all(|r| r.snapshot.is_none()));
    let child = initial
        .sessions
        .iter()
        .find(|r| r.key.id == "child")
        .unwrap();
    let (child, entries) = parse(source.as_ref(), child);
    assert_eq!(child.session.parent.unwrap().id, "main");
    assert!(child.session.archived);
    assert!(entries[0].automated);
    writer
        .execute(
            "UPDATE part SET data=? WHERE id='p1'",
            [json!({"type":"text","text":"updated opencode café 🦀"}).to_string()],
        )
        .unwrap();
    let changed = source.discover(&ctx, &cache).unwrap();
    assert!(changed.sessions.iter().all(|r| r.snapshot.is_some()));
    assert!(
        changed
            .sessions
            .iter()
            .all(|r| r.fingerprint != cache.by_source[&r.source].fingerprint)
    );
    let main = changed
        .sessions
        .iter()
        .find(|r| r.key.id == "main")
        .unwrap();
    assert_eq!(
        parse(source.as_ref(), main).1[0].text,
        "updated opencode café 🦀"
    );
    assert_eq!(
        parse(
            source.as_ref(),
            initial
                .sessions
                .iter()
                .find(|r| r.key.id == "main")
                .unwrap()
        )
        .1[0]
            .text,
        "needle opencode café 🦀"
    );
}

#[test]
fn opencode_same_size_metadata_update_and_bad_json_invalidate() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let writer = opencode(&ctx);
    let source = source_for(&ctx, Agent::Opencode).unwrap();
    let first = discover(source.as_ref(), &ctx);
    writer.execute_batch("UPDATE session SET title='Changed title!'; UPDATE part SET data='private malformed payload' WHERE id='p2';").unwrap();
    let changed = source.discover(&ctx, &cached(&first)).unwrap();
    assert_ne!(
        first.sessions[0].fingerprint,
        changed.sessions[0].fingerprint
    );
    let (parsed, entries) = parse(source.as_ref(), &changed.sessions[0]);
    assert_eq!(parsed.session.title.as_deref(), Some("Changed title!"));
    assert_eq!(entries.len(), 2);
    assert!(!parsed.complete);
    assert_eq!(
        parsed.diagnostics[0].message,
        "OpenCode contains malformed message or part records"
    );
}

#[test]
fn missing_legacy_and_schema_mismatch_stores_are_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let source = source_for(&ctx, Agent::Opencode).unwrap();
    assert_eq!(
        discover(source.as_ref(), &ctx).diagnostics[0].code,
        "source_unavailable"
    );
    fs::create_dir_all(&ctx.roots.opencode_legacy_storage).unwrap();
    let legacy = discover(source.as_ref(), &ctx);
    assert!(legacy.complete_agents.is_empty());
    assert_eq!(legacy.diagnostics[0].code, "unsupported_legacy_store");
    Connection::open(&ctx.roots.opencode_db)
        .unwrap()
        .execute_batch("CREATE TABLE session(private TEXT);")
        .unwrap();
    let broken = discover(source.as_ref(), &ctx);
    assert!(broken.complete_agents.is_empty());
    assert_eq!(
        broken.diagnostics[0].message,
        "OpenCode database could not be read consistently or has an unsupported schema"
    );
}

#[test]
fn cursor_inherits_child_cwd_and_preserves_missing_companion_state() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (transcript, meta) = cursor(&ctx, "main");
    let child = transcript.parent().unwrap().join("subagents/child.jsonl");
    write(
        &child,
        "{\"role\":\"user\",\"message\":{\"content\":\"child prompt\"}}\n",
    );
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    let first = discover(source.as_ref(), &ctx);
    assert_eq!(first.sessions.len(), 2);
    let child = first.sessions.iter().find(|r| r.key.id == "child").unwrap();
    let (parsed, entries) = parse(source.as_ref(), child);
    assert!(parsed.complete);
    assert_eq!(parsed.session.cwd, Some("/fixture/project".into()));
    assert_eq!(parsed.session.parent.unwrap().id, "main");
    assert_eq!(parsed.session.title.as_deref(), Some("child prompt"));
    assert!(entries[0].automated);
    fs::remove_file(meta).unwrap();
    let missing = discover(source.as_ref(), &ctx);
    assert!(missing.complete_agents.is_empty());
    let (parsed, entries) = parse(
        source.as_ref(),
        missing
            .sessions
            .iter()
            .find(|r| r.key.id == "main")
            .unwrap(),
    );
    assert!(!parsed.complete);
    assert_eq!(parsed.session.cwd, None);
    assert_eq!(entries[0].text, "needle cursor café 🦀");
}

#[test]
fn cursor_metadata_and_database_only_changes_refresh_subagent_flags() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (_, meta) = cursor(&ctx, "main");
    let writer = cursor_store(&meta, &json!({"name":"Store title"}));
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    let first = discover(source.as_ref(), &ctx);
    assert!(!parse(source.as_ref(), &first.sessions[0]).1[0].automated);
    let unchanged = source.discover(&ctx, &cached(&first)).unwrap();
    assert!(unchanged.sessions[0].snapshot.is_none());
    writer
        .execute(
            "UPDATE meta SET value=? WHERE key='0'",
            [hex(&json!({"name":"Store title", "subagentInfo":{}}))],
        )
        .unwrap();
    let changed = source.discover(&ctx, &cached(&first)).unwrap();
    assert_ne!(
        first.sessions[0].fingerprint,
        changed.sessions[0].fingerprint
    );
    let (parsed, entries) = parse(source.as_ref(), &changed.sessions[0]);
    assert_eq!(parsed.session.parent, None);
    assert!(entries.iter().all(|e| e.automated));
    let before = files(&meta.with_file_name("store.db"));
    write(
        &meta,
        "{\"cwd\":\"/new/location\",\"title\":\"Changed title\"}",
    );
    let renamed = source.discover(&ctx, &cached(&changed)).unwrap();
    let parsed = parse(source.as_ref(), &renamed.sessions[0]).0;
    assert_eq!(parsed.session.title.as_deref(), Some("Changed title"));
    assert_eq!(parsed.session.cwd, Some("/new/location".into()));
    assert_eq!(files(&meta.with_file_name("store.db")), before);
}

#[test]
fn cursor_partial_transcripts_and_metadata_errors_are_sanitized() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (transcript, meta) = cursor(&ctx, "main");
    let writer = cursor_store(&meta, &json!({"name":"title"}));
    writer
        .execute("UPDATE meta SET value='private invalid hex'", [])
        .unwrap();
    write(
        &transcript,
        &format!(
            "{}{{\"private incomplete",
            include_str!("fixtures/jsonl/cursor.jsonl")
        ),
    );
    fs::File::options()
        .append(true)
        .open(&transcript)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    let discovery = discover(source.as_ref(), &ctx);
    let (parsed, entries) = parse(source.as_ref(), &discovery.sessions[0]);
    assert_eq!(entries.len(), 3);
    assert!(!parsed.complete);
    assert_eq!(
        parsed
            .diagnostics
            .iter()
            .map(|d| d.code.as_str())
            .collect::<Vec<_>>(),
        ["metadata_unavailable", "malformed_records"]
    );
}

#[test]
fn unsupported_cursor_text_is_reported_and_transcript_symlinks_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (transcript, _) = cursor(&ctx, "main");
    write(&transcript.with_extension("txt"), "user:\nlegacy data");
    symlink(&transcript, transcript.with_file_name("alias.jsonl")).unwrap();
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    let discovery = discover(source.as_ref(), &ctx);
    assert_eq!(discovery.sessions.len(), 1);
    assert!(discovery.complete_agents.is_empty());
    assert_eq!(discovery.diagnostics[0].code, "unsupported_transcript");
}

#[test]
fn cursor_wrapper_parses_full_month_negative_offsets_and_utc() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (transcript, _) = cursor(&ctx, "main");
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    for (time, expected) in [
        (
            "November 16, 2026, 1:23 AM (UTC-5:30)",
            "2026-11-16T06:53:00Z",
        ),
        ("Nov 16, 2026, 1:23 PM (UTC)", "2026-11-16T13:23:00Z"),
    ] {
        write(&transcript, &json!({"role":"user","message":{"content":format!("<timestamp>{time}</timestamp>\n<user_query>\ntext\n</user_query>")}}).to_string());
        let discovery = discover(source.as_ref(), &ctx);
        let (_, entries) = parse(source.as_ref(), &discovery.sessions[0]);
        assert_eq!(entries[0].text, "text");
        assert_eq!(entries[0].timestamp_raw.as_deref(), Some(expected));
    }
}

#[test]
fn cursor_companion_directory_symlink_reports_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (_, meta) = cursor(&ctx, "main");
    let unrelated = dir.path().join("unrelated");
    fs::rename(meta.parent().unwrap(), &unrelated).unwrap();
    symlink(&unrelated, ctx.roots.cursor_chats.join("main")).unwrap();
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    let found = source
        .discover(
            &ctx,
            &DiscoveryCache {
                by_source: BTreeMap::new(),
                verify: false,
            },
        )
        .unwrap();
    assert!(found.sessions.is_empty());
    assert!(found.complete_agents.is_empty());
    assert_eq!(found.diagnostics[0].code, "source_unavailable");
}

#[test]
fn opencode_enumerates_sessions_missing_from_partial_cache() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let writer = opencode(&ctx);
    writer
        .execute_batch(
            "INSERT INTO session VALUES('other', '/fixture/project', 'Other', 1, 2, NULL, NULL)",
        )
        .unwrap();
    let source = source_for(&ctx, Agent::Opencode).unwrap();
    let initial = discover(source.as_ref(), &ctx);
    let mut partial = cached(&initial);
    partial
        .by_source
        .retain(|_, cached| cached.key.id == "main");
    let found = source.discover(&ctx, &partial).unwrap();
    assert_eq!(
        found
            .sessions
            .iter()
            .map(|raw| raw.key.id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["main", "other"])
    );
    assert!(found.complete_agents.contains(&Agent::Opencode));
}

#[test]
fn cursor_entries_of_only_tool_results_are_tool_results() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let (transcript, _) = cursor(&ctx, "tools");
    let records = [
        json!({"role":"assistant","message":{"content":[{"type":"tool-call","name":"Shell","input":{"command":"ls"}}]}}),
        json!({"role":"tool","message":{"content":[{"type":"tool-result","result":"one"},{"type":"toolResult","output":"two"},{"type":"tool_result","content":"three"}]}}),
        json!({"role":"assistant","message":{"content":[{"type":"tool_use","name":"Shell","input":"ls"},{"type":"tool_result","content":"mixed"}]}}),
    ];
    write(
        &transcript,
        &records
            .iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>(),
    );
    let source = source_for(&ctx, Agent::Cursor).unwrap();
    let discovery = discover(source.as_ref(), &ctx);
    let (_, entries) = parse(source.as_ref(), &discovery.sessions[0]);
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.kind, e.searchable))
            .collect::<Vec<_>>(),
        [
            (Kind::Tool, Searchable::Text),
            (Kind::Tool, Searchable::ToolResult),
            (Kind::Tool, Searchable::Text),
        ]
    );
}
