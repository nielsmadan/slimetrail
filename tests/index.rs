#[allow(dead_code)]
mod support;
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;
use slimetrail::{
    index::{DATABASE, IndexSession},
    sources::{AppendState, SourceState},
    *,
};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::Duration,
};
use support::{an_hour_later, context_at};

fn context(base: &Path) -> Context {
    context_at(base).cwd(base).build()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn claude(ctx: &Context, id: &str, text: &str) -> PathBuf {
    let path = ctx
        .roots
        .claude_projects
        .join(format!("project/{id}.jsonl"));
    write(
        &path,
        &format!(
            "{}\n",
            json!({"type":"user","sessionId":id,"cwd":"/fixture/project","message":{"content":text}})
        ),
    );
    path
}

fn selected(agent: Agent) -> BTreeSet<Agent> {
    BTreeSet::from([agent])
}
fn key(id: &str) -> SessionKey {
    SessionKey {
        agent: Agent::Claude,
        id: id.into(),
    }
}
fn texts(index: &IndexSession, key: &SessionKey) -> Vec<String> {
    let mut found = Vec::new();
    index
        .visit_entries(key, &mut |entry| {
            found.push(entry.text);
            Ok(())
        })
        .unwrap();
    found
}

#[test]
fn unchanged_touch_and_removal_are_incremental() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let first = claude(&ctx, "one", "old content");
    claude(&ctx, "two", "other content");
    let agents = selected(Agent::Claude);
    let first_run = refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    assert!(!first_run.incomplete);
    assert_eq!(first_run.data.unwrap().reparsed, 2);
    let unchanged = refresh(&ctx, &agents, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!((unchanged.reparsed, unchanged.reused), (0, 2));
    claude(&ctx, "one", "replacement content");
    let mut lease = IndexSession::open(&ctx).unwrap();
    let changed = lease
        .refresh(&agents, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!((changed.reparsed, changed.reused), (1, 1));
    assert_eq!(texts(&lease, &key("one")), ["replacement content"]);
    lease.verify_integrity().unwrap();
    fs::remove_file(first).unwrap();
    let removed = lease
        .refresh(&agents, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(removed.removed, 1);
    assert!(lease.session(&key("one")).unwrap().is_none());
    lease.verify_integrity().unwrap();
}

#[test]
fn removed_transcript_text_does_not_linger_in_the_index_files() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let marker = "quokka-sentinel-transcript-text";
    let gone = claude(&ctx, "gone", &marker.repeat(400));
    claude(&ctx, "kept", "ordinary content");
    let agents = selected(Agent::Claude);
    refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    fs::remove_file(gone).unwrap();
    let removed = refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    assert_eq!(removed.data.unwrap().removed, 1);
    let stored: Vec<u8> = ["", "-wal"]
        .iter()
        .filter_map(|suffix| fs::read(ctx.index_dir.join(format!("{DATABASE}{suffix}"))).ok())
        .flatten()
        .collect();
    assert!(
        !stored
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    );
}

#[test]
fn stable_partial_records_remain_incomplete_until_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "one", "good record");
    let good = fs::read_to_string(&path).unwrap();
    write(&path, &format!("{good}{{\"type\":"));
    let ctx = an_hour_later(&ctx, &path);
    let mut lease = IndexSession::open(&ctx).unwrap();
    let mut replayed = Vec::new();
    for _ in 0..2 {
        let result = lease
            .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
            .unwrap();
        assert!(result.incomplete);
        let stats = result.data.as_ref().unwrap();
        replayed.push((
            (stats.reparsed, stats.reused, stats.failed),
            result.diagnostics,
        ));
        assert_eq!(texts(&lease, &key("one")), ["good record"]);
    }
    assert_eq!(
        replayed.iter().map(|(stats, _)| *stats).collect::<Vec<_>>(),
        [(1, 0, 1), (0, 1, 1)]
    );
    assert_eq!(replayed[1].1, replayed[0].1);
    assert_eq!(
        replayed[1].1[0].message,
        "1 nonempty records were not valid JSON objects"
    );
    write(
        &path,
        &format!(
            "{good}{}\n",
            json!({"type":"assistant","message":"completed record"})
        ),
    );
    assert!(
        !lease
            .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
            .unwrap()
            .incomplete
    );
    assert_eq!(
        texts(&lease, &key("one")),
        ["good record", "completed record"]
    );
    lease.verify_integrity().unwrap();
}

#[test]
fn unavailable_inventory_keeps_old_rows_visible_and_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "prior text");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let moved = dir.path().join("temporarily-moved");
    fs::rename(&ctx.roots.claude_projects, &moved).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    let result = lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    assert!(result.incomplete);
    assert_eq!(result.data.unwrap().removed, 0);
    assert_eq!(texts(&lease, &key("one")), ["prior text"]);
    drop(lease);
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(db);
    fs::rename(moved, &ctx.roots.claude_projects).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    assert!(
        !lease
            .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
            .unwrap()
            .incomplete
    );
    assert_eq!(texts(&lease, &key("one")), ["prior text"]);
}

#[test]
fn replacement_changes_identity_and_truncation_replaces_all_entries() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "one", "first");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let other = dir.path().join("replacement");
    write(
        &other,
        &format!(
            "{}\n",
            json!({"type":"user","sessionId":"new","message":"second"})
        ),
    );
    fs::rename(other, &path).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    assert!(lease.session(&key("one")).unwrap().is_none());
    assert_eq!(texts(&lease, &key("new")), ["second"]);
    write(&path, "");
    lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    assert!(lease.session(&key("new")).unwrap().is_none());
    assert_eq!(texts(&lease, &key("one")), [] as [std::string::String; 0]);
    lease.verify_integrity().unwrap();
}

#[test]
fn namespaces_include_every_effective_root_and_resolve_missing_paths() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let original = IndexSession::open(&ctx).unwrap().namespace().to_owned();
    let roots = serde_json::to_value(&ctx.roots).unwrap();
    for name in roots.as_object().unwrap().keys() {
        let mut changed = roots.clone();
        changed[name] = json!(dir.path().join(format!("other-{name}/root")));
        let mut other = ctx.clone();
        other.roots = serde_json::from_value(changed).unwrap();
        assert_ne!(
            IndexSession::open(&other).unwrap().namespace(),
            original,
            "{name}"
        );
    }
    let mut lexical = ctx.clone();
    lexical.roots.claude_projects = ctx.roots.claude_projects.join("missing/..");
    assert_eq!(IndexSession::open(&lexical).unwrap().namespace(), original);
    let mut relative = ctx.clone();
    relative.roots.claude_projects = ctx
        .roots
        .claude_projects
        .strip_prefix(dir.path())
        .unwrap()
        .into();
    assert_eq!(IndexSession::open(&relative).unwrap().namespace(), original);
}

#[test]
fn root_and_agent_changes_cannot_reuse_or_delete_other_rows() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first root");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let mut other = ctx.clone();
    other.roots.claude_projects = dir.path().join("second-root");
    claude(&other, "one", "second root");
    let mut lease = IndexSession::open(&other).unwrap();
    assert_eq!(
        lease
            .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .reparsed,
        1
    );
    assert_eq!(texts(&lease, &key("one")), ["second root"]);
    drop(lease);
    refresh(&ctx, &selected(Agent::Pi), &RefreshOptions::QUERY).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    assert_eq!(texts(&lease, &key("one")), ["first root"]);
}

