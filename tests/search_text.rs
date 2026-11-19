use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("codex/sessions")).unwrap();
        Self(temp)
    }

    fn session(&self, id: &str, title: Option<&str>, cwd: Option<&str>, entries: &[(&str, &str)]) {
        let records = std::iter::once(json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}))
            .chain(entries.iter().map(|(role, text)| {
                json!({"type":"response_item","payload":{"type":"message","role":role,"content":text}})
            }))
            .map(|record| record.to_string() + "\n")
            .collect::<String>();
        let path = self.0.path().join("codex/sessions");
        let filename = format!("{}.jsonl", fs::read_dir(&path).unwrap().count());
        fs::write(path.join(filename), records).unwrap();
        if let Some(title) = title {
            let mut index = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.0.path().join("codex/session_index.jsonl"))
                .unwrap();
            writeln!(index, "{}", json!({"id":id,"thread_name":title})).unwrap();
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_slimetrail"));
        command
            .env_clear()
            .env("HOME", self.0.path())
            .env("CODEX_HOME", self.0.path().join("codex"))
            .env("TZ", "UTC")
            .arg("--index-dir")
            .arg(self.0.path().join("index"))
            .args(["search", "needle", "--codex", "--scope", "all"]);
        command
    }

    fn run(&self, args: &[&str], code: i32) -> Output {
        let output = self.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        output
    }
}

#[test]
fn search_text_groups_ranked_sessions_with_unicode_labels_and_exact_references() {
    let fixture = Fixture::new();
    fixture.session("a-second", None, None, &[("assistant", "needle")]);
    fixture.session(
        "z-first-long-reference-0123456789",
        Some("needle café 🐌"),
        Some("/work/日本語"),
        &[("user", "A needle in café 🐌")],
    );
    let output = fixture.run(&[], 0);
    assert_eq!(output.stderr, b"");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "Returned sessions: 2\n\n",
            "Ref: codex:z-first-long-reference-0123456789\n",
            "Title: needle café 🐌\nCwd: /work/日本語\n",
            "Occurrences: 2 | Score: 8\n",
            "  [title] needle café 🐌\n  [user/message] A needle in café 🐌\n\n",
            "Ref: codex:a-second\nOccurrences: 1 | Score: 3\n",
            "  [assistant/message] needle\n",
        )
    );
    let json: Value =
        serde_json::from_slice(&fixture.run(&["--format", "json"], 0).stdout).unwrap();
    assert_eq!(json["version"], 1);
    assert_eq!(json["incomplete"], false);
    assert_eq!(json["diagnostics"], json!([]));
    assert_eq!(json["data"]["count"], 2);
    assert_eq!(
        json["data"]["results"][0]["ref"],
        "codex:z-first-long-reference-0123456789"
    );
    assert_eq!(json["data"]["results"][1]["ref"], "codex:a-second");
    assert_eq!(json["data"]["results"][1]["title"], Value::Null);
    assert_eq!(json["data"]["results"][1]["cwd"], Value::Null);
}

#[test]
fn search_text_respects_result_and_snippet_limits_and_context() {
    let fixture = Fixture::new();
    fixture.session(
        "first",
        None,
        None,
        &[("assistant", "é🐌needle日本 needle")],
    );
    fixture.session("second", None, None, &[("assistant", "needle")]);
    let output = fixture.run(&["--max-results", "1", "--max-snippets", "0"], 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Returned sessions: 1\n\nRef: codex:first\nOccurrences: 2 | Score: 6\n"
    );
    let output = fixture.run(
        &[
            "--max-results",
            "1",
            "--max-snippets",
            "1",
            "--context",
            "1",
        ],
        0,
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "Returned sessions: 1\n\nRef: codex:first\nOccurrences: 2 | Score: 6\n",
            "  [assistant/message] …🐌needle日…\n",
        )
    );
}

#[test]
fn search_text_distinguishes_complete_and_incomplete_empty_results() {
    let fixture = Fixture::new();
    let complete = fixture.run(&[], 0);
    assert_eq!(complete.stdout, b"No matches.\n");
    assert_eq!(complete.stderr, b"");
    let partial = fixture.run(&["--claude"], 1);
    assert_eq!(
        partial.stdout,
        b"No matches in available sources (search incomplete).\n"
    );
    let json_output = fixture.run(&["--claude", "--format", "json"], 1);
    assert_eq!(partial.stderr, json_output.stderr);
    let json: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(json["incomplete"], true);
    assert_eq!(json["data"]["count"], 0);
    assert_eq!(json["diagnostics"][0]["agent"], "claude");
}

#[test]
fn search_text_marks_nonempty_partial_coverage_and_preserves_diagnostics() {
    let fixture = Fixture::new();
    fixture.session("sample", None, None, &[("assistant", "needle")]);
    let partial = fixture.run(&["--claude"], 1);
    assert_eq!(
        String::from_utf8(partial.stdout).unwrap(),
        concat!(
            "Returned sessions: 1 (search incomplete)\n\nRef: codex:sample\n",
            "Occurrences: 1 | Score: 3\n  [assistant/message] needle\n",
        )
    );
    let json_output = fixture.run(&["--claude", "--format", "json"], 1);
    assert_eq!(partial.stderr, json_output.stderr);
    let json: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(json["incomplete"], true);
    assert_eq!(json["data"]["count"], 1);
    assert_eq!(json["diagnostics"][0]["agent"], "claude");
}

#[test]
fn search_text_escapes_terminal_controls_while_json_preserves_source_values() {
    let fixture = Fixture::new();
    let id = "unsafe\u{1b}[2J\r\n\t\u{7}\u{8}\u{7f}\u{9b}\u{202e}";
    let title = "café\u{1b}]0;title\u{7} needle";
    let cwd = "/work/\r\n\u{2066}日本語";
    let text = "needle\u{1b}[31m\u{0}\u{9b}0m\u{202e}🐌";
    fixture.session(id, Some(title), Some(cwd), &[("assistant", text)]);
    let output = fixture.run(&[], 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "Returned sessions: 1\n\n",
            "Ref: codex:unsafe\\u{1b}[2J\\r\\n\\t\\u{7}\\u{8}\\u{7f}\\u{9b}\\u{202e}\n",
            "Title: café\\u{1b}]0;title\\u{7} needle\n",
            "Cwd: /work/\\r\\n\\u{2066}日本語\n",
            "Occurrences: 2 | Score: 8\n",
            "  [title] café\\u{1b}]0;title\\u{7} needle\n",
            "  [assistant/message] needle\\u{1b}[31m\\u{0}\\u{9b}0m\\u{202e}🐌\n",
        )
    );
    let json: Value =
        serde_json::from_slice(&fixture.run(&["--format", "json"], 0).stdout).unwrap();
    let hit = &json["data"]["results"][0];
    assert_eq!(hit["ref"], format!("codex:{id}"));
    assert_eq!(hit["title"], title);
    assert_eq!(hit["cwd"], cwd);
    assert_eq!(
        hit["snippets"],
        json!([
            format!("[title] {title}"),
            format!("[assistant/message] {text}")
        ])
    );
}

#[test]
fn search_text_handles_a_closed_stdout_pipe_quietly() {
    let fixture = Fixture::new();
    fixture.session("sample", None, None, &[("assistant", "needle")]);
    let mut child = fixture
        .command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
}
