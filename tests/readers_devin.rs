#[allow(dead_code)]
mod support;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use slimetrail::{
    index::{self, IndexSession},
    sources::{
        CachedSource, Discovery, DiscoveryCache, ParsedSession, RawSession, Source, source_for,
    },
    *,
};
use std::{collections::BTreeSet, fs, path::Path};
use support::context_at;

fn context(base: &Path) -> Context {
    context_at(base).without_codex_home().build()
}

fn message(role: &str, source: &str, content: &str) -> Value {
    json!({
        "role": role,
        "content": content,
        "metadata": {
            "created_at": "2026-08-10T10:00:00Z",
            "is_user_input": (source == "user" || source == "subagent_task").then_some(true),
            "telemetry": {"source": source},
        },
    })
}

fn store(ctx: &Context) -> Connection {
    fs::create_dir_all(ctx.roots.devin_sessions_db.parent().unwrap()).unwrap();
    let connection = Connection::open(&ctx.roots.devin_sessions_db).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE sessions(id TEXT PRIMARY KEY, working_directory TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER);
            CREATE TABLE message_nodes(row_id INTEGER PRIMARY KEY, session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, created_at INTEGER, metadata TEXT);
            CREATE TABLE subagent_heads(session_id TEXT, agent_id TEXT, chain_node_id INTEGER, updated_at INTEGER, PRIMARY KEY(session_id, agent_id)) WITHOUT ROWID;",
        )
        .unwrap();
    connection
}

fn node(
    connection: &Connection,
    session: &str,
    id: i64,
    parent: Option<i64>,
    chat: &Value,
    metadata: &Value,
) {
    connection
        .execute(
            "INSERT INTO message_nodes(session_id, node_id, parent_node_id, chat_message, created_at, metadata) VALUES(?,?,?,?,?,?)",
            params![session, id, parent, chat.to_string(), 1_786_356_000_i64 + id, metadata.to_string()],
        )
        .unwrap();
}

#[expect(
    clippy::too_many_lines,
    reason = "One table mirrors the observed node forest"
)]
fn main_session(connection: &Connection, id: &str) {
    connection
        .execute(
            "INSERT INTO sessions VALUES(?, '/fixture/project', 'Devin title', 1786356000, 1786356300, 14, 0)",
            [id],
        )
        .unwrap();
    let prefix = json!({"is_system_prefix": true});
    let none = json!({});
    let copy = |ids: &[i64]| json!({"extensions": {"compact/prior_node_ids": ids}});
    let mut assistant = message("assistant", "assistant", "");
    assistant["thinking"] = json!({"thinking": "Consider paths", "signature": "sig"});
    assistant["tool_calls"] =
        json!([{"id": "t1", "name": "exec", "arguments": {"command": "echo needle"}}]);
    let mut result = message("tool", "tool_result", "needle result");
    result["tool_call_id"] = json!("t1");
    for (node_id, parent, chat, metadata) in [
        (
            0,
            None,
            message("system", "sysprompt", "System prompt"),
            prefix.clone(),
        ),
        (
            1,
            Some(0),
            message("user", "user", "needle devin café 🦀"),
            none.clone(),
        ),
        (
            2,
            None,
            message("system", "sysprompt", "System prompt"),
            prefix,
        ),
        (
            3,
            Some(2),
            message("user", "user", "needle devin café 🦀"),
            copy(&[1]),
        ),
        (
            4,
            Some(3),
            message("assistant", "assistant", "Placeholder"),
            none.clone(),
        ),
        (5, Some(3), assistant, none.clone()),
        (6, Some(5), result, none.clone()),
        (
            7,
            Some(6),
            message("assistant", "assistant", "Done before compaction"),
            none.clone(),
        ),
        (
            8,
            Some(2),
            message("system", "compaction", "Summary of earlier work"),
            json!({"summarized_from": 7}),
        ),
        (
            16,
            Some(8),
            message("assistant", "assistant", "Done before compaction"),
            copy(&[7]),
        ),
        (
            9,
            Some(16),
            message("user", "user", "Second prompt"),
            none.clone(),
        ),
        (
            10,
            Some(9),
            message("assistant", "assistant", "Second answer"),
            none.clone(),
        ),
        (
            11,
            Some(10),
            message("user", "user", "Reverted prompt"),
            none.clone(),
        ),
        (
            12,
            Some(10),
            message("assistant", "assistant", "Partial essay"),
            none.clone(),
        ),
        (
            13,
            Some(12),
            message("system", "response_interrupted", "Canceled"),
            none.clone(),
        ),
        (
            14,
            Some(13),
            message("system", "skills", "Skill listing"),
            none.clone(),
        ),
        (
            20,
            None,
            message("system", "sysprompt", "Subagent prompt"),
            none.clone(),
        ),
        (
            21,
            Some(20),
            message("user", "subagent_task", "Read notes.txt"),
            none.clone(),
        ),
        (
            22,
            Some(21),
            message("assistant", "assistant", "child answer needle"),
            none,
        ),
    ] {
        node(connection, id, node_id, parent, &chat, &metadata);
    }
}

