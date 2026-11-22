use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, SystemTime},
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("codex/sessions")).unwrap();
        Self { temp }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn sessions(&self) -> std::path::PathBuf {
        self.root().join("codex/sessions/2026/11/01")
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

fn rollout(id: &str, source: &Value, text: &str) -> String {
    [
        json!({"type":"session_meta","payload":{"id":id,"cwd":"/fixture/project","source":source}}),
        json!({"type":"response_item","timestamp":"2026-11-01T10:00:00Z","payload":{"type":"message","role":"user","content":text}}),
    ]
    .iter()
    .map(|r| r.to_string() + "\n")
    .collect()
}

fn refs(value: &Value) -> Vec<String> {
    let mut refs: Vec<_> = value["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["ref"].as_str().unwrap().to_owned())
        .collect();
    refs.sort();
    refs
}

#[test]
fn vscode_and_cli_sources_are_interactive_while_exec_and_subagents_stay_automated() {
    let fixture = Fixture::new();
    let sources = [
        ("cli", json!("cli"), false),
        ("vscode", json!("vscode"), false),
        ("exec", json!("exec"), true),
        (
            "subagent",
            json!({"subagent":{"thread_spawn":{"parent_thread_id":"cli"}}}),
            true,
        ),
    ];
    fs::create_dir_all(fixture.sessions()).unwrap();
    for (id, source, _) in &sources {
        fs::write(
            fixture.sessions().join(format!("rollout-{id}.jsonl")),
            rollout(id, source, &format!("prompt from {id}")),
        )
        .unwrap();
    }
    let listed = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(
        refs(&listed),
        ["codex:cli", "codex:exec", "codex:subagent", "codex:vscode"]
    );
    for (id, _, automated) in sources {
        let entries = fixture.json(&["entries", &format!("codex:{id}")], 0);
        assert_eq!(
            entries["data"]["entries"][0]["automated"], automated,
            "{id}"
        );
    }
}

fn compress(text: &str) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(text.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
}

fn needle_rollout() -> String {
    rollout("rollout", &json!("cli"), "needle-compressed prompt")
}

fn set_mtime(path: &Path, mtime: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

fn session_view(fixture: &Fixture) -> (Value, Value, Value, Value) {
    let mut listed = fixture.json(&["sessions", "--codex"], 0);
    let session = listed["data"]["sessions"][0].take();
    let mut entries = fixture.json(&["entries", "codex:rollout"], 0)["data"].take();
    entries["session"] = without_source(entries["session"].take());
    let found: Value =
        fixture.json(&["search", "needle-compressed", "--codex"], 0)["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(without_source)
            .collect();
    let read = fixture.json(&["read", "codex:rollout"], 0)["data"]["body"].take();
    (session, entries, found, read)
}

fn without_source(mut value: Value) -> Value {
    value.as_object_mut().unwrap().remove("source");
    value
}

#[test]
fn compressed_only_rollout_reads_like_the_plain_rollout() {
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_000_000);
    let plain = Fixture::new();
    let plain_path = plain.sessions().join("rollout-a.jsonl");
    fs::create_dir_all(plain.sessions()).unwrap();
    fs::write(&plain_path, needle_rollout()).unwrap();
    set_mtime(&plain_path, mtime);
    let packed = Fixture::new();
    let packed_path = packed.sessions().join("rollout-a.jsonl.zst");
    fs::create_dir_all(packed.sessions()).unwrap();
    fs::write(&packed_path, compress(&needle_rollout())).unwrap();
    set_mtime(&packed_path, mtime);
    let (plain_session, plain_entries, plain_found, plain_read) = session_view(&plain);
    let (session, entries, found, read) = session_view(&packed);
    assert_eq!(session["ref"], "codex:rollout");
    assert_eq!(session["source"], packed_path.to_string_lossy().as_ref());
    assert_eq!(without_source(session), without_source(plain_session));
    assert_eq!(entries, plain_entries);
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found, plain_found);
    assert!(read.as_str().unwrap().contains("needle-compressed prompt"));
    assert_eq!(read, plain_read);
}

#[test]
fn plain_rollout_beside_its_compressed_copy_is_the_session() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.sessions()).unwrap();
    let plain = fixture.sessions().join("rollout-a.jsonl");
    fs::write(&plain, needle_rollout()).unwrap();
    fs::write(
        plain.with_extension("jsonl.zst"),
        compress(&rollout("rollout", &json!("cli"), "stale compressed copy")),
    )
    .unwrap();
    let listed = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(listed["data"]["count"], 1);
    assert_eq!(
        listed["data"]["sessions"][0]["source"],
        plain.to_string_lossy().as_ref()
    );
    let found = fixture.json(&["search", "stale", "--codex"], 0);
    assert_eq!(found["data"]["count"], 0);
}

#[test]
fn compressing_and_decompressing_an_indexed_rollout_rereads_it_once_under_its_id() {
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_000_000);
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.sessions()).unwrap();
    let plain = fixture.sessions().join("rollout-a.jsonl");
    let packed = plain.with_extension("jsonl.zst");
    fs::write(&plain, needle_rollout()).unwrap();
    set_mtime(&plain, mtime);
    let first = fixture.json(&["index", "--codex"], 0);
    assert_eq!(first["data"]["reparsed"], 1);
    let (original, entries, _, _) = session_view(&fixture);
    for (from, to, contents) in [
        (&plain, &packed, compress(&needle_rollout())),
        (&packed, &plain, needle_rollout().into_bytes()),
    ] {
        fs::write(to, contents).unwrap();
        set_mtime(to, mtime);
        fs::remove_file(from).unwrap();
        let swapped = fixture.json(&["index", "--codex"], 0);
        assert_eq!(swapped["data"]["reparsed"], 1);
        assert_eq!(swapped["data"]["removed"], 1);
        let again = fixture.json(&["index", "--codex"], 0);
        assert_eq!(
            (
                again["data"]["reparsed"].clone(),
                again["data"]["reused"].clone()
            ),
            (json!(0), json!(1))
        );
        let listed = fixture.json(&["sessions", "--codex"], 0);
        assert_eq!(listed["data"]["count"], 1);
        let (session, now, _, _) = session_view(&fixture);
        assert_eq!(session["source"], to.to_string_lossy().as_ref());
        assert_eq!(without_source(session), without_source(original.clone()));
        assert_eq!(now, entries);
    }
}

