use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};
use tempfile::TempDir;

const FILE_AGENTS: [&str; 5] = ["--claude", "--codex", "--cursor", "--droid", "--pi"];

struct Fixture {
    temp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        for store in [
            ".claude/projects",
            "codex/sessions",
            ".factory/sessions",
            ".pi/agent/sessions",
            ".cursor/projects",
            ".cursor/chats",
        ] {
            fs::create_dir_all(temp.path().join(store)).unwrap();
        }
        Self { temp }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn archive(&self) -> PathBuf {
        self.root().join("data/ringleader/archive/project")
    }
    fn json(&self, args: &[&str], code: i32) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_slimetrail"))
            .env_clear()
            .env("HOME", self.root())
            .env("CODEX_HOME", self.root().join("codex"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("TZ", "UTC")
            .arg("--index-dir")
            .arg(self.root().join("index"))
            .args(["--format", "json", "--scope", "all"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn write(path: &Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
}

fn codex_rollout(text: &str) -> Vec<Value> {
    vec![
        json!({"type":"session_meta","payload":{"id":"codex-archived","cwd":"/fixture/project"}}),
        json!({"type":"response_item","timestamp":"2026-11-01T10:00:00Z","payload":{"type":"message","role":"user","content":text}}),
    ]
}

fn cursor_transcript(path: &Path, text: &str) {
    write(
        path,
        &[
            json!({"role":"user","message":{"content":[{"type":"text","text":format!("<user_query>\n{text}\n</user_query>")}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"looking"}]}}),
        ],
    );
}

fn cursor_store(chat: &Path, value: &Value) {
    let connection = rusqlite::Connection::open(chat.join("store.db")).unwrap();
    connection
        .execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    connection
        .execute("INSERT INTO meta VALUES('0', hex(?1))", [value.to_string()])
        .unwrap();
}

fn mixed_archive(fixture: &Fixture) {
    let archive = fixture.archive();
    cursor_transcript(&archive.join("cursor-archived.jsonl"), "needle-cursor");
    let chat = archive.join("cursor-archived");
    fs::create_dir_all(&chat).unwrap();
    fs::write(
        chat.join("meta.json"),
        json!({"title":"Archived cursor title","cwd":"/fixture/cursor-project","createdAtMs":1_762_000_000_000_i64}).to_string(),
    )
    .unwrap();
    cursor_store(&chat, &json!({"name":"Store title"}));
    write(
        &archive.join("claude-archived.jsonl"),
        &[
            json!({"type":"user","sessionId":"claude-archived","cwd":"/fixture/project","timestamp":"2026-11-01T10:00:00Z","message":{"content":"needle-claude"}}),
        ],
    );
    write(
        &archive.join("claude-archived/subagents/agent-helper.jsonl"),
        &[
            json!({"type":"user","sessionId":"claude-archived","agentId":"helper","message":"needle-claude-child"}),
        ],
    );
    fs::create_dir_all(archive.join("claude-archived/tool-results")).unwrap();
    fs::write(
        archive.join("claude-archived/tool-results/result.txt"),
        "tool payload",
    )
    .unwrap();
    write(
        &archive.join("rollout-2026-11-01T10-00-00-codex-archived.jsonl"),
        &codex_rollout("needle-codex"),
    );
    write(
        &archive.join("2026-11-01T10-00-00_pi-archived.jsonl"),
        &[
            json!({"type":"session","id":"pi-archived","cwd":"/fixture/project"}),
            json!({"type":"message","timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":"needle-pi"}}),
        ],
    );
    write(
        &archive.join("droid-archived.jsonl"),
        &[
            json!({"type":"session_start","id":"droid-archived","cwd":"/fixture/project"}),
            json!({"type":"message","timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":"needle-droid"}}),
        ],
    );
    fs::write(
        archive.join("droid-archived.settings.json"),
        r#"{"tags":[{"name":"exec"}]}"#,
    )
    .unwrap();
    fs::write(archive.join("droid-archived.settings.json.bak"), "{}").unwrap();
    fs::write(archive.join("codex-archived.md"), "# Harvest summary\n").unwrap();
    write(
        &archive.join("unrecognized.jsonl"),
        &[json!({"type":"unrecognized","text":"Unknown source"})],
    );
}

fn refs(value: &Value) -> Vec<(String, bool, Option<String>)> {
    let mut refs: Vec<_> = value["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["ref"].as_str().unwrap().to_owned(),
                s["archived"].as_bool().unwrap(),
                s["parent_ref"].as_str().map(str::to_owned),
            )
        })
        .collect();
    refs.sort();
    refs
}

fn args<'a>(command: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    std::iter::once(command)
        .chain(rest.iter().copied())
        .collect()
}

