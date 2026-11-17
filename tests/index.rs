use rusqlite::Connection;
use serde_json::json;
use slimetrail::{
    index::{self, DATABASE, IndexSession},
    *,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::Duration,
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
        cwd: base.into(),
        now_ms: 0,
        time_zone: "UTC".into(),
    }
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
    let first_run = index::refresh(&ctx, &agents).unwrap();
    assert!(!first_run.incomplete);
    assert_eq!(first_run.data.unwrap().reparsed, 2);
    let unchanged = index::refresh(&ctx, &agents).unwrap().data.unwrap();
    assert_eq!((unchanged.reparsed, unchanged.reused), (0, 2));
    claude(&ctx, "one", "replacement content");
    let mut lease = IndexSession::open(&ctx).unwrap();
    let changed = lease.refresh_under_lease(&agents).unwrap().data.unwrap();
    assert_eq!((changed.reparsed, changed.reused), (1, 1));
    assert_eq!(texts(&lease, &key("one")), ["replacement content"]);
    lease.verify_integrity().unwrap();
    fs::remove_file(first).unwrap();
    let removed = lease.refresh_under_lease(&agents).unwrap().data.unwrap();
    assert_eq!(removed.removed, 1);
    assert!(lease.session(&key("one")).unwrap().is_none());
    lease.verify_integrity().unwrap();
}