fn discover(source: &dyn Source, ctx: &Context) -> Discovery {
    source.discover(ctx, &DiscoveryCache::default()).unwrap()
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

fn parsed(ctx: &Context, id: &str) -> (ParsedSession, Vec<Entry>) {
    let source = source_for(ctx, Agent::Devin).unwrap();
    let found = discover(source.as_ref(), ctx);
    let raw = found.sessions.iter().find(|r| r.key.id == id).unwrap();
    parse(source.as_ref(), raw)
}

fn shape(entries: &[Entry]) -> Vec<(String, Kind, Option<String>, bool, String)> {
    entries
        .iter()
        .map(|e| {
            (
                e.role.clone(),
                e.kind,
                e.subtype.clone(),
                e.automated,
                e.text.clone(),
            )
        })
        .collect()
}

#[test]
fn devin_reads_live_chain_and_restores_compacted_history() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    main_session(&store(&ctx), "main");
    let (parsed, entries) = parsed(&ctx, "main");
    assert!(parsed.complete, "{:?}", parsed.diagnostics);
    assert_eq!(parsed.session.title.as_deref(), Some("Devin title"));
    assert_eq!(parsed.session.title_origin, Some(TitleOrigin::Native));
    assert_eq!(
        parsed.session.cwd.as_deref(),
        Some(Path::new("/fixture/project"))
    );
    assert_eq!(parsed.session.started_at, Some(1_786_356_000_000));
    assert_eq!(parsed.session.source_updated_at, Some(1_786_356_300_000));
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.searchable == Searchable::ToolResult)
            .map(|e| e.subtype.as_deref())
            .collect::<Vec<_>>(),
        [Some("tool_result")]
    );
    assert!(
        entries
            .iter()
            .all(|e| matches!(e.searchable, Searchable::Text | Searchable::ToolResult))
    );
    assert_eq!(
        shape(&entries),
        [
            ("user", Kind::Message, None, false, "needle devin café 🦀"),
            (
                "assistant",
                Kind::Meta,
                Some("reasoning"),
                false,
                "Consider paths"
            ),
            (
                "assistant",
                Kind::Tool,
                Some("tool_call"),
                false,
                "exec\n{\"command\": \"echo needle\"}"
            ),
            (
                "tool",
                Kind::Tool,
                Some("tool_result"),
                false,
                "needle result"
            ),
            (
                "assistant",
                Kind::Message,
                None,
                false,
                "Done before compaction"
            ),
            (
                "meta",
                Kind::Meta,
                Some("compaction"),
                true,
                "Summary of earlier work"
            ),
            ("user", Kind::Message, None, false, "Second prompt"),
            ("assistant", Kind::Message, None, false, "Second answer"),
            ("assistant", Kind::Message, None, false, "Partial essay"),
            ("meta", Kind::Meta, Some("interrupted"), true, "Canceled"),
        ]
        .map(|(role, kind, subtype, automated, text)| (
            role.to_string(),
            kind,
            subtype.map(str::to_string),
            automated,
            text.to_string()
        ))
    );
    assert_eq!(entries[2].tool_name.as_deref(), Some("exec"));
    assert_eq!(entries[0].timestamp, Some(1_786_356_000_000));
    assert_eq!(entries[0].source_id, "node:3:0");
    assert_eq!(
        entries.iter().map(|e| e.index).collect::<Vec<_>>(),
        (0..10).collect::<Vec<_>>()
    );
}

