use serde_json::{Value, json};
use slimetrail::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn context(base: &Path) -> Context {
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
        cwd: "/fixture/project".into(),
        now_ms: ts("2026-11-16T12:00:00Z"),
        time_zone: "UTC".into(),
        coverage: slimetrail::Coverage::Compact,
        build: slimetrail::BuildPolicy::Inline,
        progress: false,
    }
}
pub(crate) fn ts(text: &str) -> i64 {
    text.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}
pub(crate) fn filter(agent: Agent) -> Filter {
    Filter {
        agents: BTreeSet::from([agent]),
        cwd: "/fixture/project".into(),
        scope: Scope::All,
        app: None,
        days: None,
        kinds: BTreeSet::new(),
    }
}
pub(crate) fn options(query: &str) -> SearchOptions {
    SearchOptions {
        filter: filter(Agent::Claude),
        query: query.into(),
        regex: false,
        case_sensitive: false,
        sort: Sort::Score,
        max_results: 50,
        max_snippets: 3,
        context_chars: 8,
    }
}
pub(crate) fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
pub(crate) fn records(path: &Path, values: &[Value]) {
    write(
        path,
        &(values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"),
    );
}
pub(crate) fn claude(
    ctx: &Context,
    id: &str,
    title: Option<&str>,
    cwd: Option<&str>,
    events: &[Value],
) -> PathBuf {
    let path = ctx
        .roots
        .claude_projects
        .join(format!("project/{id}.jsonl"));
    let mut values = vec![json!({"type":"system","sessionId":id,"cwd":cwd})];
    if let Some(title) = title {
        values.push(json!({"type":"custom-title","customTitle":title}));
    }
    values.extend_from_slice(events);
    records(&path, &values);
    path
}
pub(crate) fn event(role: &str, text: &str, time: Option<&str>) -> Value {
    json!({"type":role,"timestamp":time,"message":{"content":text}})
}
pub(crate) fn six(ctx: &Context) {
    for (agent, root) in [
        (Agent::Claude, &ctx.roots.claude_projects),
        (Agent::Codex, &ctx.roots.codex_sessions),
        (Agent::Droid, &ctx.roots.droid_sessions),
        (Agent::Pi, &ctx.roots.pi_sessions),
    ] {
        write(
            &root.join(format!("project/{agent}-canonical.jsonl")),
            &fs::read_to_string(format!("tests/fixtures/jsonl/{agent}.jsonl")).unwrap(),
        );
    }
    write(
        &ctx.roots
            .cursor_projects
            .join("slug/agent-transcripts/main/main.jsonl"),
        include_str!("../fixtures/jsonl/cursor.jsonl"),
    );
    write(
        &ctx.roots.cursor_chats.join("hash/main/meta.json"),
        r#"{"cwd":"/fixture/project","title":"Cursor title","createdAtMs":1786356360000,"updatedAtMs":1786356480000}"#,
    );
    fs::create_dir_all(ctx.roots.opencode_db.parent().unwrap()).unwrap();
    let connection = rusqlite::Connection::open(&ctx.roots.opencode_db).unwrap();
    connection.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER, parent_id TEXT, time_archived INTEGER);
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
            json!({"type":"text","text":"needle opencode café 🦀"}),
        ),
        (
            "p2",
            "m2",
            1_786_356_120_000_i64,
            json!({"type":"reasoning","text":"Reason carefully"}),
        ),
        (
            "p3",
            "m2",
            1_786_356_180_000_i64,
            json!({"type":"tool","tool":"shell","state":{"input":{"command":"echo needle"},"output":"needle"}}),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO part VALUES(?,?,?,?)",
                rusqlite::params![id, message, time, part.to_string()],
            )
            .unwrap();
    }
}