#[test]
fn permissions_symlinks_and_native_overlap_are_rejected_before_writes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    fs::create_dir(&ctx.index_dir).unwrap();
    fs::set_permissions(&ctx.index_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(IndexSession::open(&ctx).is_err());
    assert!(fs::read_dir(&ctx.index_dir).unwrap().next().is_none());
    fs::set_permissions(&ctx.index_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let db = ctx.index_dir.join(DATABASE);
    fs::write(&db, []).unwrap();
    fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(IndexSession::open(&ctx).is_err());
    assert!(!ctx.index_dir.join("lifecycle.lock").exists());
    fs::remove_file(&db).unwrap();
    let mut overlap = ctx.clone();
    overlap.index_dir = ctx.roots.claude_projects.join("private-index");
    assert!(IndexSession::open(&overlap).is_err());
    assert!(!ctx.roots.claude_projects.exists());
    let alias = dir.path().join("alias");
    symlink(&ctx.index_dir, &alias).unwrap();
    let mut linked = ctx.clone();
    linked.index_dir = alias;
    assert!(IndexSession::open(&linked).is_err());
    let target = dir.path().join("untouched");
    fs::write(&target, "sentinel").unwrap();
    symlink(&target, &db).unwrap();
    assert!(IndexSession::open(&ctx).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "sentinel");
}

fn private_file(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

const OLDER: [&str; 3] = [
    "index-v2.sqlite3",
    "index-v2.sqlite3-wal",
    "index-v2.sqlite3-shm",
];

fn older_present(ctx: &Context) -> Vec<bool> {
    OLDER
        .iter()
        .map(|name| ctx.index_dir.join(name).exists())
        .collect()
}

#[test]
fn older_index_is_kept_until_this_version_publishes_data() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first text");
    drop(IndexSession::open(&ctx).unwrap());
    for name in OLDER {
        private_file(&ctx.index_dir.join(name), "sentinel");
    }
    let gated = RefreshOptions {
        build: BuildPolicy::Limit {
            max_ms: 0,
            admission: Admission::PerAgent,
        },
        ..RefreshOptions::QUERY
    };
    let skipped = refresh(&ctx, &selected(Agent::Claude), &gated).unwrap();
    assert_eq!(codes(&skipped), ["index_required"]);
    assert!(ctx.index_dir.join(DATABASE).exists());
    drop(IndexSession::open(&ctx).unwrap());
    assert_eq!(older_present(&ctx), [true, true, true]);
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    drop(IndexSession::open(&ctx).unwrap());
    assert_eq!(older_present(&ctx), [false, false, false]);
}

fn outdated_layout(ctx: &Context) {
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.execute_batch("CREATE TABLE settings(id INTEGER PRIMARY KEY); PRAGMA user_version=3;")
        .unwrap();
    drop(db);
    fs::set_permissions(
        ctx.index_dir.join(DATABASE),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

#[test]
fn an_index_with_an_outdated_layout_is_rebuilt_once_no_other_process_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first text");
    drop(IndexSession::open(&ctx).unwrap());
    private_file(&ctx.index_dir.join(OLDER[0]), "sentinel");
    outdated_layout(&ctx);
    let held = fs::File::open(ctx.index_dir.join("lifecycle.lock")).unwrap();
    held.lock_shared().unwrap();
    let Err(Error::InvalidArgument(message)) = IndexSession::open(&ctx) else {
        panic!("an outdated layout must not be read");
    };
    assert_eq!(
        message,
        "index layout is outdated; it is rebuilt once no other slimetrail process has it open"
    );
    drop(held);
    let mut lease = IndexSession::open(&ctx).unwrap();
    assert!(!ctx.index_dir.join(DATABASE).exists());
    assert_eq!(older_present(&ctx), [true, false, false]);
    let result = lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    assert_eq!(codes(&result), [] as [&str; 0]);
    assert_eq!(texts(&lease, &key("one")), ["first text"]);
}

#[test]
fn private_modes_and_exact_older_version_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first text");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let older = [
        "index-v0.sqlite3",
        "index-v0.sqlite3-wal",
        "index-v2.sqlite3",
        "index-v2.sqlite3-wal",
        "index-v2.sqlite3-shm",
        "index-v2.sqlite3-journal",
    ];
    let kept = [
        "index-v4.sqlite3",
        "index-v00.sqlite3",
        "index-v0.sqlite3.bak",
    ];
    for name in older.iter().chain(&kept) {
        let path = ctx.index_dir.join(name);
        fs::write(&path, "sentinel").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(IndexSession::open(&ctx).unwrap());
    for name in older {
        assert!(!ctx.index_dir.join(name).exists(), "{name}");
    }
    for name in kept {
        assert_eq!(
            fs::read_to_string(ctx.index_dir.join(name)).unwrap(),
            "sentinel"
        );
    }
    for name in [DATABASE, "lifecycle.lock", "writer.lock"] {
        assert_eq!(
            fs::metadata(ctx.index_dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    for path in [&ctx.index_dir, &ctx.index_dir.join("staging")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn new_index_is_v3_in_wal_mode_with_incremental_auto_vacuum() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    drop(IndexSession::open(&ctx).unwrap());
    assert!(!ctx.index_dir.join(DATABASE).exists());
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    let mode: String = db
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    let vacuum: i64 = db
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    let settings: (i64, String) = db
        .query_row("SELECT refresh_seq,coverage FROM settings", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((mode.as_str(), vacuum), ("wal", 2));
    assert_eq!(settings, (1, "compact".into()));
}

#[test]
fn legacy_staging_is_removed_only_when_no_other_session_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let first = IndexSession::open(&ctx).unwrap();
    let staging = ctx.index_dir.join("staging/slimetrail-snapshot-abandoned");
    fs::create_dir(&staging).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
    write(&staging.join("snapshot.db"), "sentinel");
    let second = IndexSession::open_with_timeout(&ctx, Duration::ZERO).unwrap();
    assert_eq!(
        fs::read_to_string(staging.join("snapshot.db")).unwrap(),
        "sentinel"
    );
    drop(first);
    drop(IndexSession::open(&ctx).unwrap());
    assert!(staging.exists());
    drop(second);
    drop(IndexSession::open(&ctx).unwrap());
    assert!(!staging.exists());
}

#[test]
fn repeated_query_after_another_sessions_refresh_sees_later_transcripts() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first text");
    let agents = selected(Agent::Claude);
    let mut session = IndexSession::open(&ctx).unwrap();
    session.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    claude(&ctx, "two", "second text");
    let repeated = session.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    assert!(!repeated.incomplete, "{:?}", repeated.diagnostics);
    assert_eq!(texts(&session, &key("two")), ["second text"]);
}

#[test]
fn fts_documents_keep_original_text_and_native_titles_with_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "one", "ASCII Quote \" percent% abc\0tail ſensitive");
    let text = fs::read_to_string(&path).unwrap();
    write(
        &path,
        &format!(
            "{text}{}\n",
            json!({"type":"custom-title","customTitle":"Native title"})
        ),
    );
    claude(&ctx, "derived", "derived prompt");
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    let mut docs = Vec::new();
    lease
        .visit_documents(&key("one"), Some(("unlikely", false)), &mut |doc| {
            docs.push(doc);
            Ok(())
        })
        .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].text, "ASCII Quote \" percent% abc\0tail ſensitive");
    docs.clear();
    lease
        .visit_documents(&key("one"), Some(("NATIVE", true)), &mut |doc| {
            docs.push(doc);
            Ok(())
        })
        .unwrap();
    assert!(
        docs.iter()
            .any(|doc| doc.kind == "title" && doc.text == "Native title")
    );
    docs.clear();
    lease
        .visit_documents(&key("derived"), None, &mut |doc| {
            docs.push(doc);
            Ok(())
        })
        .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].kind, "message");
    for query in ["\" OR abc", "%", "ab", "\0", "ſ"] {
        lease
            .visit_documents(&key("one"), Some((query, false)), &mut |_| Ok(()))
            .unwrap();
    }
    lease.verify_integrity().unwrap();
}

#[test]
fn database_wal_changes_and_partial_cache_preserve_full_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
    let writer = Connection::open(&ctx.roots.opencode_db).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('one','/fixture','Title',1,2),('two','/fixture','Other',1,2); INSERT INTO message VALUES('m','one',1,'{\"role\":\"user\"}'); INSERT INTO part VALUES('p','m',1,'{\"type\":\"text\",\"text\":\"first\"}');").unwrap();
    let agents = selected(Agent::Opencode);
    assert_eq!(
        refresh(&ctx, &agents, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .reparsed,
        2
    );
    assert_eq!(
        refresh(&ctx, &agents, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .reused,
        2
    );
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON; DELETE FROM sources WHERE id='two';")
        .unwrap();
    drop(db);
    let recovered = refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    assert!(!recovered.incomplete);
    assert_eq!(
        (
            recovered.data.as_ref().unwrap().reused,
            recovered.data.unwrap().reparsed
        ),
        (1, 1)
    );
    writer
        .execute(
            "UPDATE part SET data=?",
            [json!({"type":"text","text":"later"}).to_string()],
        )
        .unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    assert_eq!(
        lease
            .refresh(&agents, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .reparsed,
        2
    );
    assert_eq!(
        texts(
            &lease,
            &SessionKey {
                agent: Agent::Opencode,
                id: "one".into()
            }
        ),
        ["later"]
    );
    let staged: Vec<_> = fs::read_dir(ctx.index_dir.join("staging"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(staged.len(), 1);
    assert!(
        staged[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!("proc-{}-", std::process::id()))
    );
    assert_eq!(
        fs::read_dir(&staged[0])
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        ["owner.lock"]
    );
    lease.verify_integrity().unwrap();
}

#[test]
fn global_document_stream_is_ordered_and_scoped_to_refreshed_agents() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "z-last", "needle last");
    claude(&ctx, "a-first", "needle first");
    let mut lease = IndexSession::open(&ctx).unwrap();
    assert!(lease.visit_search_documents(None, &mut |_| Ok(())).is_err());
    lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    let mut found = Vec::new();
    lease
        .visit_search_documents(Some(("NEEDLE", false)), &mut |doc| {
            found.push((doc.session.id, doc.text));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        found,
        [
            ("a-first".into(), "needle first".into()),
            ("z-last".into(), "needle last".into())
        ]
    );
    drop(lease);
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease
        .refresh(&selected(Agent::Pi), &RefreshOptions::QUERY)
        .unwrap();
    let mut count = 0;
    lease
        .visit_search_documents(None, &mut |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn foreign_keys_reject_cross_namespace_and_cross_source_links() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "content");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for column in ["namespace", "source"] {
        let columns = [
            "namespace",
            "agent",
            "id",
            "source",
            "entry_index",
            "source_id",
            "timestamp",
            "timestamp_raw",
            "role",
            "kind",
            "subtype",
            "tool_name",
            "automated",
            "searchable",
            "tail_start",
            "text_tail",
            "text",
        ];
        let select = columns
            .iter()
            .map(|name| {
                if *name == column {
                    format!("{name} || '-wrong'")
                } else {
                    (*name).into()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        let result = db.execute(
            &format!("INSERT INTO entries SELECT {select} FROM entries LIMIT 1"),
            [],
        );
        assert!(
            matches!(result,Err(rusqlite::Error::SqliteFailure(error,_)) if error.code==rusqlite::ErrorCode::ConstraintViolation)
        );
    }
}

#[test]
fn codex_native_title_dependency_invalidates_unchanged_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    write(
        &ctx.roots.codex_sessions.join("one.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"one\",\"cwd\":\"/fixture\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"prompt\"}]}}\n",
    );
    write(
        &ctx.roots.codex_session_index,
        "{\"id\":\"one\",\"thread_name\":\"First title\"}\n",
    );
    let agents = selected(Agent::Codex);
    refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    write(
        &ctx.roots.codex_session_index,
        "{\"id\":\"one\",\"thread_name\":\"Later title\"}\n",
    );
    let mut lease = IndexSession::open(&ctx).unwrap();
    assert_eq!(
        lease
            .refresh(&agents, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .reparsed,
        1
    );
    let key = SessionKey {
        agent: Agent::Codex,
        id: "one".into(),
    };
    assert_eq!(
        lease.session(&key).unwrap().unwrap().title.as_deref(),
        Some("Later title")
    );
    lease.verify_integrity().unwrap();
}

#[test]
fn namespace_canonicalization_does_not_bypass_reader_symlink_guards() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "one", "original");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let moved = dir.path().join("other-store");
    fs::rename(&ctx.roots.claude_projects, &moved).unwrap();
    symlink(&moved, &ctx.roots.claude_projects).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    let result = lease
        .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
        .unwrap();
    assert!(result.incomplete);
    assert_eq!(texts(&lease, &key("one")), [] as [std::string::String; 0]);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        fs::read_to_string(moved.join("project/one.jsonl")).unwrap()
    );
}

#[test]
fn native_roots_below_index_are_rejected_before_cleanup() {
    for field in [
        "claude_projects",
        "opencode_db",
        "codex_session_index",
        "devin_sessions_db",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        let native = ctx.index_dir.join("staging/slimetrail-snapshot-native");
        let sentinel = native.join("sentinel");
        write(&sentinel, "native contents");
        for path in [&ctx.index_dir, &ctx.index_dir.join("staging"), &native] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut roots = serde_json::to_value(&ctx.roots).unwrap();
        roots[field] = json!(if field == "claude_projects" {
            native.clone()
        } else {
            sentinel.clone()
        });
        ctx.roots = serde_json::from_value(roots).unwrap();
        assert!(IndexSession::open(&ctx).is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "native contents");
        assert_eq!(fs::read_dir(&ctx.index_dir).unwrap().count(), 1);
    }
}

#[test]
fn relative_index_symlink_is_checked_against_context_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = context(dir.path());
    let target = dir.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, dir.path().join("alias-index")).unwrap();
    ctx.index_dir = "alias-index".into();
    assert!(IndexSession::open(&ctx).is_err());
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn displaced_duplicate_is_reparsed_after_incomplete_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = context(dir.path());
    let first = claude(&ctx, "one", "original winner");
    let agents = selected(Agent::Claude);
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    let replacement = ctx.roots.claude_projects.join("project/replacement.jsonl");
    write(
        &replacement,
        &fs::read_to_string(&first)
            .unwrap()
            .replace("original winner", "new winner"),
    );
    fs::File::open(&replacement)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(60)),
        )
        .unwrap();
    drop(lease);
    let invalid_archive = dir.path().join("invalid-archive");
    write(&invalid_archive, "invalid archive");
    ctx.roots.archive = Some(invalid_archive);
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    fs::remove_file(&replacement).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    write(
        &replacement,
        &fs::read_to_string(&first)
            .unwrap()
            .replace("original winner", "new winner"),
    );
    fs::File::open(&replacement)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(60)),
        )
        .unwrap();
    assert!(
        lease
            .refresh(&agents, &RefreshOptions::QUERY)
            .unwrap()
            .incomplete
    );
    assert_eq!(texts(&lease, &key("one")), ["new winner"]);
    fs::remove_file(&replacement).unwrap();
    fs::remove_file(ctx.roots.archive.as_ref().unwrap()).unwrap();
    let restored = lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    assert!(!restored.incomplete);
    assert_eq!(restored.data.unwrap().reparsed, 1);
    assert_eq!(texts(&lease, &key("one")), ["original winner"]);
    assert!(lease.session(&key("one")).unwrap().is_some());
    assert_eq!(
        lease
            .refresh(&agents, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap()
            .reused,
        1
    );
    lease.verify_integrity().unwrap();
}