#[test]
fn devin_subagent_chain_is_an_automated_child_session() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = store(&ctx);
    main_session(&connection, "main");
    connection
        .execute(
            "INSERT INTO subagent_heads VALUES('main', 'agent1', 22, 1786356250)",
            [],
        )
        .unwrap();
    let (child, entries) = parsed(&ctx, "main/agent1");
    assert!(child.complete);
    assert_eq!(
        child.session.parent,
        Some(SessionKey {
            agent: Agent::Devin,
            id: "main".into()
        })
    );
    assert_eq!(child.session.title.as_deref(), Some("Read notes.txt"));
    assert_eq!(child.session.title_origin, Some(TitleOrigin::Derived));
    assert_eq!(child.session.source_updated_at, Some(1_786_356_250_000));
    assert_eq!(
        shape(&entries),
        [
            (
                "user".to_string(),
                Kind::Message,
                None,
                true,
                "Read notes.txt".to_string()
            ),
            (
                "assistant".to_string(),
                Kind::Message,
                None,
                true,
                "child answer needle".to_string()
            ),
        ]
    );
}

#[test]
fn devin_fork_copy_is_an_independent_session_with_shared_history() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = store(&ctx);
    main_session(&connection, "main");
    main_session(&connection, "fork");
    connection
        .execute(
            "UPDATE sessions SET title='Devin title (fork)' WHERE id='fork'",
            [],
        )
        .unwrap();
    let (fork, entries) = parsed(&ctx, "fork");
    assert_eq!(fork.session.parent, None);
    assert_eq!(fork.session.title.as_deref(), Some("Devin title (fork)"));
    assert_eq!(entries[0].text, "needle devin café 🦀");
    assert_eq!(shape(&entries), shape(&parsed(&ctx, "main").1));
}

#[test]
fn devin_untitled_session_derives_title_from_first_human_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = store(&ctx);
    main_session(&connection, "main");
    connection
        .execute("UPDATE sessions SET title='  '", [])
        .unwrap();
    let (parsed, _) = parsed(&ctx, "main");
    assert_eq!(
        parsed.session.title.as_deref(),
        Some("needle devin café 🦀")
    );
    assert_eq!(parsed.session.title_origin, Some(TitleOrigin::Derived));
}

#[test]
fn devin_reuses_unchanged_snapshot_and_rereads_changes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = store(&ctx);
    main_session(&connection, "main");
    let source = source_for(&ctx, Agent::Devin).unwrap();
    let initial = discover(source.as_ref(), &ctx);
    let cache = DiscoveryCache {
        by_source: initial
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
    };
    let unchanged = source.discover(&ctx, &cache).unwrap();
    assert!(unchanged.sessions.iter().all(|r| r.snapshot.is_none()));
    connection
        .execute("UPDATE sessions SET title='Renamed'", [])
        .unwrap();
    let changed = source.discover(&ctx, &cache).unwrap();
    assert!(changed.sessions.iter().all(|r| r.snapshot.is_some()));
    assert_eq!(
        parse(source.as_ref(), &changed.sessions[0])
            .0
            .session
            .title
            .as_deref(),
        Some("Renamed")
    );
}

