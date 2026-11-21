use crate::{Result, Roots};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn resolve_roots(home: &Path, env: &BTreeMap<String, String>) -> Result<Roots> {
    let value = |name: &str| env.get(name).filter(|s| !s.is_empty()).map(PathBuf::from);
    let codex_home = value("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
    let xdg_data = value("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    let cursor_value = |name: &str| {
        env.get(name)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    let cursor_data = cursor_value("CURSOR_DATA_DIR").unwrap_or_else(|| home.join(".cursor"));
    let cursor_config = cursor_value("CURSOR_CONFIG_DIR")
        .or_else(|| cursor_value("XDG_CONFIG_HOME").map(|p| p.join("cursor")))
        .unwrap_or_else(|| home.join(".cursor"));
    Ok(Roots {
        claude_projects: value("SESSION_TRACKER_PROJECTS_DIR").unwrap_or_else(|| {
            value("CLAUDE_CONFIG_DIR")
                .unwrap_or_else(|| home.join(".claude"))
                .join("projects")
        }),
        codex_sessions: value("CODEX_SESSIONS_DIR").unwrap_or_else(|| codex_home.join("sessions")),
        codex_archived_sessions: codex_home.join("archived_sessions"),
        codex_session_index: value("CODEX_SESSIONS_DIR")
            .and_then(|p| p.parent().map(|p| p.join("session_index.jsonl")))
            .unwrap_or_else(|| codex_home.join("session_index.jsonl")),
        cursor_projects: cursor_data.join("projects"),
        cursor_chats: cursor_config.join("chats"),
        devin_sessions_db: value("DEVIN_SESSIONS_DIR")
            .unwrap_or_else(|| xdg_data.join("devin"))
            .join("cli/sessions.db"),
        droid_sessions: value("FACTORY_HOME_OVERRIDE")
            .unwrap_or_else(|| home.to_path_buf())
            .join(".factory/sessions"),
        pi_sessions: value("PI_SESSIONS_DIR")
            .or_else(|| value("PI_CODING_AGENT_SESSION_DIR"))
            .unwrap_or_else(|| {
                value("PI_CODING_AGENT_DIR")
                    .unwrap_or_else(|| home.join(".pi/agent"))
                    .join("sessions")
            }),
        opencode_db: value("OPENCODE_DB").unwrap_or_else(|| xdg_data.join("opencode/opencode.db")),
        opencode_legacy_storage: xdg_data.join("opencode/storage"),
        archive: Some(xdg_data.join("ringleader/archive")),
    })
}