#[test]
fn long_session_replacement_preserves_entries_foreign_keys_and_fts() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = ctx.roots.claude_projects.join("long.jsonl");
    let mut index = IndexSession::open(&ctx).unwrap();
    for count in [4000, 2000] {
        let text = (0..count).map(|ordinal| json!({"type":"user", "sessionId":"long", "uuid":format!("native-{ordinal}"), "message":format!("needle {ordinal}")}).to_string()).collect::<Vec<_>>().join("\n") + "\n";
        write(&path, &text);
        let result = index
            .refresh(&selected(Agent::Claude), &RefreshOptions::QUERY)
            .unwrap();
        assert!(!result.incomplete);
        assert_eq!(result.data.unwrap().reparsed, 1);
        let mut found = Vec::new();
        index
            .visit_entries(&key("long"), &mut |entry| {
                found.push((entry.index, entry.source_id));
                Ok(())
            })
            .unwrap();
        assert_eq!(found.len(), count);
        assert_eq!(
            found.last().unwrap(),
            &((count - 1) as u64, format!("native-{}:0", count - 1))
        );
        let mut documents = 0;
        index
            .visit_documents(&key("long"), Some(("needle", false)), &mut |_| {
                documents += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(documents, count);
        index.verify_integrity().unwrap();
    }
}

const APPENDABLE: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Pi];

fn jsonl_fixture(agent: Agent) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/jsonl/{agent}.jsonl")),
    )
    .unwrap()
}

fn transcript(ctx: &Context, agent: Agent, name: &str) -> PathBuf {
    let root = match agent {
        Agent::Claude => &ctx.roots.claude_projects,
        Agent::Codex => &ctx.roots.codex_sessions,
        Agent::Droid => &ctx.roots.droid_sessions,
        Agent::Pi => &ctx.roots.pi_sessions,
        _ => unreachable!("not a JSONL transcript agent"),
    };
    root.join(format!("project/{name}.jsonl"))
}

fn canonical(agent: Agent) -> SessionKey {
    SessionKey {
        agent,
        id: format!("{agent}-canonical"),
    }
}

fn with_companion(ctx: &Context, agent: Agent) -> PathBuf {
    let path = transcript(ctx, agent, "session");
    write(&path, &jsonl_fixture(agent));
    write(
        &transcript(ctx, agent, "zz-companion"),
        &jsonl_fixture(agent).replace(&format!("{agent}-canonical"), "zz-companion"),
    );
    path
}