#[test]
fn devin_malformed_and_unlinked_nodes_are_diagnosed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = store(&ctx);
    main_session(&connection, "main");
    connection
        .execute_batch(
            "UPDATE message_nodes SET chat_message='private malformed payload' WHERE node_id=10;
            UPDATE message_nodes SET parent_node_id=99 WHERE node_id=2;",
        )
        .unwrap();
    let (parsed, entries) = parsed(&ctx, "main");
    assert!(!parsed.complete);
    assert_eq!(parsed.diagnostics[0].code, "malformed_records");
    assert_eq!(entries[0].text, "needle devin café 🦀");
    assert!(
        entries
            .iter()
            .all(|e| e.text != "private malformed payload")
    );
}

#[test]
fn devin_missing_store_and_unsupported_schema_are_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let source = source_for(&ctx, Agent::Devin).unwrap();
    let missing = discover(source.as_ref(), &ctx);
    assert!(missing.complete_agents.is_empty());
    assert_eq!(missing.diagnostics[0].code, "source_unavailable");
    fs::create_dir_all(ctx.roots.devin_sessions_db.parent().unwrap()).unwrap();
    Connection::open(&ctx.roots.devin_sessions_db)
        .unwrap()
        .execute_batch("CREATE TABLE sessions(private TEXT);")
        .unwrap();
    let broken = discover(source.as_ref(), &ctx);
    assert!(broken.complete_agents.is_empty());
    assert_eq!(
        broken.diagnostics[0].message,
        "Devin sessions database could not be read consistently or has an unsupported schema"
    );
}

#[test]
fn devin_subagent_child_is_searchable_but_excluded_from_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = store(&ctx);
    main_session(&connection, "main");
    connection
        .execute(
            "INSERT INTO subagent_heads VALUES('main', 'agent1', 22, 1786356250)",
            [],
        )
        .unwrap();
    let filter = Filter {
        agents: [Agent::Devin].into(),
        cwd: "/fixture/project".into(),
        scope: Scope::All,
        app: None,
        days: None,
        kinds: [Kind::Message, Kind::Tool, Kind::Meta].into(),
    };
    let hits = search(
        &ctx,
        &SearchOptions {
            filter: filter.clone(),
            query: "child answer".into(),
            regex: false,
            case_sensitive: false,
            sort: Sort::Score,
            max_results: 10,
            max_snippets: 1,
            context_chars: 40,
        },
        &RefreshOptions::QUERY,
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(hits.results[0].session.key.id, "main/agent1");
    let result = timeline(
        &ctx,
        &TimelineOptions {
            filter,
            since: 1_786_320_000_000,
            until: 1_786_406_400_000,
            idle_ms: 1_800_000,
        },
        &RefreshOptions::QUERY,
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(
        result
            .sessions
            .iter()
            .map(|s| s.session.key.id.as_str())
            .collect::<Vec<_>>(),
        ["main"]
    );
    assert_eq!(result.sessions[0].spans[0].human_prompts, 2);
}

fn devin() -> BTreeSet<Agent> {
    BTreeSet::from([Agent::Devin])
}

fn refreshed(ctx: &Context, options: &RefreshOptions<'_>) -> (u64, u64, u64, u64) {
    let result = refresh(ctx, &devin(), options).unwrap();
    assert_eq!(result.diagnostics, []);
    let stats = result.data.unwrap();
    (
        stats.discovered,
        stats.reused,
        stats.reparsed,
        stats.removed,
    )
}

fn queried(ctx: &Context) -> (IndexSession, IndexStats) {
    let mut index = IndexSession::open(ctx).unwrap();
    let result = index.refresh(&devin(), &RefreshOptions::QUERY).unwrap();
    assert_eq!(result.diagnostics, []);
    (index, result.data.unwrap())
}

fn texts(index: &IndexSession, id: &str) -> Vec<String> {
    let mut found = Vec::new();
    index
        .visit_entries(
            &SessionKey {
                agent: Agent::Devin,
                id: id.into(),
            },
            &mut |entry| {
                found.push(entry.text);
                Ok(())
            },
        )
        .unwrap();
    found
}

fn visible(index: &IndexSession) -> Vec<String> {
    let mut found = Vec::new();
    index
        .visit_sessions(&mut |session| {
            found.push(session.key.id);
            Ok(())
        })
        .unwrap();
    found
}

fn two_sessions(ctx: &Context) -> Connection {
    let connection = store(ctx);
    main_session(&connection, "a");
    main_session(&connection, "b");
    connection
        .execute(
            "INSERT INTO subagent_heads VALUES('a', 'agent1', 22, 1786356250)",
            [],
        )
        .unwrap();
    assert_eq!(refreshed(ctx, &RefreshOptions::QUERY), (3, 0, 3, 0));
    connection
}

#[test]
fn devin_added_nodes_reparse_only_their_session() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = two_sessions(&ctx);
    node(
        &connection,
        "b",
        30,
        Some(14),
        &message("user", "user", "Unlinked addition"),
        &json!({}),
    );
    assert_eq!(refreshed(&ctx, &RefreshOptions::QUERY), (3, 2, 1, 0));
    connection
        .execute("UPDATE sessions SET main_chain_id=30 WHERE id='b'", [])
        .unwrap();
    let (index, stats) = queried(&ctx);
    assert_eq!((stats.reused, stats.reparsed), (2, 1));
    assert_eq!(texts(&index, "b").last().unwrap(), "Unlinked addition");
    assert_eq!(texts(&index, "a").last().unwrap(), "Canceled");
}