#[test]
fn mixed_ringleader_archive_attributes_each_transcript_to_its_agent() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    let listed = fixture.json(&args("sessions", &FILE_AGENTS), 1);
    assert_eq!(
        refs(&listed),
        [
            ("claude:claude-archived".to_owned(), true, None),
            (
                "claude:helper".to_owned(),
                true,
                Some("claude:claude-archived".to_owned())
            ),
            ("codex:codex-archived".to_owned(), true, None),
            ("cursor:cursor-archived".to_owned(), true, None),
            ("droid:droid-archived".to_owned(), true, None),
            ("pi:pi-archived".to_owned(), true, None),
        ]
    );
    let diagnostics = listed["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["code"], "archive_format_unknown");
    assert!(
        diagnostics[0]["source"]
            .as_str()
            .unwrap()
            .ends_with("unrecognized.jsonl")
    );
    for (agent, id) in [
        ("codex", "codex-archived"),
        ("cursor", "cursor-archived"),
        ("droid", "droid-archived"),
        ("pi", "pi-archived"),
    ] {
        let found = fixture.json(
            &["search", &format!("needle-{agent}"), &format!("--{agent}")],
            0,
        );
        assert_eq!(found["data"]["count"], 1);
        assert_eq!(found["data"]["results"][0]["ref"], format!("{agent}:{id}"));
        let read = fixture.json(&["read", &format!("{agent}:{id}")], 0);
        assert!(
            read["data"]["body"]
                .as_str()
                .unwrap()
                .contains(&format!("needle-{agent}"))
        );
        assert_eq!(read["data"]["archived"], true);
    }
    let droid = fixture.json(&["entries", "droid:droid-archived"], 0);
    assert_eq!(droid["data"]["entries"][0]["automated"], true);
    let cursor = fixture.json(&["sessions", "--cursor"], 0);
    let session = &cursor["data"]["sessions"][0];
    assert_eq!(session["title"], "Archived cursor title");
    assert_eq!(session["cwd"], "/fixture/cursor-project");
    assert_eq!(session["archived"], true);
}

#[test]
fn agent_selectors_decide_which_archived_sessions_are_visible() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    fs::remove_file(fixture.archive().join("unrecognized.jsonl")).unwrap();
    let claude = fixture.json(&["sessions", "--claude"], 0);
    assert_eq!(
        refs(&claude)
            .into_iter()
            .map(|(r, _, _)| r)
            .collect::<Vec<_>>(),
        ["claude:claude-archived", "claude:helper"]
    );
    let codex = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(
        refs(&codex),
        [("codex:codex-archived".to_owned(), true, None)]
    );
    assert_eq!(codex["diagnostics"], json!([]));
}

#[test]
fn unknown_archive_file_is_not_diagnosed_for_other_selected_agents() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    let codex = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(codex["incomplete"], false);
    assert_eq!(refs(&codex).len(), 1);
}

#[test]
fn agent_like_archive_files_no_reader_claims_are_diagnosed() {
    let fixture = Fixture::new();
    let archive = fixture.archive();
    write(
        &archive.join("headerless.jsonl"),
        &[json!({"type":"message","message":{"role":"user","content":"needle-headerless"}})],
    );
    write(
        &archive.join("early-codex.jsonl"),
        &[
            json!({"id":"early","timestamp":"2025-01-01T00:00:00Z"}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"needle-early"}]}),
            json!({"type":"user","message":"Claude-looking record after a foreign one"}),
        ],
    );
    let listed = fixture.json(&args("sessions", &FILE_AGENTS), 1);
    assert_eq!(listed["data"]["count"], 0);
    let mut diagnostics: Vec<_> = listed["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["code"].as_str().unwrap().to_owned(),
                d["agent"].as_str().unwrap().to_owned(),
                Path::new(d["source"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect();
    diagnostics.sort();
    let expected = |name: &str| {
        (
            "archive_format_unknown".to_owned(),
            "claude".to_owned(),
            name.to_owned(),
        )
    };
    assert_eq!(
        diagnostics,
        [expected("early-codex.jsonl"), expected("headerless.jsonl")]
    );
}

#[test]
fn codex_rollout_in_native_and_ringleader_archives_appears_once() {
    let fixture = Fixture::new();
    let native = fixture
        .root()
        .join("codex/archived_sessions/rollout-codex-archived.jsonl");
    write(&native, &codex_rollout("needle-native"));
    fs::File::open(&native)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();
    let shared = fixture.archive().join("rollout-codex-archived.jsonl");
    write(&shared, &codex_rollout("needle-native"));
    let listed = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(
        refs(&listed),
        [("codex:codex-archived".to_owned(), true, None)]
    );
    assert_eq!(
        listed["data"]["sessions"][0]["source"],
        shared.to_string_lossy().as_ref()
    );
    let found = fixture.json(&["search", "needle-native", "--codex"], 0);
    assert_eq!(found["data"]["count"], 1);
}

#[test]
fn warm_index_reuses_archived_sessions_and_droid_sidecar_invalidates() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    fs::remove_file(fixture.archive().join("unrecognized.jsonl")).unwrap();
    let first = fixture.json(&args("index", &FILE_AGENTS), 0);
    assert_eq!(first["data"]["reparsed"], 6);
    let warm = fixture.json(&args("index", &FILE_AGENTS), 0);
    assert_eq!(warm["data"]["reused"], 6);
    assert_eq!(warm["data"]["reparsed"], 0);
    fs::write(
        fixture.archive().join("droid-archived.settings.json"),
        r#"{"tags":[]}"#,
    )
    .unwrap();
    let changed = fixture.json(&args("index", &FILE_AGENTS), 0);
    assert_eq!(changed["data"]["reparsed"], 1);
    assert_eq!(changed["data"]["reused"], 5);
    let droid = fixture.json(&["entries", "droid:droid-archived"], 0);
    assert_eq!(droid["data"]["entries"][0]["automated"], false);
}

#[test]
fn archived_cursor_chat_metadata_and_store_invalidate_their_session() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    let chat = fixture.archive().join("cursor-archived");
    assert_eq!(
        fixture.json(&["index", "--cursor"], 0)["data"]["reparsed"],
        1
    );
    assert_eq!(fixture.json(&["index", "--cursor"], 0)["data"]["reused"], 1);
    fs::write(
        chat.join("meta.json"),
        json!({"title":"Renamed","cwd":"/fixture/cursor-project"}).to_string(),
    )
    .unwrap();
    assert_eq!(
        fixture.json(&["index", "--cursor"], 0)["data"]["reparsed"],
        1
    );
    let listed = fixture.json(&["sessions", "--cursor"], 0);
    assert_eq!(listed["data"]["sessions"][0]["title"], "Renamed");
    rusqlite::Connection::open(chat.join("store.db"))
        .unwrap()
        .execute("UPDATE meta SET value='7b7d'", [])
        .unwrap();
    assert_eq!(
        fixture.json(&["index", "--cursor"], 0)["data"]["reparsed"],
        1
    );
}