fn append(path: &Path, text: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn appended_records(agent: Agent) -> String {
    let records = match agent {
        Agent::Claude => vec![
            json!({"type":"user","uuid":"u9","timestamp":"2026-08-10T13:00:00Z","gitBranch":"later","message":{"content":"appended prompt"}}),
            json!({"type":"custom-title","customTitle":"Renamed"}),
            json!({"type":"assistant","timestamp":"2026-08-10T13:01:00Z","message":{"content":[{"type":"thinking","thinking":"appended thought"},{"type":"text","text":"appended reply"}]}}),
        ],
        Agent::Codex => vec![
            json!({"type":"response_item","timestamp":"2026-08-10T13:00:00Z","payload":{"type":"message","role":"assistant","content":"appended reply"}}),
            json!({"type":"response_item","timestamp":"2026-08-10T13:01:00Z","payload":{"type":"function_call","name":"exec","arguments":"{}"}}),
        ],
        Agent::Droid => vec![
            json!({"type":"message","timestamp":"2026-08-10T13:00:00Z","message":{"role":"user","content":"appended prompt"}}),
            json!({"type":"message","message":{"role":"tool","toolName":"Shell","content":"appended result"}}),
        ],
        _ => vec![
            json!({"type":"session_info","name":"Renamed"}),
            json!({"type":"message","timestamp":"2026-08-10T13:00:00Z","message":{"role":"user","content":"appended prompt"}}),
        ],
    };
    records.iter().map(|r| r.to_string() + "\n").collect()
}

fn refreshed(ctx: &Context, agents: &BTreeSet<Agent>) -> (IndexSession, Envelope<IndexStats>) {
    let mut lease = IndexSession::open(ctx).unwrap();
    let result = lease.refresh(agents, &RefreshOptions::QUERY).unwrap();
    lease.verify_integrity().unwrap();
    (lease, result)
}

fn snapshot(lease: &IndexSession, key: &SessionKey) -> (Session, Vec<Entry>) {
    let mut entries = Vec::new();
    lease
        .visit_entries(key, &mut |entry| {
            entries.push(entry);
            Ok(())
        })
        .unwrap();
    (lease.session(key).unwrap().unwrap(), entries)
}

fn cold(ctx: &Context, key: &SessionKey) -> (Session, Vec<Entry>) {
    let dir = tempfile::tempdir().unwrap();
    let mut other = ctx.clone();
    other.index_dir = dir.path().join("index");
    let (lease, result) = refreshed(&other, &selected(key.agent));
    assert_eq!(result.diagnostics, []);
    snapshot(&lease, key)
}

fn source_state(ctx: &Context, path: &Path) -> Option<SourceState> {
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.query_row(
        "SELECT json_extract(state,'$.state') FROM sources WHERE source=?1",
        [path.to_string_lossy()],
        |r| r.get::<_, Option<String>>(0),
    )
    .unwrap()
    .map(|state| serde_json::from_str(&state).unwrap())
}

fn append_state(ctx: &Context, path: &Path) -> AppendState {
    let Some(SourceState::Append(state)) = source_state(ctx, path) else {
        panic!("expected an append state for {}", path.display());
    };
    state
}

fn rowids(ctx: &Context, key: &SessionKey) -> Vec<i64> {
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    let mut statement = db
        .prepare("SELECT rowid FROM entries WHERE agent=?1 AND id=?2 ORDER BY entry_index")
        .unwrap();
    statement
        .query_map([key.agent.to_string(), key.id.clone()], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn appended_records_resume_and_match_a_cold_index() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let agents = BTreeSet::from(APPENDABLE);
    let paths = APPENDABLE.map(|agent| with_companion(&ctx, agent));
    let (lease, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    drop(lease);
    let before = APPENDABLE.map(|agent| {
        let path = transcript(&ctx, agent, "session");
        (append_state(&ctx, &path), rowids(&ctx, &canonical(agent)))
    });
    for (agent, path) in APPENDABLE.iter().zip(&paths) {
        append(path, &appended_records(*agent));
    }
    let (lease, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    let counts = result.data.unwrap();
    assert_eq!((counts.reparsed, counts.reused), (4, 4));
    for ((agent, path), (old_state, old_rowids)) in APPENDABLE.iter().zip(&paths).zip(&before) {
        let key = canonical(*agent);
        let state = append_state(&ctx, path);
        assert!(state.consumed > old_state.consumed, "{agent}");
        assert_eq!(state.consumed, fs::metadata(path).unwrap().len(), "{agent}");
        let rowids = rowids(&ctx, &key);
        assert!(rowids.len() > old_rowids.len(), "{agent}");
        assert_eq!(
            &rowids[..old_rowids.len()],
            old_rowids.as_slice(),
            "{agent}"
        );
        let (session, entries) = snapshot(&lease, &key);
        assert_eq!(state.entries, entries.len() as u64, "{agent}");
        assert_eq!((session, entries), cold(&ctx, &key), "{agent}");
    }
}

#[test]
fn unterminated_final_record_is_searchable_then_completed_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = with_companion(&ctx, Agent::Claude);
    let key = canonical(Agent::Claude);
    let prefix = fs::metadata(&path).unwrap().len();
    append(
        &path,
        &json!({"type":"custom-title","customTitle":"Torn title"}).to_string(),
    );
    let (lease, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    let mut found = Vec::new();
    lease
        .visit_documents(&key, Some(("torn title", false)), &mut |doc| {
            found.push((doc.kind, doc.text));
            Ok(())
        })
        .unwrap();
    assert!(found.contains(&("title".to_string(), "Torn title".to_string())));
    drop(lease);
    let state = append_state(&ctx, &path);
    assert_eq!(state.consumed, prefix);
    assert_eq!(state.session.title.as_deref(), Some("Native title"));
    let before = rowids(&ctx, &key);
    append(
        &path,
        &format!(
            "\n{}\n",
            json!({"type":"user","timestamp":"2026-08-10T13:00:00Z","message":"torn needle follow-up"})
        ),
    );
    let (lease, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    let (session, entries) = snapshot(&lease, &key);
    assert_eq!(session.title.as_deref(), Some("Torn title"));
    assert_eq!(&rowids(&ctx, &key)[..before.len()], before.as_slice());
    let ids: BTreeSet<_> = entries.iter().map(|e| e.source_id.clone()).collect();
    assert_eq!(ids.len(), entries.len());
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.text == "torn needle follow-up")
            .count(),
        1
    );
    assert_eq!(
        append_state(&ctx, &path).consumed,
        fs::metadata(&path).unwrap().len()
    );
    assert_eq!((session, entries), cold(&ctx, &key));
}

#[test]
fn recent_torn_final_line_is_not_malformed_and_old_one_is() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = with_companion(&ctx, Agent::Claude);
    let key = canonical(Agent::Claude);
    append(&path, "{\"type\":\"assistant\",\"message\":\"half");
    let (lease, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    assert!(!result.incomplete);
    drop(lease);
    let before = rowids(&ctx, &key);
    append(&path, " written\"}\n");
    let (lease, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    let (session, entries) = snapshot(&lease, &key);
    assert_eq!(entries.last().unwrap().text, "half written");
    assert_eq!(&rowids(&ctx, &key)[..before.len()], before.as_slice());
    assert_eq!((session, entries), cold(&ctx, &key));
    drop(lease);
    append(&path, "{\"type\":\"assistant\",\"message\":\"stalled");
    let ctx = an_hour_later(&ctx, &path);
    let (_, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(
        result
            .diagnostics
            .iter()
            .map(|d| (d.code.as_str(), d.message.as_str()))
            .collect::<Vec<_>>(),
        [(
            "malformed_records",
            "1 nonempty records were not valid JSON objects"
        )]
    );
}

#[test]
fn rewritten_prefix_falls_back_to_a_full_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = with_companion(&ctx, Agent::Claude);
    let key = canonical(Agent::Claude);
    refreshed(&ctx, &selected(Agent::Claude));
    let companion = rowids(
        &ctx,
        &SessionKey {
            agent: Agent::Claude,
            id: "zz-companion".into(),
        },
    );
    let original = fs::read_to_string(&path).unwrap();
    let rewritten = original.replace("\"Done\"", "\"Dune\"");
    assert_eq!(rewritten.len(), original.len());
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all(rewritten.as_bytes())
        .unwrap();
    append(&path, &appended_records(Agent::Claude));
    let (lease, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    let (session, entries) = snapshot(&lease, &key);
    assert!(entries.iter().any(|e| e.text == "Dune"));
    let max = companion.iter().max().unwrap();
    assert!(rowids(&ctx, &key).iter().all(|id| id > max));
    assert_eq!(
        append_state(&ctx, &path).consumed,
        fs::metadata(&path).unwrap().len()
    );
    assert_eq!((session, entries), cold(&ctx, &key));
}

#[test]
fn truncation_and_replacement_fully_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = with_companion(&ctx, Agent::Pi);
    let key = canonical(Agent::Pi);
    append(&path, &appended_records(Agent::Pi));
    refreshed(&ctx, &selected(Agent::Pi));
    let inode = append_state(&ctx, &path).ino;
    let fixture = jsonl_fixture(Agent::Pi);
    let shorter: String = fixture
        .lines()
        .take(2)
        .map(|l| l.to_owned() + "\n")
        .collect();
    fs::write(&path, &shorter).unwrap();
    let (lease, result) = refreshed(&ctx, &selected(Agent::Pi));
    assert_eq!(result.diagnostics, []);
    let state = append_state(&ctx, &path);
    assert_eq!((state.consumed, state.ino), (shorter.len() as u64, inode));
    let (session, entries) = snapshot(&lease, &key);
    assert_eq!(session.title.as_deref(), Some("Native title"));
    assert_eq!((session, entries), cold(&ctx, &key));
    drop(lease);
    let replacement = dir.path().join("replacement.jsonl");
    write(
        &replacement,
        &format!("{fixture}{}", appended_records(Agent::Pi)),
    );
    fs::rename(&replacement, &path).unwrap();
    let (lease, result) = refreshed(&ctx, &selected(Agent::Pi));
    assert_eq!(result.diagnostics, []);
    let state = append_state(&ctx, &path);
    assert_eq!(state.ino, fs::metadata(&path).unwrap().ino());
    assert_ne!(state.ino, inode);
    let (session, entries) = snapshot(&lease, &key);
    assert_eq!(session.title.as_deref(), Some("Renamed"));
    assert_eq!((session, entries), cold(&ctx, &key));
}

#[test]
fn compressed_rollouts_always_fully_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = transcript(&ctx, Agent::Codex, "rollout").with_extension("jsonl.zst");
    let compress = |text: &str| {
        ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        )
    };
    let fixture = jsonl_fixture(Agent::Codex);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, compress(&fixture)).unwrap();
    let key = canonical(Agent::Codex);
    refreshed(&ctx, &selected(Agent::Codex));
    assert_eq!(source_state(&ctx, &path), None);
    fs::write(
        &path,
        compress(&format!("{fixture}{}", appended_records(Agent::Codex))),
    )
    .unwrap();
    let (lease, result) = refreshed(&ctx, &selected(Agent::Codex));
    assert_eq!(result.diagnostics, []);
    assert_eq!(source_state(&ctx, &path), None);
    let (session, entries) = snapshot(&lease, &key);
    assert_eq!(entries.last().unwrap().text, "exec\n{}");
    assert_eq!((session, entries), cold(&ctx, &key));
}

#[test]
fn droid_settings_change_reparses_and_codex_title_change_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let droid = with_companion(&ctx, Agent::Droid);
    let codex = with_companion(&ctx, Agent::Codex);
    let names = &ctx.roots.codex_session_index;
    write(
        names,
        "{\"id\":\"codex-canonical\",\"thread_name\":\"First\"}\n",
    );
    let agents = BTreeSet::from([Agent::Codex, Agent::Droid]);
    refreshed(&ctx, &agents);
    let companion = rowids(
        &ctx,
        &SessionKey {
            agent: Agent::Droid,
            id: "zz-companion".into(),
        },
    );
    let codex_state = append_state(&ctx, &codex);
    let codex_rowids = rowids(&ctx, &canonical(Agent::Codex));
    write(
        &droid.with_extension("settings.json"),
        "{\"tags\":[{\"name\":\"exec\"}]}",
    );
    append(&droid, &appended_records(Agent::Droid));
    write(
        names,
        "{\"id\":\"codex-canonical\",\"thread_name\":\"Second\"}\n",
    );
    let (lease, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    let max = companion.iter().max().unwrap();
    assert!(
        rowids(&ctx, &canonical(Agent::Droid))
            .iter()
            .all(|id| id > max)
    );
    let (session, entries) = snapshot(&lease, &canonical(Agent::Droid));
    assert!(entries.iter().all(|e| e.automated));
    assert_eq!((session, entries), cold(&ctx, &canonical(Agent::Droid)));
    assert_eq!(append_state(&ctx, &codex), codex_state);
    assert_eq!(rowids(&ctx, &canonical(Agent::Codex)), codex_rowids);
    let (session, entries) = snapshot(&lease, &canonical(Agent::Codex));
    assert_eq!(
        (session.title.as_deref(), session.title_origin),
        (Some("Second"), Some(TitleOrigin::Native))
    );
    let mut titles = Vec::new();
    lease
        .visit_documents(
            &canonical(Agent::Codex),
            Some(("second", false)),
            &mut |doc| {
                titles.push((doc.kind, doc.text));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        titles
            .iter()
            .filter(|(kind, _)| kind == "title")
            .collect::<Vec<_>>(),
        [&("title".to_string(), "Second".to_string())]
    );
    assert_eq!((session, entries), cold(&ctx, &canonical(Agent::Codex)));
}

fn titled_hits(ctx: &Context, title: &str) -> Vec<(String, Option<String>)> {
    let mut options = query_options(title);
    options.filter.agents = selected(Agent::Codex);
    let result = search(ctx, &options, &RefreshOptions::QUERY).unwrap();
    result
        .data
        .unwrap()
        .results
        .into_iter()
        .map(|hit| (hit.session.key.id, hit.session.title))
        .collect()
}

#[test]
fn codex_title_index_changes_invalidate_only_the_named_session() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let agents = selected(Agent::Codex);
    let fixture = jsonl_fixture(Agent::Codex);
    let plain = transcript(&ctx, Agent::Codex, "session");
    write(&plain, &fixture);
    let zst = transcript(&ctx, Agent::Codex, "zst-thread").with_extension("jsonl.zst");
    fs::write(
        &zst,
        ruzstd::encoding::compress_to_vec(
            fixture.replace("codex-canonical", "zst-thread").as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .unwrap();
    let names = &ctx.roots.codex_session_index;
    let line = |id: &str, name: &str| format!("{}\n", json!({"id":id,"thread_name":name}));
    write(
        names,
        &(line("codex-canonical", "Plain title") + &line("zst-thread", "Packed title")),
    );
    let zst_key = SessionKey {
        agent: Agent::Codex,
        id: "zst-thread".into(),
    };
    let (_, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    let plain_state = append_state(&ctx, &plain);
    let (plain_rows, zst_rows) = (
        rowids(&ctx, &canonical(Agent::Codex)),
        rowids(&ctx, &zst_key),
    );

    write(
        &transcript(&ctx, Agent::Codex, "new-thread"),
        &fixture.replace("codex-canonical", "new-thread"),
    );
    append(names, &line("new-thread", "Fresh title"));
    let (_, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (1, 2));
    assert_eq!(rowids(&ctx, &zst_key), zst_rows);
    assert_eq!(rowids(&ctx, &canonical(Agent::Codex)), plain_rows);

    append(names, &line("zst-thread", "Repacked title"));
    let (lease, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (1, 2));
    assert_eq!(
        lease.session(&zst_key).unwrap().unwrap().title.as_deref(),
        Some("Repacked title")
    );
    drop(lease);
    assert_eq!(
        titled_hits(&ctx, "Repacked title"),
        [("zst-thread".into(), Some("Repacked title".into()))]
    );
    assert_eq!(rowids(&ctx, &canonical(Agent::Codex)), plain_rows);
    let zst_renamed = rowids(&ctx, &zst_key);
    assert_ne!(zst_renamed, zst_rows);

    append(names, &line("codex-canonical", "Plain renamed"));
    let (lease, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (1, 2));
    assert_eq!(append_state(&ctx, &plain), plain_state);
    assert_eq!(rowids(&ctx, &canonical(Agent::Codex)), plain_rows);
    assert_eq!(rowids(&ctx, &zst_key), zst_renamed);
    let session = lease.session(&canonical(Agent::Codex)).unwrap().unwrap();
    assert_eq!(
        (session.title.as_deref(), session.title_origin),
        (Some("Plain renamed"), Some(TitleOrigin::Native))
    );
    drop(lease);
    assert_eq!(
        titled_hits(&ctx, "Plain renamed"),
        [("codex-canonical".into(), Some("Plain renamed".into()))]
    );

    write(
        names,
        &(line("zst-thread", "Repacked title") + &line("new-thread", "Fresh title")),
    );
    let (lease, result) = refreshed(&ctx, &agents);
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (1, 2));
    let session = lease.session(&canonical(Agent::Codex)).unwrap().unwrap();
    assert_eq!(session.title_origin, Some(TitleOrigin::Derived));
    assert_ne!(session.title.as_deref(), Some("Plain renamed"));
    drop(lease);
    assert_eq!(titled_hits(&ctx, "Plain renamed"), []);
}

#[test]
fn cursor_appends_still_fully_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let fixture = include_str!("fixtures/jsonl/cursor.jsonl");
    let transcript = |id: &str| {
        let path = ctx
            .roots
            .cursor_projects
            .join(format!("slug/agent-transcripts/{id}/{id}.jsonl"));
        write(&path, fixture);
        write(
            &ctx.roots.cursor_chats.join(format!("hash/{id}/meta.json")),
            &json!({"cwd":"/fixture/project","title":"Cursor title","createdAtMs":1_786_356_360_000_i64,"updatedAtMs":1_786_356_480_000_i64}).to_string(),
        );
        path
    };
    let path = transcript("main");
    transcript("zz-companion");
    let key = SessionKey {
        agent: Agent::Cursor,
        id: "main".into(),
    };
    let (_, result) = refreshed(&ctx, &selected(Agent::Cursor));
    assert_eq!(result.diagnostics, []);
    assert_eq!(source_state(&ctx, &path), None);
    let companion = rowids(
        &ctx,
        &SessionKey {
            agent: Agent::Cursor,
            id: "zz-companion".into(),
        },
    );
    append(
        &path,
        &format!(
            "{}\n",
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"appended cursor reply"}]}})
        ),
    );
    let (lease, result) = refreshed(&ctx, &selected(Agent::Cursor));
    assert_eq!(result.diagnostics, []);
    assert_eq!(source_state(&ctx, &path), None);
    let max = companion.iter().max().unwrap();
    assert!(rowids(&ctx, &key).iter().all(|id| id > max));
    let (session, entries) = snapshot(&lease, &key);
    assert_eq!(entries.last().unwrap().text, "appended cursor reply");
    assert_eq!((session, entries), cold(&ctx, &key));
}