#[test]
fn devin_deleted_session_is_removed_by_the_next_complete_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = two_sessions(&ctx);
    connection
        .execute_batch(
            "DELETE FROM message_nodes WHERE session_id='b'; DELETE FROM sessions WHERE id='b';",
        )
        .unwrap();
    assert_eq!(refreshed(&ctx, &RefreshOptions::QUERY), (2, 2, 0, 1));
    let (index, _) = queried(&ctx);
    assert_eq!(visible(&index), ["a", "a/agent1"]);
}

#[test]
fn devin_in_place_edit_is_found_only_by_the_verification_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = two_sessions(&ctx);
    assert_eq!(refreshed(&ctx, &RefreshOptions::FULL), (3, 3, 0, 0));
    connection
        .execute(
            "UPDATE message_nodes SET chat_message=? WHERE session_id='b' AND node_id=9",
            [message("user", "user", "Rewritten prompt").to_string()],
        )
        .unwrap();
    let (index, stats) = queried(&ctx);
    assert_eq!((stats.reused, stats.reparsed), (3, 0));
    assert!(texts(&index, "b").contains(&"Second prompt".to_string()));
    drop(index);
    assert_eq!(refreshed(&ctx, &RefreshOptions::FULL), (3, 2, 1, 0));
    let (index, stats) = queried(&ctx);
    assert_eq!((stats.reused, stats.reparsed), (3, 0));
    let found = texts(&index, "b");
    assert!(found.contains(&"Rewritten prompt".to_string()));
    assert!(!found.contains(&"Second prompt".to_string()));
    assert!(texts(&index, "a").contains(&"Second prompt".to_string()));
    drop(index);
    assert_eq!(refreshed(&ctx, &RefreshOptions::FULL), (3, 3, 0, 0));
    let stored = Connection::open(ctx.index_dir.join(index::DATABASE))
        .unwrap()
        .prepare("SELECT json_extract(state,'$.state.kind'), length(json_extract(state,'$.state.sha256')) FROM sources WHERE agent='devin'")
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(stored, vec![("content".to_string(), 64); 3]);
}