#[test]
fn every_frame_of_a_multi_frame_rollout_is_read() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.sessions()).unwrap();
    let text = needle_rollout();
    let (head, tail) = text.split_at(text.find('\n').unwrap() + 1);
    let mut bytes = compress(head);
    bytes.extend([0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3]);
    bytes.extend(compress(tail));
    fs::write(fixture.sessions().join("rollout-a.jsonl.zst"), bytes).unwrap();
    let entries = fixture.json(&["entries", "codex:rollout"], 0);
    assert_eq!(
        entries["data"]["entries"][0]["text"],
        "needle-compressed prompt"
    );
}

#[test]
fn corrupt_or_truncated_compressed_rollouts_are_diagnosed_and_partial() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.sessions()).unwrap();
    fs::write(
        fixture.sessions().join("rollout-good.jsonl"),
        needle_rollout(),
    )
    .unwrap();
    fs::write(
        fixture.sessions().join("rollout-garbage.jsonl.zst"),
        b"not zstd at all",
    )
    .unwrap();
    let whole = compress(&rollout("cut", &json!("cli"), &"long prompt ".repeat(2000)));
    fs::write(
        fixture.sessions().join("rollout-cut.jsonl.zst"),
        &whole[..whole.len() / 2],
    )
    .unwrap();
    let listed = fixture.json(&["sessions", "--codex"], 1);
    assert_eq!(listed["incomplete"], true);
    assert_eq!(refs(&listed), ["codex:rollout"]);
    let mut diagnosed: Vec<_> = listed["diagnostics"]
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
    diagnosed.sort();
    assert_eq!(
        diagnosed,
        [
            (
                "source_unavailable".to_owned(),
                "rollout-cut.jsonl.zst".to_owned()
            ),
            (
                "source_unavailable".to_owned(),
                "rollout-garbage.jsonl.zst".to_owned()
            ),
        ]
    );
}

#[test]
fn compressed_rollouts_in_both_archives_are_archived_sessions() {
    let fixture = Fixture::new();
    let native = fixture.root().join("codex/archived_sessions");
    let ringleader = fixture.root().join("data/ringleader/archive/project");
    fs::create_dir_all(&native).unwrap();
    fs::create_dir_all(&ringleader).unwrap();
    fs::write(
        native.join("rollout-native.jsonl.zst"),
        compress(&rollout("native", &json!("cli"), "needle-native")),
    )
    .unwrap();
    fs::write(
        ringleader.join("rollout-ringleader.jsonl.zst"),
        compress(&rollout("ringleader", &json!("cli"), "needle-ringleader")),
    )
    .unwrap();
    let listed = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(refs(&listed), ["codex:native", "codex:ringleader"]);
    for session in listed["data"]["sessions"].as_array().unwrap() {
        assert_eq!(session["archived"], true);
    }
    let found = fixture.json(&["search", "needle-ringleader", "--codex"], 0);
    assert_eq!(found["data"]["results"][0]["ref"], "codex:ringleader");
}

#[test]
fn compressed_ringleader_archive_files_are_routed_by_their_decoded_format() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root().join(".claude/projects")).unwrap();
    let ringleader = fixture.root().join("data/ringleader/archive/project");
    fs::create_dir_all(&ringleader).unwrap();
    let claude =
        json!({"type":"user","sessionId":"claude-packed","message":"hello"}).to_string() + "\n";
    fs::write(
        ringleader.join("claude-packed.jsonl.zst"),
        compress(&claude),
    )
    .unwrap();
    fs::write(ringleader.join("broken.jsonl.zst"), b"garbage").unwrap();
    let codex = fixture.json(&["sessions", "--codex"], 1);
    assert_eq!(codex["data"]["count"], 0);
    let mut diagnosed: Vec<_> = codex["diagnostics"]
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
    diagnosed.sort();
    assert_eq!(
        diagnosed,
        [
            (
                "archive_format_unknown".to_owned(),
                "codex".to_owned(),
                "claude-packed.jsonl.zst".to_owned()
            ),
            (
                "source_unavailable".to_owned(),
                "codex".to_owned(),
                "broken.jsonl.zst".to_owned()
            ),
        ]
    );
    let claude = fixture.json(&["sessions", "--claude"], 0);
    assert_eq!(claude["data"]["count"], 0);
}

#[test]
fn compressed_rollout_without_a_header_id_keeps_the_plain_file_id() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.sessions()).unwrap();
    let headerless =
        json!({"type":"session_meta","payload":{"cwd":"/fixture/project"}}).to_string() + "\n";
    fs::write(
        fixture.sessions().join("rollout-unnamed.jsonl.zst"),
        compress(&headerless),
    )
    .unwrap();
    let listed = fixture.json(&["sessions", "--codex"], 0);
    assert_eq!(refs(&listed), ["codex:rollout-unnamed"]);
}