fn codes(result: &Envelope<IndexStats>) -> Vec<&str> {
    result.diagnostics.iter().map(|d| d.code.as_str()).collect()
}

const fn counts(result: &Envelope<IndexStats>) -> (u64, u64) {
    let stats = result.data.as_ref().unwrap();
    (stats.reparsed, stats.reused)
}

#[test]
fn unchanged_recent_torn_final_line_is_rechecked_until_it_counts_as_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = with_companion(&ctx, Agent::Claude);
    let key = canonical(Agent::Claude);
    append(&path, "{\"type\":\"assistant\",\"message\":\"half");
    let (_, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    let before = rowids(&ctx, &key);
    let (_, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (1, 1));
    assert_eq!(rowids(&ctx, &key), before);
    let ctx = an_hour_later(&ctx, &path);
    let (_, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(codes(&result), ["malformed_records"]);
    assert!(result.incomplete);
    assert_eq!(rowids(&ctx, &key), before);
}

#[test]
fn unchanged_recent_torn_title_index_line_is_rechecked_until_it_counts_as_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    with_companion(&ctx, Agent::Codex);
    let names = &ctx.roots.codex_session_index;
    write(
        names,
        "{\"id\":\"codex-canonical\",\"thread_name\":\"First\"}\n{\"id\":\"zz-companion\",\"thr",
    );
    let (_, result) = refreshed(&ctx, &selected(Agent::Codex));
    assert_eq!(result.diagnostics, []);
    let (lease, result) = refreshed(&ctx, &selected(Agent::Codex));
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (2, 0));
    let session = lease.session(&canonical(Agent::Codex)).unwrap().unwrap();
    assert_eq!(session.title.as_deref(), Some("First"));
    drop(lease);
    let ctx = an_hour_later(&ctx, names);
    let (_, result) = refreshed(&ctx, &selected(Agent::Codex));
    assert_eq!(codes(&result), ["malformed_metadata"]);
}

#[test]
fn malformed_title_index_line_is_reported_while_sessions_are_reused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    with_companion(&ctx, Agent::Codex);
    let names = &ctx.roots.codex_session_index;
    write(
        names,
        "{\"id\":\"codex-canonical\",\"thread_name\":\"First\"}\n",
    );
    let (_, result) = refreshed(&ctx, &selected(Agent::Codex));
    assert_eq!(result.diagnostics, []);
    let before = rowids(&ctx, &canonical(Agent::Codex));
    append(names, "malformed\n");
    let ctx = an_hour_later(&ctx, names);
    for _ in 0..2 {
        let (lease, result) = refreshed(&ctx, &selected(Agent::Codex));
        assert!(result.incomplete);
        assert_eq!(codes(&result), ["malformed_metadata"]);
        assert_eq!(result.diagnostics[0].agent, Some(Agent::Codex));
        assert_eq!(counts(&result), (0, 2));
        let session = lease.session(&canonical(Agent::Codex)).unwrap().unwrap();
        assert_eq!(session.title.as_deref(), Some("First"));
    }
    assert_eq!(rowids(&ctx, &canonical(Agent::Codex)), before);
}

