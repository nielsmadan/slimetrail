#[allow(dead_code)]
mod support;
use slimetrail::{
    dto::{EntryDto, SessionDto, format_timestamp},
    *,
};
use std::collections::BTreeMap;

#[test]
fn synthetic_roots_never_depend_on_process_environment() {
    let temp = tempfile::tempdir().unwrap();
    let roots = support::roots_at(temp.path());
    assert_eq!(
        roots.codex_archived_sessions,
        temp.path().join("codex/archived_sessions")
    );
    assert_eq!(
        roots.codex_session_index,
        temp.path().join("codex/session_index.jsonl")
    );
    assert_eq!(
        roots.opencode_db,
        temp.path().join("data/opencode/opencode.db")
    );
    assert_eq!(roots.claude_projects, temp.path().join(".claude/projects"));
}

#[test]
fn explicit_roots_win_and_archives_keep_agent_home() {
    let names = [
        "SESSION_TRACKER_PROJECTS_DIR",
        "CLAUDE_CONFIG_DIR",
        "CODEX_SESSIONS_DIR",
        "CODEX_HOME",
        "CURSOR_DATA_DIR",
        "CURSOR_CONFIG_DIR",
        "DEVIN_SESSIONS_DIR",
        "XDG_CONFIG_HOME",
        "FACTORY_HOME_OVERRIDE",
        "PI_SESSIONS_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "PI_CODING_AGENT_DIR",
        "OPENCODE_DB",
        "XDG_DATA_HOME",
    ];
    let env = names
        .into_iter()
        .map(|name| (name.into(), format!("/fixture/{name}")))
        .collect();
    let roots = resolve_roots(std::path::Path::new("/fixture/home"), &env).unwrap();
    for (actual, expected) in [
        (
            roots.claude_projects,
            "/fixture/SESSION_TRACKER_PROJECTS_DIR",
        ),
        (roots.codex_sessions, "/fixture/CODEX_SESSIONS_DIR"),
        (roots.codex_session_index, "/fixture/session_index.jsonl"),
        (
            roots.codex_archived_sessions,
            "/fixture/CODEX_HOME/archived_sessions",
        ),
        (roots.cursor_projects, "/fixture/CURSOR_DATA_DIR/projects"),
        (roots.cursor_chats, "/fixture/CURSOR_CONFIG_DIR/chats"),
        (
            roots.devin_sessions_db,
            "/fixture/DEVIN_SESSIONS_DIR/cli/sessions.db",
        ),
        (
            roots.droid_sessions,
            "/fixture/FACTORY_HOME_OVERRIDE/.factory/sessions",
        ),
        (roots.pi_sessions, "/fixture/PI_SESSIONS_DIR"),
        (roots.opencode_db, "/fixture/OPENCODE_DB"),
        (
            roots.opencode_legacy_storage,
            "/fixture/XDG_DATA_HOME/opencode/storage",
        ),
        (
            roots.archive.unwrap(),
            "/fixture/XDG_DATA_HOME/ringleader/archive",
        ),
    ] {
        assert_eq!(actual, std::path::Path::new(expected));
    }
}

#[test]
fn aliases_and_empty_overrides_fall_back() {
    let home = std::path::Path::new("/fixture/home");
    let mut env = BTreeMap::from([
        (
            "PI_CODING_AGENT_SESSION_DIR".into(),
            "/fixture/alias".into(),
        ),
        ("PI_SESSIONS_DIR".into(), String::new()),
        ("CURSOR_DATA_DIR".into(), "  ".into()),
        ("CURSOR_CONFIG_DIR".into(), "  ".into()),
        ("XDG_CONFIG_HOME".into(), "/fixture/config".into()),
    ]);
    let roots = resolve_roots(home, &env).unwrap();
    assert_eq!(roots.pi_sessions, std::path::Path::new("/fixture/alias"));
    assert_eq!(roots.cursor_projects, home.join(".cursor/projects"));
    assert_eq!(
        roots.cursor_chats,
        std::path::Path::new("/fixture/config/cursor/chats")
    );
    env.remove("PI_CODING_AGENT_SESSION_DIR");
    env.insert("PI_CODING_AGENT_DIR".into(), "/fixture/pi".into());
    assert_eq!(
        resolve_roots(home, &env).unwrap().pi_sessions,
        std::path::Path::new("/fixture/pi/sessions")
    );
}