#[test]
fn stable_partial_records_remain_incomplete_until_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(&ctx, "one", "good record");
    let good = fs::read_to_string(&path).unwrap();
    write(&path, &format!("{good}{{\"type\":"));
    let mut lease = IndexSession::open(&ctx).unwrap();
    for _ in 0..2 {
        let result = lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
        assert!(result.incomplete);
        assert_eq!(result.data.unwrap().reparsed, 1);
        assert_eq!(texts(&lease, &key("one")), ["good record"]);
    }
    write(
        &path,
        &format!(
            "{good}{}\n",
            json!({"type":"assistant","message":"completed record"})
        ),
    );
    assert!(
        !lease
            .refresh_under_lease(&selected(Agent::Claude))
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
fn unavailable_inventory_excludes_and_retains_old_rows() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(&ctx, "one", "prior text");
    index::refresh(&ctx, &selected(Agent::Claude)).unwrap();
    let moved = dir.path().join("temporarily-moved");
    fs::rename(&ctx.roots.claude_projects, &moved).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    let result = lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
    assert!(result.incomplete);
    assert_eq!(result.data.unwrap().removed, 0);
    assert!(texts(&lease, &key("one")).is_empty());
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
            .refresh_under_lease(&selected(Agent::Claude))
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
    index::refresh(&ctx, &selected(Agent::Claude)).unwrap();
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
    lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
    assert!(lease.session(&key("one")).unwrap().is_none());
    assert_eq!(texts(&lease, &key("new")), ["second"]);
    write(&path, "");
    lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
    assert!(lease.session(&key("new")).unwrap().is_none());
    assert!(texts(&lease, &key("one")).is_empty());
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
    index::refresh(&ctx, &selected(Agent::Claude)).unwrap();
    let mut other = ctx.clone();
    other.roots.claude_projects = dir.path().join("second-root");
    claude(&other, "one", "second root");
    let mut lease = IndexSession::open(&other).unwrap();
    assert_eq!(
        lease
            .refresh_under_lease(&selected(Agent::Claude))
            .unwrap()
            .data
            .unwrap()
            .reparsed,
        1
    );
    assert_eq!(texts(&lease, &key("one")), ["second root"]);
    drop(lease);
    index::refresh(&ctx, &selected(Agent::Pi)).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
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

#[test]
fn private_modes_and_exact_older_version_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    drop(IndexSession::open(&ctx).unwrap());
    for name in [
        "index-v0.sqlite3",
        "index-v0.sqlite3-wal",
        "index-v3.sqlite3",
        "index-v00.sqlite3",
        "index-v0.sqlite3.bak",
    ] {
        let path = ctx.index_dir.join(name);
        fs::write(&path, "sentinel").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(IndexSession::open(&ctx).unwrap());
    assert!(!ctx.index_dir.join("index-v0.sqlite3").exists());
    assert!(!ctx.index_dir.join("index-v0.sqlite3-wal").exists());
    for name in [
        "index-v3.sqlite3",
        "index-v00.sqlite3",
        "index-v0.sqlite3.bak",
    ] {
        assert_eq!(
            fs::read_to_string(ctx.index_dir.join(name)).unwrap(),
            "sentinel"
        );
    }
    for name in [DATABASE, "lifecycle.lock"] {
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
fn owned_staging_cleanup_respects_other_index_lease() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let first = IndexSession::open(&ctx).unwrap();
    let staging = ctx.index_dir.join("staging/slimetrail-snapshot-abandoned");
    fs::create_dir(&staging).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
    write(&staging.join("snapshot.db"), "sentinel");
    assert!(matches!(
        IndexSession::open_with_timeout(&ctx, Duration::ZERO),
        Err(Error::IndexBusy)
    ));
    let mut other = ctx.clone();
    other.index_dir = dir.path().join("other-index");
    let second = IndexSession::open(&other).unwrap();
    assert_eq!(
        fs::read_to_string(staging.join("snapshot.db")).unwrap(),
        "sentinel"
    );
    drop(first);
    drop(IndexSession::open(&ctx).unwrap());
    assert!(!staging.exists());
    drop(second);
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
    lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
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
        index::refresh(&ctx, &agents)
            .unwrap()
            .data
            .unwrap()
            .reparsed,
        2
    );
    assert_eq!(
        index::refresh(&ctx, &agents).unwrap().data.unwrap().reused,
        2
    );
    let db = Connection::open(ctx.index_dir.join(DATABASE)).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON; DELETE FROM sources WHERE id='two';")
        .unwrap();
    drop(db);
    let recovered = index::refresh(&ctx, &agents).unwrap();
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
            .refresh_under_lease(&agents)
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
    assert_eq!(
        fs::read_dir(ctx.index_dir.join("staging")).unwrap().count(),
        0
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
    lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
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
    lease.refresh_under_lease(&selected(Agent::Pi)).unwrap();
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
    index::refresh(&ctx, &selected(Agent::Claude)).unwrap();
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
    index::refresh(&ctx, &agents).unwrap();
    write(
        &ctx.roots.codex_session_index,
        "{\"id\":\"one\",\"thread_name\":\"Later title\"}\n",
    );
    let mut lease = IndexSession::open(&ctx).unwrap();
    assert_eq!(
        lease
            .refresh_under_lease(&agents)
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
    index::refresh(&ctx, &selected(Agent::Claude)).unwrap();
    let moved = dir.path().join("other-store");
    fs::rename(&ctx.roots.claude_projects, &moved).unwrap();
    symlink(&moved, &ctx.roots.claude_projects).unwrap();
    let mut lease = IndexSession::open(&ctx).unwrap();
    let result = lease.refresh_under_lease(&selected(Agent::Claude)).unwrap();
    assert!(result.incomplete);
    assert!(texts(&lease, &key("one")).is_empty());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        fs::read_to_string(moved.join("project/one.jsonl")).unwrap()
    );
}

#[test]
fn native_roots_below_index_are_rejected_before_cleanup() {
    for field in ["claude_projects", "opencode_db", "codex_session_index"] {
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
    lease.refresh_under_lease(&agents).unwrap();
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
    lease.refresh_under_lease(&agents).unwrap();
    fs::remove_file(&replacement).unwrap();
    lease.refresh_under_lease(&agents).unwrap();
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
    assert!(lease.refresh_under_lease(&agents).unwrap().incomplete);
    assert_eq!(texts(&lease, &key("one")), ["new winner"]);
    fs::remove_file(&replacement).unwrap();
    fs::remove_file(ctx.roots.archive.as_ref().unwrap()).unwrap();
    let restored = lease.refresh_under_lease(&agents).unwrap();
    assert!(!restored.incomplete);
    assert_eq!(restored.data.unwrap().reparsed, 1);
    assert_eq!(texts(&lease, &key("one")), ["original winner"]);
    assert!(lease.session(&key("one")).unwrap().is_some());
    assert_eq!(
        lease
            .refresh_under_lease(&agents)
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
        let result = index.refresh_under_lease(&selected(Agent::Claude)).unwrap();
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