#[test]
fn unchanged_recent_torn_cursor_line_is_rechecked_until_it_counts_as_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = ctx
        .roots
        .cursor_projects
        .join("slug/agent-transcripts/main/main.jsonl");
    write(
        &path,
        &format!(
            "{}{{\"role\":\"assistant\",\"mess",
            include_str!("fixtures/jsonl/cursor.jsonl")
        ),
    );
    write(
        &ctx.roots.cursor_chats.join("hash/main/meta.json"),
        &json!({"cwd":"/fixture/project","title":"Cursor title","createdAtMs":1_786_356_360_000_i64}).to_string(),
    );
    let (_, result) = refreshed(&ctx, &selected(Agent::Cursor));
    assert_eq!(result.diagnostics, []);
    let (_, result) = refreshed(&ctx, &selected(Agent::Cursor));
    assert_eq!(result.diagnostics, []);
    assert_eq!(counts(&result), (1, 0));
    let ctx = an_hour_later(&ctx, &path);
    let (_, result) = refreshed(&ctx, &selected(Agent::Cursor));
    assert_eq!(codes(&result), ["malformed_records"]);
}

#[test]
fn partial_source_resumes_and_keeps_its_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = with_companion(&ctx, Agent::Claude);
    let key = canonical(Agent::Claude);
    append(&path, "not json\n");
    let (_, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(codes(&result), ["malformed_records"]);
    let before = rowids(&ctx, &key);
    let old_state = append_state(&ctx, &path);
    assert_eq!(old_state.malformed, 1);
    append(&path, &appended_records(Agent::Claude));
    let (lease, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(
        result
            .diagnostics
            .iter()
            .map(|d| (d.code.as_str(), d.message.as_str()))
            .collect::<Vec<_>>(),
        [(
            "malformed_records",
            "1 nonempty records were not valid JSON objects"
        )]
    );
    assert_eq!(counts(&result), (1, 1));
    let state = append_state(&ctx, &path);
    assert!(state.consumed > old_state.consumed);
    assert_eq!(state.consumed, fs::metadata(&path).unwrap().len());
    assert_eq!(state.malformed, 1);
    let resumed = rowids(&ctx, &key);
    assert!(resumed.len() > before.len());
    assert_eq!(&resumed[..before.len()], before.as_slice());
    let resumed_snapshot = snapshot(&lease, &key);
    drop(lease);
    let cold_dir = tempfile::tempdir().unwrap();
    let mut other = ctx.clone();
    other.index_dir = cold_dir.path().join("index");
    let (cold, result) = refreshed(&other, &selected(Agent::Claude));
    assert_eq!(codes(&result), ["malformed_records"]);
    assert_eq!(resumed_snapshot, snapshot(&cold, &key));
    let (_, result) = refreshed(&ctx, &selected(Agent::Claude));
    assert_eq!(codes(&result), ["malformed_records"]);
    assert_eq!(counts(&result), (0, 2));
    assert_eq!(rowids(&ctx, &key), resumed);
}

#[test]
fn query_resumes_spot_check_a_long_prefix_and_slimetrail_index_verifies_all_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let line =
        |text: &str| json!({"type":"user","sessionId":"long","message":text}).to_string() + "\n";
    let filler = "x".repeat(4000);
    let record = line(&filler);
    let count = (5 << 20) / record.len() + 1;
    let path = ctx.roots.claude_projects.join("project/long.jsonl");
    write(&path, &record.repeat(count));
    let agents = selected(Agent::Claude);
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    let offset = (2 << 20) + 100;
    let changed = offset + bytes[offset..].iter().position(|b| *b == b'x').unwrap();
    bytes[changed] = b'y';
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    append(&path, &line("appended"));
    let row = changed / record.len();
    let rewritten =
        String::from_utf8(bytes[row * record.len()..(row + 1) * record.len()].to_vec()).unwrap();
    let rewritten: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
    let stats = |result: Envelope<IndexStats>| {
        assert_eq!(codes(&result), [] as [&str; 0]);
        let stats = result.data.unwrap();
        (stats.reparsed, stats.reused)
    };
    assert_eq!(
        stats(lease.refresh(&agents, &RefreshOptions::QUERY).unwrap()),
        (1, 0)
    );
    let spot_checked = texts(&lease, &key("long"));
    assert_eq!(spot_checked.len(), count + 1);
    assert_eq!(spot_checked[row], filler);
    assert_eq!(
        stats(lease.refresh(&agents, &RefreshOptions::QUERY).unwrap()),
        (0, 1)
    );
    assert_eq!(
        stats(lease.refresh(&agents, &RefreshOptions::FULL).unwrap()),
        (1, 0)
    );
    let verified = texts(&lease, &key("long"));
    assert_eq!(verified[row], rewritten["message"].as_str().unwrap());
    assert_eq!(verified.last().map(String::as_str), Some("appended"));
    assert_eq!(
        stats(lease.refresh(&agents, &RefreshOptions::FULL).unwrap()),
        (0, 1)
    );
    lease.verify_integrity().unwrap();
}

#[test]
fn compact_scans_read_only_tool_result_tails_while_reads_return_the_whole_text() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let body = format!("headmark {} tailmark", "é".repeat(3000));
    let path = ctx.roots.claude_projects.join("project/tool.jsonl");
    write(
        &path,
        &(tool_result_line("tool", &body) + &tool_result_line("tool", "short tailmark")),
    );
    let agents = selected(Agent::Claude);
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    let tail_start = body.len() - 2047;
    assert!(body.is_char_boundary(tail_start));
    let stored: Vec<(i64, i64, i64)> = Connection::open(ctx.index_dir.join(DATABASE))
        .unwrap()
        .prepare("SELECT length(CAST(text AS BLOB)),tail_start,length(CAST(text_tail AS BLOB)) FROM entries ORDER BY entry_index")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let tail_start = i64::try_from(tail_start).unwrap();
    assert_eq!(stored, [(tail_start, tail_start, 2047), (0, 0, 14)]);
    let documents = |lease: &IndexSession, candidate| {
        let mut found = Vec::new();
        lease
            .visit_documents(&key("tool"), candidate, &mut |doc| {
                found.push(doc.text);
                Ok(())
            })
            .unwrap();
        found
    };
    let tail = &body[usize::try_from(tail_start).unwrap()..];
    for candidate in [None, Some(("tailmark", false)), Some(("é", false))] {
        assert_eq!(
            documents(&lease, candidate),
            [tail, "short tailmark"],
            "{candidate:?}"
        );
    }
    assert_eq!(
        texts(&lease, &key("tool")),
        [body.as_str(), "short tailmark"]
    );
    lease.verify_integrity().unwrap();
    drop(lease);
    let matched = |ctx: &Context, query: &str| {
        let options = SearchOptions {
            regex: true,
            ..query_options(query)
        };
        let result = search(ctx, &options, &RefreshOptions::QUERY).unwrap();
        result.data.unwrap().count
    };
    assert_eq!(
        (matched(&ctx, "headmark"), matched(&ctx, "tailmar[k]")),
        (0, 1)
    );
    let full = ctx.with_coverage(Coverage::Full, false);
    let mut lease = IndexSession::open(&full).unwrap();
    lease.refresh(&agents, &RefreshOptions::FULL).unwrap();
    assert_eq!(documents(&lease, None), [body.as_str(), "short tailmark"]);
    lease.verify_integrity().unwrap();
    drop(lease);
    assert_eq!(matched(&full, "headmark"), 1);
}

fn fts_counts(ctx: &Context) -> (String, i64, i64) {
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.query_row(
        "SELECT (SELECT sql FROM sqlite_schema WHERE name='search_fts'),(SELECT count(*) FROM search_documents),(SELECT count(*) FROM search_fts)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn tool_result_line(id: &str, text: &str) -> String {
    json!({"type":"user","sessionId":id,"message":{"content":[{"type":"tool_result","content":text}]}}).to_string() + "\n"
}

fn stats(ctx: &Context, agents: &BTreeSet<Agent>, options: &RefreshOptions<'_>) -> IndexStats {
    refresh(ctx, agents, options).unwrap().data.unwrap()
}

#[test]
fn contentless_fts_survives_cascaded_deletes_appends_and_coverage_flips() {
    let dir = tempfile::tempdir().unwrap();
    let compact = context(dir.path());
    let full = compact.clone().with_coverage(Coverage::Full, false);
    let agents = selected(Agent::Claude);
    let many = compact.roots.claude_projects.join("project/many.jsonl");
    let lines: String = (0..10_050)
        .map(|i| {
            json!({"type":"assistant","sessionId":"many","message":{"content":[
                {"type":"thinking","thinking":format!("thought {i}")},
                {"type":"text","text":format!("said {i}")}
            ]}})
            .to_string()
                + "\n"
        })
        .collect();
    write(&many, &lines);
    let tool = compact.roots.claude_projects.join("project/tool.jsonl");
    let long = format!("early {} late", "y".repeat(4096));
    write(&tool, &tool_result_line("tool", &long));
    let doomed = claude(&compact, "doomed", "short lived");
    refresh(&compact, &agents, &RefreshOptions::QUERY).unwrap();
    let (sql, documents, rows) = fts_counts(&compact);
    assert!(sql.contains("contentless_delete=1"), "{sql}");
    assert_eq!((documents, rows), (10_052, 10_052));
    IndexSession::open(&compact)
        .unwrap()
        .verify_integrity()
        .unwrap();
    fs::remove_file(&doomed).unwrap();
    assert_eq!(stats(&compact, &agents, &RefreshOptions::QUERY).removed, 1);
    let (_, documents, rows) = fts_counts(&compact);
    assert_eq!((documents, rows), (10_051, 10_051));
    IndexSession::open(&compact)
        .unwrap()
        .verify_integrity()
        .unwrap();
    assert_eq!(stats(&full, &agents, &RefreshOptions::FULL).reparsed, 0);
    let (_, documents, rows) = fts_counts(&full);
    assert_eq!((documents, rows), (20_101, 20_101));
    let lease = IndexSession::open(&full).unwrap();
    lease.verify_integrity().unwrap();
    assert_eq!(lease.coverage().unwrap(), Coverage::Full);
    drop(lease);
    append(&tool, &tool_result_line("tool", &format!("again {long}")));
    let full = an_hour_later(&full, &tool);
    assert_eq!(stats(&full, &agents, &RefreshOptions::QUERY).reparsed, 1);
    let mut lease = IndexSession::open(&full).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    let mut hits = Vec::new();
    lease
        .visit_documents(&key("tool"), Some(("early", false)), &mut |doc| {
            hits.push((doc.entry_index, doc.text.len()));
            Ok(())
        })
        .unwrap();
    assert_eq!(hits, [(Some(0), long.len()), (Some(1), long.len() + 6)]);
    lease.verify_integrity().unwrap();
    drop(lease);
    refresh(&compact, &agents, &RefreshOptions::FULL).unwrap();
    let mut lease = IndexSession::open(&compact).unwrap();
    lease.refresh(&agents, &RefreshOptions::QUERY).unwrap();
    let mut tails = Vec::new();
    lease
        .visit_documents(&key("tool"), None, &mut |doc| {
            tails.push(doc.text.len());
            Ok(())
        })
        .unwrap();
    assert_eq!(tails, [2048, 2048]);
    lease.verify_integrity().unwrap();
    drop(lease);
    let db = Connection::open(compact.index_dir.join(DATABASE)).unwrap();
    db.execute(
        "DELETE FROM search_fts WHERE rowid=(SELECT max(document_id) FROM search_documents)",
        [],
    )
    .unwrap();
    drop(db);
    assert!(
        IndexSession::open(&compact)
            .unwrap()
            .verify_integrity()
            .is_err()
    );
}

const fn gate(max_ms: u64, admission: Admission) -> RefreshOptions<'static> {
    RefreshOptions {
        build: BuildPolicy::Limit { max_ms, admission },
        ..RefreshOptions::QUERY
    }
}