#[test]
fn archived_cursor_without_chat_directory_reports_missing_metadata() {
    let fixture = Fixture::new();
    let archive = fixture.archive();
    cursor_transcript(&archive.join("no-chat.jsonl"), "needle-no-chat");
    cursor_transcript(&archive.join("claude-like.jsonl"), "needle-claude-like");
    fs::create_dir_all(archive.join("claude-like/tool-results")).unwrap();
    fs::write(
        archive.join("claude-like/tool-results/result.txt"),
        "payload",
    )
    .unwrap();
    let listed = fixture.json(&["sessions", "--cursor"], 1);
    assert_eq!(
        refs(&listed),
        [
            ("cursor:claude-like".to_owned(), true, None),
            ("cursor:no-chat".to_owned(), true, None),
        ]
    );
    assert!(
        listed["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["cwd"].is_null())
    );
    let mut diagnostics: Vec<_> = listed["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["code"].as_str().unwrap().to_owned(),
                Path::new(d["source"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect();
    diagnostics.sort();
    let expected = |name: &str| ("metadata_unavailable".to_owned(), name.to_owned());
    assert_eq!(
        diagnostics,
        [
            expected("claude-like.jsonl"),
            expected("claude-like.jsonl"),
            expected("no-chat.jsonl"),
            expected("no-chat.jsonl"),
        ]
    );
    let found = fixture.json(&["search", "needle-no-chat", "--cursor"], 1);
    assert_eq!(found["data"]["results"][0]["ref"], "cursor:no-chat");
}

#[test]
fn cursor_chat_directory_and_claude_companion_stay_with_their_own_transcripts() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    fs::remove_file(fixture.archive().join("unrecognized.jsonl")).unwrap();
    let claude = fixture.json(&["sessions", "--claude"], 0);
    let session = claude["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["ref"] == "claude:claude-archived")
        .unwrap();
    assert_eq!(session["cwd"], "/fixture/project");
    assert_eq!(refs(&claude).len(), 2);
    let cursor = fixture.json(&["sessions", "--cursor"], 0);
    assert_eq!(
        refs(&cursor),
        [("cursor:cursor-archived".to_owned(), true, None)]
    );
}

#[test]
fn live_cursor_session_wins_over_its_archived_copy() {
    let fixture = Fixture::new();
    mixed_archive(&fixture);
    let live = fixture
        .root()
        .join(".cursor/projects/slug/agent-transcripts/cursor-archived/cursor-archived.jsonl");
    cursor_transcript(&live, "needle-live");
    fs::File::open(&live)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();
    let chat = fixture.root().join(".cursor/chats/hash/cursor-archived");
    fs::create_dir_all(&chat).unwrap();
    fs::write(
        chat.join("meta.json"),
        json!({"title":"Live title","cwd":"/fixture/live"}).to_string(),
    )
    .unwrap();
    let listed = fixture.json(&["sessions", "--cursor"], 0);
    assert_eq!(
        refs(&listed),
        [("cursor:cursor-archived".to_owned(), false, None)]
    );
    assert_eq!(listed["data"]["sessions"][0]["title"], "Live title");
}

#[test]
fn missing_ringleader_archive_is_a_complete_result() {
    let fixture = Fixture::new();
    let listed = fixture.json(&args("sessions", &FILE_AGENTS), 0);
    assert_eq!(listed["incomplete"], false);
    assert_eq!(listed["data"]["count"], 0);
}
