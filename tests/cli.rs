use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
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
    fn write(&self, text: &str) {
        let records = [
            json!({"type":"session_meta","payload":{"id":"sample","cwd":self.root().join("work/app"),"git":{"branch":"main"}}}),
            json!({"type":"response_item","timestamp":"2026-10-25T00:30:00Z","payload":{"type":"message","role":"user","content":text}}),
            json!({"type":"response_item","timestamp":"2026-10-25T00:40:00Z","payload":{"type":"message","role":"assistant","content":"Second reply"}}),
        ];
        fs::write(
            self.root().join("codex/sessions/sample.jsonl"),
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_slimetrail"));
        command
            .env_clear()
            .env("HOME", self.root())
            .env("CODEX_HOME", self.root().join("codex"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("TZ", "UTC")
            .arg("--index-dir")
            .arg(self.root().join("index"));
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn json(&self, args: &[&str], code: i32) -> Value {
        let output = self
            .command()
            .args(["--format", "json"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["version"], 1);
        value
    }
}

#[test]
fn search_complete_empty_partial_and_fatal_envelopes() {
    let fixture = Fixture::new();
    fixture.write("Unicode café 🐌 and a.b");
    let complete = fixture.json(&["search", "A.B", "--codex", "--scope", "all"], 0);
    assert_eq!(complete["incomplete"], false);
    assert_eq!(complete["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(complete["diagnostics"][0]["code"], "search_coverage");
    assert_eq!(complete["data"]["count"], 1);
    assert_eq!(complete["data"]["results"][0]["ref"], "codex:sample");
    assert_eq!(
        complete["data"]["results"][0]["first_ts"],
        "2026-10-25T00:30:00Z"
    );
    let empty = fixture.json(&["search", "missing", "--codex", "--scope", "all"], 0);
    assert_eq!(empty["data"]["results"], json!([]));
    let partial = fixture.json(
        &["search", "café", "--codex", "--claude", "--scope", "all"],
        1,
    );
    assert_eq!(partial["incomplete"], true);
    assert_eq!(partial["data"]["count"], 1);
    assert_ne!(
        partial["diagnostics"].as_array().unwrap().as_slice(),
        &[] as &[Value]
    );
    let unavailable = fixture.json(&["search", "x", "--claude", "--scope", "all"], 1);
    assert_eq!(unavailable["data"]["results"], json!([]));
    let fatal = fixture.json(&["search", "(", "--regex", "--codex"], 2);
    assert_eq!(fatal["data"], Value::Null);
    assert_eq!(fatal["diagnostics"][0]["code"], "fatal");
    let absent = fixture.json(&["read", "codex:absent"], 2);
    assert_eq!(absent["data"], Value::Null);
    let unavailable_read = fixture.json(&["read", "claude:absent"], 1);
    assert_eq!(unavailable_read["data"], Value::Null);
    let unavailable_entries = fixture.json(&["entries", "claude:absent"], 1);
    assert_eq!(unavailable_entries["data"]["entries"], json!([]));
    assert_eq!(unavailable_entries["data"]["session"], Value::Null);
}

#[test]
fn read_pages_concatenate_unicode_and_entries_finalize_metadata() {
    let fixture = Fixture::new();
    fixture.write("café 🐌 needle");
    let full = fixture.json(&["read", "codex:sample"], 0);
    let body = full["data"]["body"].as_str().unwrap();
    let mut actual = String::new();
    let mut offset = 0;
    loop {
        let page = fixture.json(
            &[
                "read",
                "codex:sample",
                "--max-chars",
                "7",
                "--offset",
                &offset.to_string(),
            ],
            0,
        );
        let data = &page["data"];
        let text = data["body"].as_str().unwrap();
        assert_eq!(data["returned_chars"], text.chars().count());
        actual.push_str(text);
        match data["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(actual, body);
    let entries = fixture.json(&["entries", "codex:sample"], 0);
    assert_eq!(entries["data"]["entries"][0]["text"], "café 🐌 needle");
    assert_eq!(
        entries["data"]["entries"][0]["timestamp_ms"],
        1_792_888_200_000_i64
    );
    assert_eq!(entries["data"]["session"]["ref"], "codex:sample");
    assert_eq!(entries["incomplete"], false);
    let raw = fixture.run(&["entries", "codex:sample", "--format", "json"]);
    let raw = String::from_utf8(raw.stdout).unwrap();
    assert!(raw.find("\"entries\"").unwrap() < raw.find("\"session\"").unwrap());
}

#[test]
fn text_read_reports_scalar_continuation_without_changing_transcript() {
    let fixture = Fixture::new();
    fixture.write("café 🐌 needle");
    let full = fixture.json(&["read", "codex:sample"], 0);
    let body = full["data"]["body"].as_str().unwrap();
    let split = body.chars().position(|ch| ch == '🐌').unwrap() + 1;
    let first = fixture.run(&["read", "codex:sample", "--max-chars", &split.to_string()]);
    assert!(first.status.success());
    assert_eq!(
        String::from_utf8(first.stdout).unwrap(),
        body.chars().take(split).collect::<String>()
    );
    assert_eq!(
        String::from_utf8(first.stderr).unwrap(),
        format!("Transcript truncated; continue with --offset {split}\n")
    );
    let last = fixture.run(&["read", "codex:sample", "--offset", &split.to_string()]);
    assert!(last.status.success());
    assert_eq!(
        String::from_utf8(last.stdout).unwrap(),
        body.chars().skip(split).collect::<String>()
    );
    assert_eq!(last.stderr, b"");
}

#[test]
fn incremental_index_reports_reuse_and_change() {
    let fixture = Fixture::new();
    fixture.write("needle");
    let first = fixture.json(&["index", "--codex"], 0);
    assert_eq!(first["data"]["reparsed"], 1);
    let repeated = fixture.json(&["index", "--codex"], 0);
    assert_eq!(repeated["data"]["reparsed"], 0);
    assert_eq!(repeated["data"]["reused"], 1);
    fixture.write("changed needle text");
    let changed = fixture.json(&["index", "--codex"], 0);
    assert_eq!(changed["data"]["reparsed"], 1);
}

#[test]
fn help_selection_scope_and_environment_are_honored() {
    let fixture = Fixture::new();
    fixture.write("needle");
    let help = fixture.run(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    for command in ["search", "read", "timeline", "index", "sessions", "entries"] {
        let row = text
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{command} ")))
            .unwrap();
        assert!(
            !row.trim_start()
                .strip_prefix(command)
                .unwrap()
                .trim()
                .is_empty(),
            "{row}"
        );
    }
    let mut command = fixture.command();
    let output = command
        .env("AGENT_HARNESS", "codex")
        .args([
            "sessions",
            "--current",
            "--scope",
            "all",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["count"],
        1
    );
    let invalid = fixture.json(&["sessions", "--current"], 2);
    assert!(
        invalid["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("AGENT_HARNESS")
    );
    let target = fixture.root().join("work/app");
    let current = fixture.json(
        &[
            "sessions",
            "--codex",
            "--scope",
            "current",
            "--cwd",
            target.to_str().unwrap(),
        ],
        0,
    );
    assert_eq!(current["data"]["count"], 1);
    let expanded = fixture.json(
        &[
            "sessions",
            "--codex",
            "--scope",
            "current",
            "--cwd",
            "~/work/app",
        ],
        0,
    );
    assert_eq!(expanded["data"]["count"], 1);
    let sibling = fixture.root().join("work/other");
    let siblings = fixture.json(
        &["sessions", "--codex", "--cwd", sibling.to_str().unwrap()],
        0,
    );
    assert_eq!(siblings["data"]["count"], 1);
    let mismatch = fixture.json(
        &[
            "sessions",
            "--codex",
            "--scope",
            "current",
            "--cwd",
            sibling.to_str().unwrap(),
        ],
        0,
    );
    assert_eq!(mismatch["data"]["count"], 0);
    let default_all = fixture.json(&["sessions", "--scope", "all"], 1);
    assert_eq!(default_all["data"]["count"], 1);
}

#[test]
fn agent_selectors_are_additive_and_all_takes_precedence() {
    let fixture = Fixture::new();
    fixture.write("needle");
    let projects = fixture.root().join(".claude/projects/app");
    fs::create_dir_all(&projects).unwrap();
    fs::write(
        projects.join("claude.jsonl"),
        json!({"type":"user","sessionId":"claude-sample","uuid":"u1","cwd":fixture.root().join("work/app"),"message":{"content":"needle"}}).to_string() + "\n",
    ).unwrap();
    let output = fixture
        .command()
        .env("AGENT_HARNESS", "CoDeX")
        .args([
            "search",
            "needle",
            "--current",
            "--claude",
            "--scope",
            "all",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let selected: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected["data"]["agents"], json!(["claude", "codex"]));
    assert_eq!(
        selected["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["ref"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["claude:claude-sample", "codex:sample"])
    );
    let all = fixture.json(
        &["search", "needle", "--all", "--codex", "--scope", "all"],
        1,
    );
    assert_eq!(
        all["data"]["agents"],
        json!([
            "claude", "codex", "cursor", "devin", "droid", "opencode", "pi"
        ])
    );
    assert_eq!(all["data"]["count"], 2);
    let invalid = fixture.json(&["sessions", "--all", "--current"], 2);
    assert_eq!(invalid["data"], Value::Null);
    assert!(
        invalid["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("AGENT_HARNESS")
    );
    let output = fixture
        .command()
        .env("AGENT_HARNESS", "CoDeX")
        .args([
            "search",
            "needle",
            "--all",
            "--current",
            "--scope",
            "all",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let selected: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected["data"]["agents"], all["data"]["agents"]);
    assert_eq!(selected["data"]["count"], 2);
}

#[test]
fn literal_regex_case_and_kind_options_reach_api() {
    let fixture = Fixture::new();
    fixture.write("a.b NEEDLE");
    let literal = fixture.json(&["search", "a.b", "--codex", "--scope", "all"], 0);
    assert_eq!(literal["data"]["count"], 1);
    let sensitive = fixture.json(
        &[
            "search",
            "needle",
            "--case-sensitive",
            "--codex",
            "--scope",
            "all",
        ],
        0,
    );
    assert_eq!(sensitive["data"]["count"], 0);
    let regex = fixture.json(
        &[
            "search",
            "a.b|reply",
            "--regex",
            "--codex",
            "--scope",
            "all",
        ],
        0,
    );
    assert_eq!(regex["data"]["results"][0]["matches"], 2);
    let tools = fixture.json(
        &[
            "search", "a.b", "--kind", "tool", "--codex", "--scope", "all",
        ],
        0,
    );
    assert_eq!(tools["data"]["count"], 0);
}

#[test]
fn timeline_parses_zones_inclusive_dates_and_rejects_dst_ambiguities() {
    let fixture = Fixture::new();
    fixture.write("needle");
    let timeline = fixture.json(
        &[
            "timeline",
            "--since",
            "2026-10-25",
            "--until",
            "2026-10-25",
            "--time-zone",
            "Europe/Berlin",
            "--codex",
            "--scope",
            "all",
        ],
        0,
    );
    assert_eq!(timeline["data"]["sessions"][0]["active_minutes"], 10.0);
    assert_eq!(timeline["data"]["daily_active_minutes"]["2026-10-25"], 10.0);
    for time in ["2026-10-25T02:30:00", "2026-03-29T02:30:00"] {
        let result = fixture.json(
            &[
                "timeline",
                "--since",
                time,
                "--until",
                "2026-10-26",
                "--time-zone",
                "Europe/Berlin",
                "--codex",
            ],
            2,
        );
        assert!(
            result["diagnostics"][0]["message"]
                .as_str()
                .unwrap()
                .contains("explicit UTC offset")
        );
    }
    let explicit = fixture.json(
        &[
            "timeline",
            "--since",
            "2026-10-25T02:00:00+02:00",
            "--until",
            "2026-10-25T03:00:00+01:00",
            "--time-zone",
            "Europe/Berlin",
            "--codex",
            "--scope",
            "all",
        ],
        0,
    );
    assert_eq!(explicit["data"]["sessions"].as_array().unwrap().len(), 1);
    for args in [
        vec!["timeline"],
        vec!["timeline", "--days", "0"],
        vec!["timeline", "--days", "-1"],
        vec!["timeline", "--since", "2026-10-25", "--days", "1"],
        vec![
            "timeline",
            "--since",
            "2026-10-25",
            "--time-zone",
            "invalid/zone",
        ],
    ] {
        assert_eq!(fixture.run(&args).status.code(), Some(2));
    }
    let duration = fixture.json(
        &[
            "timeline",
            "--days",
            "1",
            "--until",
            "2026-10-25T01:00:00Z",
            "--codex",
            "--scope",
            "all",
        ],
        0,
    );
    assert_eq!(duration["data"]["sessions"].as_array().unwrap().len(), 1);
    let mut command = fixture.command();
    let output = command
        .env("TZ", "Invalid/Zone")
        .args(["sessions", "--codex", "--time-zone", "UTC"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn fatal_index_errors_are_json_and_stderr_without_transcript() {
    let fixture = Fixture::new();
    fixture.write("private synthetic marker");
    fs::write(fixture.root().join("index"), "blocking file").unwrap();
    let output = fixture
        .command()
        .args(["sessions", "--codex", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"], Value::Null);
    assert_eq!(value["incomplete"], true);
    assert_eq!(
        value["diagnostics"][0]["message"],
        "invalid argument: index directories must be private directories with mode 0700"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "fatal: invalid argument: index directories must be private directories with mode 0700\n"
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("private synthetic marker")
    );
}

#[test]
fn post_read_timestamp_errors_keep_private_text_out_of_diagnostics() {
    let fixture = Fixture::new();
    fixture.write("private synthetic marker");
    fixture.json(&["index", "--codex"], 0);
    let db = rusqlite::Connection::open(
        fixture
            .root()
            .join("index")
            .join(slimetrail::index::DATABASE),
    )
    .unwrap();
    db.execute(
        "UPDATE entries SET timestamp=?1,timestamp_raw=NULL WHERE entry_index=0",
        [i64::MAX],
    )
    .unwrap();
    drop(db);
    for format in ["json", "text"] {
        let output = fixture.run(&["read", "codex:sample", "--format", format]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "fatal: invalid timestamp\n"
        );
        if format == "json" {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["data"], Value::Null);
            assert_eq!(value["diagnostics"][0]["message"], "invalid timestamp");
        }
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("private synthetic marker")
        );
    }
}

#[test]
fn broken_stdout_is_quiet() {
    let fixture = Fixture::new();
    fixture.write(&"needle".repeat(100_000));
    let mut child = fixture
        .command()
        .args(["entries", "codex:sample", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
}

#[test]
fn default_index_uses_xdg_data_home_and_explicit_index_wins() {
    let fixture = Fixture::new();
    fixture.write("needle");
    let output = Command::new(env!("CARGO_BIN_EXE_slimetrail"))
        .env_clear()
        .env("HOME", fixture.root())
        .env("CODEX_HOME", fixture.root().join("codex"))
        .env("XDG_DATA_HOME", fixture.root().join("data"))
        .env("TZ", "UTC")
        .args(["index", "--codex"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.root().join("data/slimetrail").is_dir());
    fixture.json(&["index", "--codex"], 0);
    assert!(fixture.root().join("index").is_dir());
}

#[test]
fn entries_keep_partial_records_and_report_final_status() {
    use std::io::Write;
    let fixture = Fixture::new();
    fixture.write("good record");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(fixture.root().join("codex/sessions/sample.jsonl"))
        .unwrap();
    writeln!(file, "{{malformed").unwrap();
    let output = fixture.json(&["entries", "codex:sample"], 1);
    assert_eq!(output["data"]["entries"][0]["text"], "good record");
    assert_eq!(output["data"]["session"]["ref"], "codex:sample");
    assert_eq!(output["incomplete"], true);
    assert_ne!(
        output["diagnostics"].as_array().unwrap().as_slice(),
        &[] as &[Value]
    );
}

#[test]
fn unavailable_unselected_root_allows_healthy_selection_and_identifies_partial_source() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let blocked = fixture.root().join("blocked");
    fs::create_dir(&blocked).unwrap();
    let pi = fixture.root().join("pi");
    fs::create_dir(&pi).unwrap();
    fs::write(pi.join("one.jsonl"), "{\"type\":\"session\",\"id\":\"healthy\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"usable needle\"}}\n").unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    let run = |mixed| {
        let mut command = fixture.command();
        command
            .env("SESSION_TRACKER_PROJECTS_DIR", blocked.join("projects"))
            .env("PI_SESSIONS_DIR", &pi)
            .args([
                "search", "needle", "--pi", "--scope", "all", "--format", "json",
            ]);
        if mixed {
            command.arg("--claude");
        }
        command.output().unwrap()
    };
    let single = run(false);
    let mixed = run(true);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        single.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&single.stderr)
    );
    assert_eq!(mixed.status.code(), Some(1));
    for output in [&single, &mixed] {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["results"][0]["ref"], "pi:healthy");
    }
    assert_eq!(
        String::from_utf8(mixed.stderr).unwrap(),
        format!(
            "source_unavailable: [claude {}] Required session store could not be enumerated\n{}",
            blocked.join("projects").display(),
            String::from_utf8(single.stderr).unwrap()
        )
    );
}

#[test]
fn malformed_source_stderr_has_identity_and_preserves_text_read_stdout() {
    let fixture = Fixture::new();
    fixture.write("usable needle");
    let complete = fixture.run(&["read", "codex:sample"]);
    let path = fixture.root().join("codex/sessions/sample.jsonl");
    let valid = fs::read_to_string(&path).unwrap();
    fs::write(&path, valid + "private malformed marker\n").unwrap();
    let partial = fixture.run(&["read", "codex:sample"]);
    assert_eq!(partial.status.code(), Some(1));
    assert_eq!(partial.stdout, complete.stdout);
    assert_eq!(
        String::from_utf8(partial.stderr).unwrap(),
        format!(
            "malformed_records: [codex {}] 1 nonempty records were not valid JSON objects\n",
            path.display()
        )
    );
}

fn tool_session(fixture: &Fixture) {
    let output = format!("early-literal {} late-literal", "x".repeat(10_000));
    let records = [
        json!({"type":"session_meta","payload":{"id":"tool","cwd":fixture.root().join("work/app")}}),
        json!({"type":"response_item","timestamp":"2026-10-25T00:30:00Z","payload":{"type":"function_call_output","call_id":"c1","output":output}}),
        json!({"type":"response_item","timestamp":"2026-10-25T00:31:00Z","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"pondered riddle"}]}}),
    ];
    fs::write(
        fixture.root().join("codex/sessions/tool.jsonl"),
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
}

fn index_settings(fixture: &Fixture) -> (String, Option<String>) {
    let db = rusqlite::Connection::open(
        fixture
            .root()
            .join("index")
            .join(slimetrail::index::DATABASE),
    )
    .unwrap();
    db.query_row("SELECT coverage,coverage_pending FROM settings", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .unwrap()
}

fn codes(value: &Value) -> Vec<&str> {
    value["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect()
}

fn search_args(query: &str) -> Vec<&str> {
    vec!["search", query, "--codex", "--scope", "all"]
}

#[test]
fn coverage_flag_rebuilds_search_documents_without_reparsing() {
    let fixture = Fixture::new();
    tool_session(&fixture);
    let built = fixture.json(&["index", "--codex"], 0);
    assert_eq!(built["data"]["reparsed"], 1);
    let compact = fixture.json(&search_args("early-literal"), 0);
    assert_eq!(compact["data"]["count"], 0);
    assert_eq!(codes(&compact), ["search_coverage"]);
    assert_eq!(compact["incomplete"], false);
    assert_eq!(
        fixture.json(&search_args("late-literal"), 0)["data"]["count"],
        1
    );
    assert_eq!(
        fixture.json(&search_args("pondered"), 0)["data"]["count"],
        0
    );
    let rebuilt = fixture.json(&["index", "--codex", "--coverage", "full"], 0);
    assert_eq!(rebuilt["data"]["reparsed"], 0);
    assert_eq!(rebuilt["data"]["reused"], 1);
    assert_eq!(
        fs::read_to_string(fixture.root().join(".config/slimetrail/config.toml")).unwrap(),
        "coverage = \"full\"\n"
    );
    assert_eq!(index_settings(&fixture), ("full".into(), None));
    for query in ["early-literal", "late-literal", "pondered"] {
        let full = fixture.json(&search_args(query), 0);
        assert_eq!(full["data"]["count"], 1, "{query}");
        assert_eq!(full["diagnostics"], json!([]));
    }
    let back = fixture.json(&["index", "--codex", "--coverage", "compact"], 0);
    assert_eq!(back["data"]["reparsed"], 0);
    assert_eq!(index_settings(&fixture), ("compact".into(), None));
    let chosen = fixture.json(&search_args("early-literal"), 0);
    assert_eq!(chosen["data"]["count"], 0);
    assert_eq!(chosen["diagnostics"], json!([]));
}

#[test]
fn coverage_flag_repairs_an_invalid_config_and_writes_through_a_symlink() {
    let fixture = Fixture::new();
    tool_session(&fixture);
    let config = fixture.root().join(".config/slimetrail/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "coverage = \"everything\"\n").unwrap();
    fixture.json(&search_args("early-literal"), 2);
    fixture.json(&["index", "--codex", "--coverage", "full"], 0);
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "coverage = \"full\"\n"
    );
    let target = fixture.root().join("dotfiles/real.toml");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::rename(&config, &target).unwrap();
    std::os::unix::fs::symlink(&target, &config).unwrap();
    fixture.json(&["index", "--codex", "--coverage", "compact"], 0);
    assert!(
        fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "coverage = \"compact\"\n"
    );
}

#[test]
fn config_mismatch_is_noted_by_queries_and_reconciled_only_by_index() {
    let fixture = Fixture::new();
    tool_session(&fixture);
    fixture.json(&["index", "--codex"], 0);
    let config = fixture.root().join("elsewhere/custom.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "coverage = \"full\"\n").unwrap();
    let run = |args: &[&str], code: i32| {
        let output = fixture
            .command()
            .env("SLIMETRAIL_CONFIG", &config)
            .args(["--format", "json"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let noted = run(&search_args("early-literal"), 0);
    assert_eq!(noted["data"]["count"], 0);
    assert_eq!(noted["incomplete"], false);
    assert_eq!(codes(&noted), ["coverage_mismatch"]);
    let message = noted["diagnostics"][0]["message"].as_str().unwrap();
    assert!(message.contains("config asks for full"), "{message}");
    assert!(message.contains("run `slimetrail index`"), "{message}");
    assert_eq!(index_settings(&fixture), ("compact".into(), None));
    assert_eq!(run(&["index", "--codex"], 0)["data"]["reparsed"], 0);
    assert_eq!(index_settings(&fixture), ("full".into(), None));
    let full = run(&search_args("early-literal"), 0);
    assert_eq!(full["data"]["count"], 1);
    assert_eq!(full["diagnostics"], json!([]));
    let mismatch = fixture.json(&search_args("early-literal"), 0);
    assert_eq!(mismatch["data"]["count"], 1);
    assert_eq!(mismatch["incomplete"], false);
    assert_eq!(codes(&mismatch), ["coverage_mismatch"]);
    assert!(
        mismatch["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("config asks for compact")
    );
    assert_eq!(index_settings(&fixture), ("full".into(), None));
}

#[test]
fn interrupted_coverage_rebuild_keeps_queries_stale_until_index_completes_it() {
    let fixture = Fixture::new();
    tool_session(&fixture);
    fixture.json(&["index", "--codex"], 0);
    let db = rusqlite::Connection::open(
        fixture
            .root()
            .join("index")
            .join(slimetrail::index::DATABASE),
    )
    .unwrap();
    db.execute("UPDATE settings SET coverage_pending='full'", [])
        .unwrap();
    drop(db);
    let stale = fixture.json(&search_args("late-literal"), 1);
    assert_eq!(stale["incomplete"], true);
    assert_eq!(
        codes(&stale),
        ["coverage_rebuild_pending", "search_coverage"]
    );
    let listed = fixture.json(&["sessions", "--codex", "--scope", "all"], 1);
    assert_eq!(codes(&listed), ["coverage_rebuild_pending"]);
    assert_eq!(
        index_settings(&fixture),
        ("compact".into(), Some("full".into()))
    );
    assert_eq!(
        fixture.json(&["index", "--codex"], 0)["data"]["reparsed"],
        0
    );
    assert_eq!(index_settings(&fixture), ("compact".into(), None));
    let complete = fixture.json(&search_args("late-literal"), 0);
    assert_eq!(complete["data"]["count"], 1);
    assert_eq!(codes(&complete), ["search_coverage"]);
}

#[test]
fn compact_kind_filters_name_the_limited_kind() {
    let fixture = Fixture::new();
    tool_session(&fixture);
    for (kind, limited) in [("meta", "--kind meta"), ("tool", "--kind tool")] {
        let mut args = search_args("literal");
        args.extend(["--kind", kind]);
        let value = fixture.json(&args, 0);
        assert_eq!(value["incomplete"], false);
        assert_eq!(codes(&value), ["coverage_excluded"]);
        assert!(
            value["diagnostics"][0]["message"]
                .as_str()
                .unwrap()
                .contains(limited)
        );
    }
    let mut args = search_args("literal");
    args.extend(["--kind", "message"]);
    assert_eq!(codes(&fixture.json(&args, 0)), ["search_coverage"]);
}

#[test]
fn config_file_location_and_validation() {
    let fixture = Fixture::new();
    tool_session(&fixture);
    let xdg = fixture.root().join("xdg");
    let xdg_file = xdg.join("slimetrail/config.toml");
    fs::create_dir_all(xdg_file.parent().unwrap()).unwrap();
    let explicit = fixture.root().join("explicit.toml");
    let run = |env: &[(&str, &Path)]| {
        let mut command = fixture.command();
        for (name, value) in env {
            command.env(name, value);
        }
        command
            .args(["--format", "json"])
            .args(search_args("early-literal"))
            .output()
            .unwrap()
    };
    for (file, env) in [
        (&xdg_file, vec![("XDG_CONFIG_HOME", xdg.as_path())]),
        (
            &explicit,
            vec![
                ("XDG_CONFIG_HOME", xdg.as_path()),
                ("SLIMETRAIL_CONFIG", explicit.as_path()),
            ],
        ),
    ] {
        fs::write(file, "coverage = \"sometimes\"\n").unwrap();
        let output = run(&env);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(&file.display().to_string()), "{stderr}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(codes(&value), ["fatal"]);
        fs::write(file, "coverage = \"full\"\n").unwrap();
        let output = run(&env);
        assert_eq!(output.status.code(), Some(0));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["count"], 1);
        assert_eq!(value["diagnostics"], json!([]));
        fs::remove_file(file).unwrap();
    }
    fs::write(&explicit, "coverage = \"full\"\nextra = 1\n").unwrap();
    let output = run(&[("SLIMETRAIL_CONFIG", explicit.as_path())]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&explicit.display().to_string()), "{stderr}");
    assert!(stderr.contains("unknown field `extra`"), "{stderr}");
    let value: Value = serde_json::from_slice(&run(&[]).stdout).unwrap();
    assert_eq!(codes(&value), ["coverage_mismatch"]);
    assert!(
        value["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("config asks for compact")
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn non_interactive_over_limit_queries_return_index_required_shapes() {
    let fixture = Fixture::new();
    fixture.write("Gated needle");
    let gated = |args: &[&str]| {
        let output = fixture
            .command()
            .env("SLIMETRAIL_TEST_BUILD_LIMIT_MS", "0")
            .args(["--format", "json", "--codex", "--scope", "all"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["incomplete"], true);
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(value["diagnostics"][0]["code"], "index_required");
        assert_eq!(value["diagnostics"][0]["agent"], "codex");
        value["data"].clone()
    };
    assert_eq!(
        gated(&["search", "needle"]),
        json!({"query":"needle","agents":["codex"],"sort":"score","count":0,"results":[]})
    );
    assert_eq!(gated(&["sessions"]), json!({"count":0,"sessions":[]}));
    assert_eq!(
        gated(&["timeline", "--days", "1"]),
        json!({"sessions":[],"daily_active_minutes":{}})
    );
    assert_eq!(gated(&["read", "codex:sample"]), Value::Null);
    assert_eq!(
        gated(&["entries", "codex:sample"]),
        json!({"entries":[],"session":null})
    );
    let text = fixture
        .command()
        .env("SLIMETRAIL_TEST_BUILD_LIMIT_MS", "0")
        .args(["entries", "codex:sample"])
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(1));
    assert_eq!(text.stdout, b"");
    assert!(String::from_utf8_lossy(&text.stderr).contains("index_required: [codex]"));
}

#[cfg(feature = "test-hooks")]
#[test]
fn index_command_builds_regardless_of_the_estimate() {
    let fixture = Fixture::new();
    fixture.write("Gated needle");
    let index = fixture
        .command()
        .env("SLIMETRAIL_TEST_BUILD_LIMIT_MS", "0")
        .args(["--format", "json", "--codex", "index"])
        .output()
        .unwrap();
    assert_eq!(index.status.code(), Some(0));
    let stats: Value = serde_json::from_slice(&index.stdout).unwrap();
    assert_eq!(stats["data"]["reparsed"], 1);
    let search = fixture
        .command()
        .env("SLIMETRAIL_TEST_BUILD_LIMIT_MS", "0")
        .args([
            "--format", "json", "--codex", "--scope", "all", "search", "needle",
        ])
        .output()
        .unwrap();
    assert_eq!(search.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(value["data"]["count"], 1);
}

#[cfg(feature = "test-hooks")]
#[test]
fn index_required_names_the_index_coverage_when_config_differs() {
    let fixture = Fixture::new();
    let projects = fixture.root().join(".claude/projects/app");
    fs::create_dir_all(&projects).unwrap();
    fs::write(
        projects.join("claude.jsonl"),
        json!({"type":"user","sessionId":"claude-sample","uuid":"u1","cwd":fixture.root().join("work/app"),"message":{"content":"needle"}}).to_string() + "\n",
    ).unwrap();
    fixture.json(&["--claude", "index"], 0);
    fixture.write("Gated needle");
    let config = fixture.root().join("config.toml");
    fs::write(&config, "coverage = \"full\"\n").unwrap();
    let output = fixture
        .command()
        .env("SLIMETRAIL_TEST_BUILD_LIMIT_MS", "0")
        .env("SLIMETRAIL_CONFIG", &config)
        .args([
            "--format", "json", "--codex", "--scope", "all", "search", "needle",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["diagnostics"][0]["code"], "index_required");
    let message = value["diagnostics"][0]["message"].as_str().unwrap();
    assert!(message.contains("at compact coverage"), "{message}");
}

#[cfg(feature = "test-hooks")]
#[test]
fn non_interactive_stale_entries_stream_once_with_index_required() {
    let fixture = Fixture::new();
    fixture.write("Published text");
    fixture.json(&["--codex", "index"], 0);
    fixture.write("Changed text that is not indexed yet");
    let output = fixture
        .command()
        .env("SLIMETRAIL_TEST_BUILD_LIMIT_MS", "0")
        .args(["entries", "codex:sample"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"Published text\nSecond reply\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("index_required: [codex]"));
}