const fn limited(max_ms: u64) -> RefreshOptions<'static> {
    gate(max_ms, Admission::PerAgent)
}

const fn all_or_nothing(max_ms: u64) -> RefreshOptions<'static> {
    gate(max_ms, Admission::AllOrNothing)
}

fn listed(ctx: &Context, refresh: &RefreshOptions<'_>, agents: &[Agent]) -> Envelope<Vec<Session>> {
    sessions(
        ctx,
        &Filter {
            agents: agents.iter().copied().collect(),
            cwd: ctx.cwd.clone(),
            scope: Scope::All,
            app: None,
            days: None,
            kinds: BTreeSet::new(),
        },
        refresh,
    )
    .unwrap()
}

fn ids(envelope: &Envelope<Vec<Session>>) -> Vec<String> {
    let data = envelope.data.as_ref().unwrap();
    data.iter()
        .map(|s| format!("{}:{}", s.key.agent, s.key.id))
        .collect()
}

fn large(ctx: &Context, id: &str) -> PathBuf {
    claude(ctx, id, &"large backlog text ".repeat(100_000))
}

fn index_rows(ctx: &Context) -> (i64, i64) {
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.query_row(
        "SELECT (SELECT count(*) FROM refresh_runs),(SELECT count(*) FROM sources)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn over_limit_first_build_reports_the_estimate_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let one = claude(&ctx, "one", "first text");
    let two = claude(&ctx, "two", "second text");
    let pi = transcript(&ctx, Agent::Pi, "session");
    write(&pi, &jsonl_fixture(Agent::Pi));
    let size = |path: &Path| fs::metadata(path).unwrap().len();
    let mut index = IndexSession::open(&ctx).unwrap();
    let Err(Error::IndexRequired(estimate)) = index.refresh(
        &BTreeSet::from([Agent::Claude, Agent::Pi]),
        &all_or_nothing(1),
    ) else {
        panic!("expected IndexRequired");
    };
    let found: Vec<_> = estimate
        .agents
        .iter()
        .map(|a| (a.agent, a.sessions, a.bytes))
        .collect();
    assert_eq!(
        found,
        [
            (Agent::Claude, 2, size(&one) + size(&two)),
            (Agent::Pi, 1, size(&pi))
        ]
    );
    assert_eq!(
        estimate.agents[1].compact.millis,
        size(&pi).div_ceil(30_000) + 2
    );
    drop(index);
    assert_eq!(index_rows(&ctx), (0, 0));
    let mut options = query_options("text");
    options.filter.agents = selected(Agent::Claude);
    let unavailable = search(&ctx, &options, &limited(1)).unwrap();
    assert!(unavailable.incomplete);
    let required: Vec<_> = unavailable
        .diagnostics
        .iter()
        .map(|d| (d.code.as_str(), d.agent))
        .collect();
    assert_eq!(required, [("index_required", Some(Agent::Claude))]);
    let data = unavailable.data.unwrap();
    assert_eq!((data.agents, data.count), (vec![Agent::Claude], 0));
    assert_eq!(index_rows(&ctx), (0, 0));
}

fn query_options(query: &str) -> SearchOptions {
    SearchOptions {
        filter: Filter {
            agents: BTreeSet::new(),
            cwd: "/".into(),
            scope: Scope::All,
            app: None,
            days: None,
            kinds: BTreeSet::new(),
        },
        query: query.into(),
        regex: false,
        case_mode: CaseMode::Insensitive,
        whole_word: false,
        sort: Sort::Score,
        max_results: 50,
        max_snippets: 3,
        context_chars: 80,
    }
}

#[test]
fn gated_agent_with_a_published_state_is_served_stale_while_small_agents_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "old", "published text");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    large(&ctx, "new");
    write(
        &transcript(&ctx, Agent::Pi, "session"),
        &jsonl_fixture(Agent::Pi),
    );
    let result = listed(&ctx, &limited(50), &[Agent::Claude, Agent::Pi]);
    assert!(result.incomplete);
    let required: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| (d.code.as_str(), d.agent))
        .collect();
    assert_eq!(required, [("index_required", Some(Agent::Claude))]);
    assert!(result.diagnostics[0].message.contains("slimetrail index"));
    assert_eq!(ids(&result), ["claude:old", "pi:pi-canonical"]);
    let built = listed(&ctx, &RefreshOptions::QUERY, &[Agent::Claude, Agent::Pi]);
    assert!(!built.incomplete, "{:?}", built.diagnostics);
    assert_eq!(ids(&built), ["claude:new", "claude:old", "pi:pi-canonical"]);
}

#[test]
fn whole_gate_requires_the_index_when_any_agent_is_over_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "old", "published text");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let new = large(&ctx, "new");
    let pi = transcript(&ctx, Agent::Pi, "session");
    write(&pi, &jsonl_fixture(Agent::Pi));
    let before = index_rows(&ctx);
    let full = ctx.clone().with_coverage(Coverage::Full, false);
    let Err(Error::IndexRequired(estimate)) = sessions(
        &full,
        &Filter {
            agents: BTreeSet::from([Agent::Claude, Agent::Pi]),
            cwd: ctx.cwd.clone(),
            scope: Scope::All,
            app: None,
            days: None,
            kinds: BTreeSet::new(),
        },
        &all_or_nothing(50),
    ) else {
        panic!("expected IndexRequired");
    };
    let pending: Vec<_> = estimate
        .agents
        .iter()
        .map(|a| (a.agent, a.sessions, a.bytes))
        .collect();
    assert_eq!(
        pending,
        [
            (Agent::Claude, 1, fs::metadata(&new).unwrap().len()),
            (Agent::Pi, 1, fs::metadata(&pi).unwrap().len())
        ]
    );
    assert_eq!(
        (estimate.coverage, estimate.coverage_locked),
        (slimetrail::Coverage::Compact, true)
    );
    assert_eq!(index_rows(&ctx), before);
    assert_eq!(
        ids(&listed(&ctx, &all_or_nothing(50), &[Agent::Pi])),
        ["pi:pi-canonical"]
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn incremental_backlogs_are_gated_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "old", "published text");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    claude(&ctx, "small", "small backlog");
    let small = listed(&ctx, &limited(10), &[Agent::Claude]);
    assert!(!small.incomplete, "{:?}", small.diagnostics);
    assert_eq!(ids(&small), ["claude:old", "claude:small"]);
    large(&ctx, "large");
    let gated = listed(&ctx, &limited(10), &[Agent::Claude]);
    assert!(gated.incomplete);
    assert_eq!(gated.diagnostics[0].code, "index_required");
    assert_eq!(ids(&gated), ["claude:old", "claude:small"]);
    let estimate = IndexSession::open(&ctx)
        .unwrap()
        .estimate(&selected(Agent::Claude))
        .unwrap();
    assert_eq!(estimate.agents.len(), 1);
    assert_eq!(estimate.agents[0].sessions, 1);
}