#[test]
fn stable_wire_envelope_matches_golden() {
    let value = Envelope {
        version: 1,
        incomplete: false,
        diagnostics: vec![],
        data: Some(SessionDto::try_from(&support::session()).unwrap()),
    };
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/session-envelope.json")).unwrap();
    assert_eq!(serde_json::to_value(value).unwrap(), expected);
}

#[test]
fn enums_and_exact_references_round_trip() {
    for agent in [
        Agent::Claude,
        Agent::Codex,
        Agent::Cursor,
        Agent::Droid,
        Agent::Opencode,
        Agent::Pi,
    ] {
        let key: SessionKey = format!("{agent}:synthetic:child").parse().unwrap();
        assert_eq!(key.reference(), format!("{agent}:synthetic:child"));
        assert_eq!(serde_json::to_value(agent).unwrap(), agent.to_string());
    }
    for invalid in ["codex", "codex:", "unknown:id", ":id"] {
        assert!(invalid.parse::<SessionKey>().is_err());
    }
    assert_eq!(serde_json::to_value(Kind::Tool).unwrap(), "tool");
    assert_eq!(serde_json::to_value(ReadMode::Full).unwrap(), "full");
    assert_eq!(serde_json::to_value(Scope::Siblings).unwrap(), "siblings");
    assert_eq!(serde_json::to_value(Sort::Recent).unwrap(), "recent");
}

#[test]
fn timestamp_dto_preserves_original_offset_and_falls_back_to_utc() {
    let mut entry = Entry {
        session: support::session().key,
        index: 0,
        source_id: "record:0".into(),
        timestamp: Some(0),
        timestamp_raw: Some("1970-01-01T01:00:00+01:00".into()),
        role: "user".into(),
        kind: Kind::Message,
        subtype: None,
        tool_name: None,
        automated: false,
        text: "synthetic 🐌".into(),
        searchable: Searchable::Text,
    };
    assert_eq!(
        EntryDto::try_from(&entry).unwrap().timestamp.as_deref(),
        Some("1970-01-01T01:00:00+01:00")
    );
    entry.timestamp_raw = None;
    assert_eq!(
        EntryDto::try_from(&entry).unwrap().timestamp.as_deref(),
        Some("1970-01-01T00:00:00Z")
    );
    assert_eq!(format_timestamp(-1).unwrap(), "1969-12-31T23:59:59.999Z");
    assert!(format_timestamp(i64::MAX).is_err());
}

#[test]
fn agent_home_overrides_supply_default_subdirectories() {
    let home = std::path::Path::new("/fixture/home");
    let env = BTreeMap::from([
        ("CLAUDE_CONFIG_DIR".into(), "/fixture/claude".into()),
        ("CODEX_HOME".into(), "/fixture/codex".into()),
        ("PI_CODING_AGENT_DIR".into(), "/fixture/pi".into()),
    ]);
    let roots = resolve_roots(home, &env).unwrap();
    assert_eq!(
        roots.claude_projects,
        std::path::Path::new("/fixture/claude/projects")
    );
    assert_eq!(
        roots.codex_sessions,
        std::path::Path::new("/fixture/codex/sessions")
    );
    assert_eq!(
        roots.pi_sessions,
        std::path::Path::new("/fixture/pi/sessions")
    );
    let defaults = resolve_roots(home, &BTreeMap::new()).unwrap();
    assert_eq!(defaults.cursor_chats, home.join(".cursor/chats"));
    assert_eq!(defaults.droid_sessions, home.join(".factory/sessions"));
    assert_eq!(
        defaults.devin_sessions_db,
        home.join(".local/share/devin/cli/sessions.db")
    );
    assert_eq!(defaults.pi_sessions, home.join(".pi/agent/sessions"));
    assert_eq!(
        defaults.opencode_db,
        home.join(".local/share/opencode/opencode.db")
    );
}
