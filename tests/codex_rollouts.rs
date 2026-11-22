use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
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