#[cfg(feature = "test-hooks")]
#[test]
fn observed_throughput_is_recorded_per_coverage_and_drives_later_estimates() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first text");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    let observed = |db: &Connection| -> Vec<(String, u64, String)> {
        db.prepare("SELECT coverage,bytes,typeof(micros) FROM throughput WHERE agent='claude'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(observed(&db), []);
    let two = claude(&ctx, "two", "second text");
    let bytes = fs::metadata(&two).unwrap().len();
    let estimate = IndexSession::open(&ctx)
        .unwrap()
        .estimate(&selected(Agent::Claude))
        .unwrap();
    assert_eq!(
        estimate.agents[0].compact.millis,
        bytes.div_ceil(31_000) + 2
    );
    db.execute(
        "INSERT INTO throughput(agent,coverage,bytes,micros) VALUES('claude','compact',67108864,67108864000)",
        [],
    )
    .unwrap();
    let estimate = IndexSession::open(&ctx)
        .unwrap()
        .estimate(&selected(Agent::Claude))
        .unwrap();
    assert_eq!(estimate.agents[0].compact.millis, bytes + 2);
    assert_eq!(estimate.agents[0].full.millis, bytes.div_ceil(14_000) + 2);
    db.execute(
        "INSERT INTO throughput(agent,coverage,bytes,micros) VALUES('claude','full',67108864,134217728000)",
        [],
    )
    .unwrap();
    let estimate = IndexSession::open(&ctx)
        .unwrap()
        .estimate(&selected(Agent::Claude))
        .unwrap();
    assert_eq!(estimate.agents[0].full.millis, 2 * bytes + 2);
    assert_eq!(estimate.agents[0].compact.millis, bytes + 2);
    let gated = listed(&ctx, &limited(bytes), &[Agent::Claude]);
    assert_eq!(gated.diagnostics[0].code, "index_required");
    assert_eq!(ids(&gated), ["claude:one"]);
    let admitted = listed(&ctx, &limited(bytes + 2), &[Agent::Claude]);
    assert!(!admitted.incomplete, "{:?}", admitted.diagnostics);
    assert_eq!(ids(&admitted), ["claude:one", "claude:two"]);

    let full_dir = tempfile::tempdir().unwrap();
    let full = context_at(full_dir.path())
        .cwd(full_dir.path())
        .coverage(Coverage::Full)
        .build();
    let three = large(&full, "three");
    refresh(&full, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let db = Connection::open(full.index_dir.join(DATABASE)).unwrap();
    assert_eq!(
        observed(&db),
        [(
            "full".into(),
            fs::metadata(&three).unwrap().len(),
            "integer".into()
        )]
    );
}

fn throughput_row(ctx: &Context) -> Option<(u64, u64)> {
    Connection::open(ctx.index_dir.join(DATABASE))
        .unwrap()
        .query_row(
            "SELECT bytes,micros FROM throughput WHERE agent='claude' AND coverage='compact'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

#[test]
fn learned_throughput_counts_only_ingests_of_at_least_one_mebibyte() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let agents = selected(Agent::Claude);
    let first = large(&ctx, "first");
    let first_bytes = fs::metadata(&first).unwrap().len();
    assert!(first_bytes >= 1 << 20);
    refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    let (bytes, micros) = throughput_row(&ctx).unwrap();
    assert_eq!(bytes, first_bytes);
    assert!(micros > 0);
    for round in 0..4 {
        append(
            &first,
            &format!(
                "{}\n",
                json!({"type":"user","sessionId":"first","message":{"content":format!("tail {round}")}})
            ),
        );
        claude(&ctx, &format!("small-{round}"), "small session");
        let stats = refresh(&ctx, &agents, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap();
        assert_eq!(stats.reparsed, 2);
        assert_eq!(throughput_row(&ctx), Some((bytes, micros)));
    }
    let second = large(&ctx, "second");
    refresh(&ctx, &agents, &RefreshOptions::QUERY).unwrap();
    let (summed, more) = throughput_row(&ctx).unwrap();
    assert_eq!(summed, first_bytes + fs::metadata(&second).unwrap().len());
    assert!(more > micros);
}

#[cfg(feature = "test-hooks")]
#[test]
fn small_appends_to_large_indexed_transcripts_are_costed_as_hashing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = large(&ctx, "big");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let consumed = fs::metadata(&path).unwrap().len();
    fs::File::options()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"type":"user","sessionId":"big","cwd":"/fixture/project","message":{"content":"appended text"}})
            )
            .as_bytes(),
        )
        .unwrap();
    let size = fs::metadata(&path).unwrap().len();
    let full_parse = size.div_ceil(31_000) + 2;
    let limit = 20;
    assert!(full_parse > limit, "{full_parse}");
    let estimate = IndexSession::open(&ctx)
        .unwrap()
        .estimate(&selected(Agent::Claude))
        .unwrap();
    assert_eq!(estimate.agents[0].bytes, size - consumed);
    assert_eq!(
        estimate.agents[0].compact.index_bytes,
        ((size - consumed) * 64).div_ceil(100)
    );
    assert_eq!(
        estimate.agents[0].compact.millis,
        (size - consumed).div_ceil(31_000) + consumed.div_ceil(1_000_000) + 2
    );
    let mut index = IndexSession::open(&ctx).unwrap();
    let admitted = index
        .refresh(&selected(Agent::Claude), &limited(limit))
        .unwrap();
    assert!(!admitted.incomplete, "{:?}", admitted.diagnostics);
    assert_eq!(admitted.data.unwrap().reparsed, 1);
    assert_eq!(texts(&index, &key("big")).last().unwrap(), "appended text");
}

#[cfg(feature = "test-hooks")]
#[test]
fn first_build_after_a_refused_query_takes_the_chosen_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "first text");
    let mut refused = IndexSession::open(&ctx).unwrap();
    let Err(Error::IndexRequired(_)) =
        refused.refresh(&selected(Agent::Claude), &all_or_nothing(0))
    else {
        panic!("expected IndexRequired");
    };
    drop(refused);
    let stored = |ctx: &Context| -> String {
        Connection::open(ctx.index_dir.join(DATABASE))
            .unwrap()
            .query_row("SELECT coverage FROM settings", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(stored(&ctx), "compact");
    let full = ctx.clone().with_coverage(Coverage::Full, false);
    let unlocked = |ctx: &Context| {
        let estimate = IndexSession::open(ctx)
            .unwrap()
            .estimate(&selected(Agent::Claude))
            .unwrap();
        (estimate.coverage, estimate.coverage_locked)
    };
    assert_eq!(unlocked(&full), (Coverage::Full, false));
    assert_eq!(unlocked(&ctx), (Coverage::Compact, false));
    let built = listed(&full, &RefreshOptions::QUERY, &[Agent::Claude]);
    assert!(!built.incomplete, "{:?}", built.diagnostics);
    assert_eq!(stored(&ctx), "full");
    assert_eq!(
        IndexSession::open(&ctx).unwrap().coverage().unwrap(),
        Coverage::Full
    );
    assert_eq!(unlocked(&ctx), (Coverage::Full, true));
}

#[cfg(feature = "test-hooks")]
#[test]
fn shared_database_bytes_are_split_across_its_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
    let writer = Connection::open(&ctx.roots.opencode_db).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('one','/fixture','A',1,2),('two','/fixture','B',1,2),('three','/fixture','C',1,2);").unwrap();
    let expected: u64 = slimetrail::sources::sqlite::dependencies(&ctx.roots.opencode_db)
        .iter()
        .map(|path| fs::metadata(path).map_or(0, |m| m.len()).div_ceil(3) * 3)
        .sum();
    let estimate = IndexSession::open(&ctx)
        .unwrap()
        .estimate(&selected(Agent::Opencode))
        .unwrap();
    assert_eq!(estimate.agents.len(), 1);
    assert_eq!(estimate.agents[0].sessions, 3);
    assert_eq!(estimate.agents[0].bytes, expected);
    assert!(expected > 0);
}

#[test]
fn integrity_check_rejects_fts_text_that_no_longer_matches_its_document() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "genuine searchable words");
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    IndexSession::open(&ctx)
        .unwrap()
        .verify_integrity()
        .unwrap();
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    let id: i64 = db
        .query_row(
            "SELECT document_id FROM search_documents WHERE entry_index IS NOT NULL LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute("DELETE FROM search_fts WHERE rowid=?1", [id])
        .unwrap();
    db.execute(
        "INSERT INTO search_fts(rowid,ascii_lower) VALUES(?1,'unrelated words')",
        [id],
    )
    .unwrap();
    drop(db);
    assert!(
        IndexSession::open(&ctx)
            .unwrap()
            .verify_integrity()
            .is_err()
    );
}

#[test]
fn integrity_check_samples_large_indexes_reproducibly() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "many", "entry text 0");
    let more: String = (1..300)
        .map(|i| {
            json!({"type":"user","sessionId":"many","cwd":"/fixture/project","message":{"content":format!("entry text {i}")}})
                .to_string()
                + "\n"
        })
        .collect();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(more.as_bytes())
        .unwrap();
    refresh(&ctx, &selected(Agent::Claude), &RefreshOptions::QUERY).unwrap();
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    let (first, documents): (i64, i64) = db
        .query_row(
            "SELECT min(document_id),count(*) FROM search_documents",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(documents >= 300, "{documents}");
    db.execute("DELETE FROM search_fts WHERE rowid=?1", [first])
        .unwrap();
    db.execute(
        "INSERT INTO search_fts(rowid,ascii_lower) VALUES(?1,'unrelated words')",
        [first],
    )
    .unwrap();
    drop(db);
    let index = IndexSession::open(&ctx).unwrap();
    for _ in 0..5 {
        assert!(index.verify_integrity().is_err());
    }
}

#[test]
fn an_interrupted_first_build_keeps_its_stored_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let compact = context(dir.path());
    let full = compact.clone().with_coverage(Coverage::Full, false);
    claude(&compact, "one", "first text");
    let agents = selected(Agent::Claude);
    refresh(&compact, &agents, &RefreshOptions::QUERY).unwrap();
    let db = Connection::open(compact.index_dir.join(DATABASE)).unwrap();
    db.execute("UPDATE refresh_runs SET published=0", [])
        .unwrap();
    drop(db);
    claude(&compact, "two", "second text");
    refresh(&full, &agents, &RefreshOptions::QUERY).unwrap();
    let session = IndexSession::open(&full).unwrap();
    assert_eq!(session.coverage().unwrap(), Coverage::Compact);
    session.verify_integrity().unwrap();
}