#[test]
fn devin_moved_child_head_reparses_parent_and_children_only() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = two_sessions(&ctx);
    node(
        &connection,
        "a",
        23,
        Some(22),
        &message("assistant", "assistant", "child follow-up"),
        &json!({}),
    );
    connection
        .execute(
            "UPDATE subagent_heads SET chain_node_id=23, updated_at=1786356260 WHERE session_id='a'",
            [],
        )
        .unwrap();
    assert_eq!(refreshed(&ctx, &RefreshOptions::QUERY), (3, 1, 2, 0));
    let (index, _) = queried(&ctx);
    assert_eq!(
        texts(&index, "a/agent1"),
        ["Read notes.txt", "child answer needle", "child follow-up"]
    );
    assert_eq!(texts(&index, "a"), texts(&index, "b"));
}

#[test]
fn devin_head_moved_back_to_an_existing_node_reparses_only_that_child() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = two_sessions(&ctx);
    connection
        .execute(
            "UPDATE subagent_heads SET chain_node_id=14 WHERE session_id='a'",
            [],
        )
        .unwrap();
    assert_eq!(refreshed(&ctx, &RefreshOptions::QUERY), (3, 2, 1, 0));
}

#[test]
fn devin_sweep_hash_covers_every_node_column() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let connection = two_sessions(&ctx);
    for assignment in [
        "parent_node_id=coalesce(parent_node_id, 0)+1000",
        "chat_message=chat_message||' '",
        "created_at=created_at+1",
        "metadata=metadata||' '",
    ] {
        assert_eq!(
            refreshed(&ctx, &RefreshOptions::FULL),
            (3, 3, 0, 0),
            "{assignment}"
        );
        connection
            .execute(
                &format!(
                    "UPDATE message_nodes SET {assignment} WHERE session_id='b' AND node_id=9"
                ),
                [],
            )
            .unwrap();
        let stats = refresh(&ctx, &devin(), &RefreshOptions::FULL)
            .unwrap()
            .data
            .unwrap();
        assert_eq!(
            (
                stats.discovered,
                stats.reused,
                stats.reparsed,
                stats.removed
            ),
            (3, 2, 1, 0),
            "{assignment}"
        );
        connection
            .execute(
                "UPDATE message_nodes SET parent_node_id=(SELECT parent_node_id FROM message_nodes WHERE session_id='a' AND node_id=9) WHERE session_id='b' AND node_id=9",
                [],
            )
            .unwrap();
        refresh(&ctx, &devin(), &RefreshOptions::FULL).unwrap();
    }
}

#[test]
fn devin_minimal_schema_without_head_timestamps_or_unique_indexes_works() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    fs::create_dir_all(ctx.roots.devin_sessions_db.parent().unwrap()).unwrap();
    let connection = Connection::open(&ctx.roots.devin_sessions_db).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE sessions(id TEXT, working_directory TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER);
            CREATE TABLE message_nodes(session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, created_at INTEGER, metadata TEXT);
            CREATE TABLE subagent_heads(session_id TEXT, agent_id TEXT, chain_node_id INTEGER);
            INSERT INTO subagent_heads VALUES('main', 'agent1', 22);",
        )
        .unwrap();
    main_session(&connection, "main");
    assert_eq!(refreshed(&ctx, &RefreshOptions::QUERY), (2, 0, 2, 0));
    let (index, _) = queried(&ctx);
    assert_eq!(
        texts(&index, "main/agent1"),
        ["Read notes.txt", "child answer needle"]
    );
    assert_eq!(texts(&index, "main")[0], "needle devin café 🦀");
    drop(index);
    assert_eq!(refreshed(&ctx, &RefreshOptions::FULL), (2, 2, 0, 0));
    node(
        &connection,
        "main",
        23,
        Some(22),
        &message("assistant", "assistant", "child follow-up"),
        &json!({}),
    );
    connection
        .execute("UPDATE subagent_heads SET chain_node_id=23", [])
        .unwrap();
    assert_eq!(refreshed(&ctx, &RefreshOptions::QUERY), (2, 0, 2, 0));
    let (index, _) = queried(&ctx);
    assert_eq!(
        texts(&index, "main/agent1").last().unwrap(),
        "child follow-up"
    );
}
