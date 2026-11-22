use crate::{
    Agent, Context, Diagnostic, Discovery, DiscoveryCache, Entry, Error, Fingerprint, Kind,
    ParsedSession, RawSession, Result, Session, SessionKey, TitleOrigin,
};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct FileReader {
    agent: Agent,
    archives: Vec<PathBuf>,
    shared: Option<PathBuf>,
    names: Option<PathBuf>,
    titles: RefCell<Option<(Fingerprint, Arc<Titles>)>>,
}

struct Titles {
    names: BTreeMap<String, Option<String>>,
    diagnostics: Vec<Diagnostic>,
}

fn read_titles(path: &Path) -> Titles {
    let mut names = BTreeMap::new();
    let status = records(path, &mut |_, record| {
        if let (Some(id), Some(name)) = (record["id"].as_str(), record["thread_name"].as_str()) {
            names.insert(
                id.to_owned(),
                (!name.trim().is_empty()).then(|| name.trim().to_owned()),
            );
        }
        Ok(true)
    });
    let diagnostic = match status {
        Ok(0) => None,
        Ok(_) => Some(diagnostic(
            Agent::Codex,
            path,
            "malformed_metadata",
            "Session names contain malformed records",
        )),
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(diagnostic(
            Agent::Codex,
            path,
            "source_unavailable",
            "Session names could not be read",
        )),
    };
    Titles {
        names,
        diagnostics: diagnostic.into_iter().collect(),
    }
}

fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}

pub fn fingerprint(paths: &[PathBuf]) -> Result<Fingerprint> {
    let mut values = Vec::with_capacity(paths.len());
    for path in paths {
        let value = match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error::InvalidArgument(
                    "source dependency is a symlink".into(),
                ));
            }
            Ok(m) => format!(
                "{}:{}:{}:{}:{}:{}:{}",
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing".into(),
            Err(e) => return Err(io(path, e)),
        };
        values.push((path, value));
    }
    Ok(Fingerprint {
        signature: serde_json::to_string(&values)?,
    })
}

pub fn diagnostic(agent: Agent, path: &Path, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        agent: Some(agent),
        source: Some(path.to_string_lossy().into_owned()),
        code: code.into(),
        message: message.into(),
    }
}

fn walk(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(root).map_err(|e| io(root, e))?;
    if metadata.file_type().is_symlink() {
        return Err(Error::InvalidArgument("source root is a symlink".into()));
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).map_err(|e| io(&directory, e))? {
            let entry = entry.map_err(|e| io(&directory, e))?;
            let ty = entry.file_type().map_err(|e| io(&entry.path(), e))?;
            if ty.is_dir() {
                directories.push(entry.path());
            } else if ty.is_file() && entry.path().extension().is_some_and(|x| x == "jsonl") {
                files.push(entry.path());
            }
        }
    }
    Ok(())
}

pub fn records(path: &Path, visit: &mut dyn FnMut(u64, &Value) -> Result<bool>) -> Result<u64> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if !metadata.is_file() {
        return Err(Error::InvalidArgument(
            "transcript must be a regular file".into(),
        ));
    }
    let file = File::open(path).map_err(|e| io(path, e))?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut line = 0;
    let mut malformed = 0;
    loop {
        bytes.clear();
        if reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| io(path, e))?
            == 0
        {
            break;
        }
        line += 1;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => {
                if !visit(line, &value)? {
                    break;
                }
            }
            _ => malformed += 1,
        }
    }
    Ok(malformed)
}

pub fn string(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()
        .map(jiff::Timestamp::as_millisecond)
}

#[derive(Default)]
struct Header {
    id: Option<String>,
    parent: Option<String>,
    cwd: Option<PathBuf>,
    branch: Option<String>,
    automated: bool,
}

fn header(agent: Agent, path: &Path) -> Result<Header> {
    let mut header = Header::default();
    records(path, &mut |_, record| {
        let kind = record["type"].as_str().unwrap_or("");
        let payload = &record["payload"];
        match agent {
            Agent::Claude => {
                if let Some(id) = string(record, "sessionId") {
                    if path
                        .parent()
                        .is_some_and(|p| p.file_name().is_some_and(|n| n == "subagents"))
                    {
                        header.id = string(record, "agentId")
                            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()));
                        header.parent = Some(id);
                        header.automated = true;
                    } else {
                        header.id = Some(id);
                    }
                    header.cwd = string(record, "cwd").map(PathBuf::from);
                    header.automated |= record["isSidechain"].as_bool().unwrap_or(false);
                    return Ok(false);
                }
            }
            Agent::Codex if kind == "session_meta" => {
                header.id = string(payload, "id").or_else(|| string(payload, "session_id"));
                header.parent = string(payload, "parent_thread_id")
                    .or_else(|| string(payload, "forked_from_id"))
                    .or_else(|| {
                        string(
                            &payload["source"]["subagent"]["thread_spawn"],
                            "parent_thread_id",
                        )
                    })
                    .or_else(|| {
                        string(payload, "session_id").filter(|s| Some(s) != header.id.as_ref())
                    });
                header.cwd = string(payload, "cwd").map(PathBuf::from);
                header.branch = string(&payload["git"], "branch");
                header.automated = !payload["source"].is_null()
                    && payload["source"] != "cli"
                    && payload["source"] != "vscode";
                return Ok(false);
            }
            Agent::Droid if kind == "session_start" => {
                header.id = string(record, "id");
                header.parent = string(record, "callingSessionId");
                header.cwd = string(record, "cwd").map(PathBuf::from);
                header.automated = header.parent.is_some();
                return Ok(false);
            }
            Agent::Pi if kind == "session" => {
                header.id = string(record, "id");
                header.parent = string(record, "parentSessionId").or_else(|| {
                    string(record, "parentSession").map(|s| fallback_id(agent, Path::new(&s)))
                });
                header.cwd = string(record, "cwd").map(PathBuf::from);
                header.automated = record["isSubagent"].as_bool().unwrap_or(false)
                    || record["automated"].as_bool().unwrap_or(false)
                    || path
                        .components()
                        .any(|part| part.as_os_str() == "subagents");
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    })?;
    if agent == Agent::Claude
        && path
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "subagents"))
    {
        header.automated = true;
        header.parent = header.parent.or_else(|| {
            path.parent()?
                .parent()?
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        });
    }
    Ok(header)
}

fn fallback_id(agent: Agent, path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    if agent == Agent::Pi {
        stem.split_once('_')
            .map_or_else(|| stem.to_string(), |(_, id)| id.to_string())
    } else {
        stem.into_owned()
    }
}

pub(crate) enum ArchiveKind {
    Agent(Agent),
    Unknown,
}

pub(crate) fn archive_kind(path: &Path) -> Result<ArchiveKind> {
    let mut found = ArchiveKind::Unknown;
    records(path, &mut |_, record| {
        let kind = record["type"].as_str().unwrap_or("");
        let agent = match kind {
            "session_meta" if record["payload"].is_object() => Some(Agent::Codex),
            "session_start" => Some(Agent::Droid),
            "session" => Some(Agent::Pi),
            _ => None,
        };
        if let Some(agent) = agent {
            found = ArchiveKind::Agent(agent);
            return Ok(false);
        }
        if record.get("type").is_none()
            && record["role"].is_string()
            && record["message"].is_object()
        {
            found = ArchiveKind::Agent(Agent::Cursor);
            return Ok(false);
        }
        if matches!(kind, "session_meta" | "session_start" | "session")
            || (kind == "message" && record["message"]["role"].is_string())
            || record["role"].is_string()
        {
            return Ok(false);
        }
        if matches!(kind, "custom-title" | "ai-title")
            || (matches!(kind, "user" | "assistant")
                && (record["message"].is_object() || record["message"].is_string()))
        {
            found = ArchiveKind::Agent(Agent::Claude);
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(found)
}

impl FileReader {
    pub fn new(agent: Agent, ctx: &Context) -> Self {
        let shared = ctx.roots.archive.clone();
        let mut archives = match agent {
            Agent::Codex => vec![ctx.roots.codex_archived_sessions.clone()],
            _ => Vec::new(),
        };
        archives.extend(shared.iter().cloned());
        Self {
            agent,
            archives,
            shared,
            names: (agent == Agent::Codex).then(|| ctx.roots.codex_session_index.clone()),
            titles: RefCell::new(None),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered parsing states together"
    )]
    pub fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
        let root = match self.agent {
            Agent::Claude => &ctx.roots.claude_projects,
            Agent::Codex => &ctx.roots.codex_sessions,
            Agent::Droid => &ctx.roots.droid_sessions,
            Agent::Pi => &ctx.roots.pi_sessions,
            _ => return Err(Error::InvalidArgument("not a JSONL agent".into())),
        };
        let mut diagnostics = Vec::new();
        let mut files = Vec::new();
        if walk(root, &mut files).is_err() {
            diagnostics.push(diagnostic(
                self.agent,
                root,
                "source_unavailable",
                "Required session store could not be enumerated",
            ));
        }
        for archive in &self.archives {
            if fs::symlink_metadata(archive).is_ok_and(|m| m.is_dir()) {
                if walk(archive, &mut files).is_err() {
                    diagnostics.push(diagnostic(
                        self.agent,
                        archive,
                        "source_unavailable",
                        "Archive could not be enumerated",
                    ));
                }
            } else if fs::symlink_metadata(archive)
                .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
                || fs::symlink_metadata(archive).is_ok()
            {
                diagnostics.push(diagnostic(
                    self.agent,
                    archive,
                    "source_unavailable",
                    "Archive could not be enumerated",
                ));
            }
        }
        files.sort();
        files.dedup();
        let mut chosen: BTreeMap<String, (bool, i128, RawSession)> = BTreeMap::new();
        for path in files {
            let source = path.to_string_lossy().into_owned();
            let mut dependencies = vec![path.clone()];
            dependencies.extend(self.names.iter().cloned());
            if self.agent == Agent::Droid {
                dependencies.push(path.with_extension("settings.json"));
            }
            let Ok(fingerprint) = fingerprint(&dependencies) else {
                diagnostics.push(diagnostic(
                    self.agent,
                    &path,
                    "source_unavailable",
                    "Session metadata could not be read",
                ));
                continue;
            };
            let cached = previous
                .by_source
                .get(&source)
                .filter(|c| c.fingerprint == fingerprint && c.key.agent == self.agent);
            if self.shared.as_ref().is_some_and(|a| path.starts_with(a)) && cached.is_none() {
                let owner = self.agent == Agent::Claude;
                match archive_kind(&path) {
                    Ok(ArchiveKind::Agent(agent)) if agent == self.agent => {}
                    Ok(ArchiveKind::Unknown) if owner => {
                        diagnostics.push(diagnostic(
                            self.agent,
                            &path,
                            "archive_format_unknown",
                            "Archive transcript format could not be identified",
                        ));
                        continue;
                    }
                    Err(_) if owner => {
                        diagnostics.push(diagnostic(
                            self.agent,
                            &path,
                            "source_unavailable",
                            "Archive transcript could not be read",
                        ));
                        continue;
                    }
                    _ => continue,
                }
            }
            let result: Result<(i128, RawSession)> = (|| {
                let key = if let Some(cached) = cached {
                    cached.key.clone()
                } else {
                    SessionKey {
                        agent: self.agent,
                        id: header(self.agent, &path)?
                            .id
                            .unwrap_or_else(|| fallback_id(self.agent, &path)),
                    }
                };
                let m = fs::metadata(&path).map_err(|e| io(&path, e))?;
                let modified = i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec());
                Ok((
                    modified,
                    RawSession {
                        key,
                        source: source.clone(),
                        dependencies,
                        fingerprint,
                        snapshot: None,
                    },
                ))
            })();
            match result {
                Ok((modified, raw)) => {
                    let live = !self.archives.iter().any(|a| path.starts_with(a));
                    let replace =
                        chosen
                            .get(&raw.key.id)
                            .is_none_or(|(old_live, old_time, old)| {
                                match (live, modified).cmp(&(*old_live, *old_time)) {
                                    std::cmp::Ordering::Greater => true,
                                    std::cmp::Ordering::Equal => raw.source < old.source,
                                    std::cmp::Ordering::Less => false,
                                }
                            });
                    if replace {
                        chosen.insert(raw.key.id.clone(), (live, modified, raw));
                    }
                }
                Err(_) => diagnostics.push(diagnostic(
                    self.agent,
                    &path,
                    "source_unavailable",
                    "Session metadata could not be read",
                )),
            }
        }
        let complete_agents = if diagnostics.is_empty() {
            BTreeSet::from([self.agent])
        } else {
            BTreeSet::new()
        };
        Ok(Discovery {
            sessions: chosen.into_values().map(|(_, _, raw)| raw).collect(),
            diagnostics,
            complete_agents,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered parsing states together"
    )]
    pub fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession> {
        let path = Path::new(&raw.source);
        let h = header(self.agent, path)?;
        let key = SessionKey {
            agent: self.agent,
            id: h.id.unwrap_or_else(|| fallback_id(self.agent, path)),
        };
        let metadata = fs::metadata(path).map_err(|e| io(path, e))?;
        let mut session = Session {
            key,
            title: None,
            title_origin: None,
            source_updated_at: Some(metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000),
            cwd: h.cwd,
            branch: h.branch,
            started_at: None,
            updated_at: None,
            parent: h.parent.map(|id| SessionKey {
                agent: self.agent,
                id,
            }),
            source: raw.source.clone(),
            archived: self.archives.iter().any(|a| path.starts_with(a)),
        };
        let mut automated = h.automated;
        let mut diagnostics = Vec::new();
        if self.agent == Agent::Droid {
            let settings = path.with_extension("settings.json");
            fingerprint(std::slice::from_ref(&settings))?;
            match File::open(&settings) {
                Ok(file) => match serde_json::from_reader::<_, Value>(BufReader::new(file)) {
                    Ok(value) if value.is_object() => {
                        automated |= value["tags"]
                            .as_array()
                            .is_some_and(|tags| tags.iter().any(|tag| tag["name"] == "exec"));
                    }
                    _ => diagnostics.push(diagnostic(
                        self.agent,
                        &settings,
                        "malformed_metadata",
                        "Settings are not a JSON object",
                    )),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => diagnostics.push(diagnostic(
                    self.agent,
                    &settings,
                    "source_unavailable",
                    "Settings could not be read",
                )),
            }
        }
        if let Some(names) = &self.names {
            let signature = fingerprint(std::slice::from_ref(names))?;
            let cached = self
                .titles
                .borrow()
                .as_ref()
                .filter(|(old, _)| *old == signature)
                .map(|(_, titles)| Arc::clone(titles));
            let titles = if let Some(titles) = cached {
                titles
            } else {
                let titles = Arc::new(read_titles(names));
                if fingerprint(std::slice::from_ref(names))? != signature {
                    return Err(Error::InvalidArgument(
                        "Session names changed during parsing".into(),
                    ));
                }
                *self.titles.borrow_mut() = Some((signature, Arc::clone(&titles)));
                titles
            };
            session.title = titles.names.get(&session.key.id).cloned().flatten();
            session.title_origin = session.title.as_ref().map(|_| TitleOrigin::Native);
            diagnostics.extend(titles.diagnostics.iter().cloned());
        }
        let mut index = 0;
        let malformed = records(path, &mut |line, record| {
            update_metadata(&mut session, record);
            let mut found = normalize(self.agent, record);
            let base = string(record, "uuid")
                .or_else(|| string(record, "id"))
                .or_else(|| string(&record["payload"], "id"))
                .unwrap_or_else(|| format!("record:{line}"));
            for (ordinal, item) in found.iter_mut().enumerate() {
                super::derive_title(&mut session, &item.role, item.kind, &item.text);
                item.session = session.key.clone();
                item.index = index;
                item.source_id = format!("{base}:{ordinal}");
                item.automated |= automated;
                index += 1;
                emit(item.clone())?;
            }
            Ok(true)
        })?;
        if malformed > 0 {
            diagnostics.push(diagnostic(
                self.agent,
                path,
                "malformed_records",
                &format!("{malformed} nonempty records were not valid JSON objects"),
            ));
        }
        Ok(ParsedSession {
            session,
            complete: diagnostics.is_empty(),
            diagnostics,
        })
    }
}

fn update_metadata(session: &mut Session, record: &Value) {
    for value in [&record["timestamp"], &record["payload"]["timestamp"]] {
        if let Some(ts) = timestamp(value) {
            session.started_at = Some(session.started_at.map_or(ts, |old| old.min(ts)));
            session.updated_at = Some(session.updated_at.map_or(ts, |old| old.max(ts)));
        }
    }
    if session.cwd.is_none() {
        session.cwd = string(record, "cwd").map(PathBuf::from);
    }
    if let Some(branch) = string(record, "gitBranch").or_else(|| string(&record["git"], "branch")) {
        session.branch = Some(branch);
    }
    let kind = record["type"].as_str().unwrap_or("");
    let title = match session.key.agent {
        Agent::Claude => string(record, "customTitle")
            .or_else(|| string(record, "aiTitle"))
            .or_else(|| string(record, "slug")),
        Agent::Droid if kind == "session_start" => string(record, "title"),
        Agent::Pi if matches!(kind, "session" | "session_name" | "session_info") => record["name"]
            .as_str()
            .map(|s| s.replace(['\r', '\n'], " ").trim().to_string()),
        _ => None,
    };
    if let Some(title) = title {
        session.title = (!title.is_empty()).then_some(title);
        session.title_origin = session.title.as_ref().map(|_| TitleOrigin::Native);
    }
}

fn python_json(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    serde_json::to_string(k).expect("string serialization"),
                    python_json(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}

pub fn flatten(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(flatten)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(items) => [
            "text",
            "thinking",
            "reason",
            "name",
            "tool",
            "arguments",
            "input",
            "output",
            "content",
            "error",
        ]
        .into_iter()
        .filter_map(|key| items.get(key))
        .filter(|v| !v.is_null())
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            _ => python_json(v),
        })
        .collect::<Vec<_>>()
        .join("\n"),
        _ => String::new(),
    }
}

fn tool_block(value: &Value) -> bool {
    matches!(
        value["type"].as_str(),
        Some(
            "tool_use"
                | "tool_result"
                | "tool-call"
                | "tool-result"
                | "tool_call"
                | "function_call"
                | "function_call_output"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "toolCall"
                | "toolResult"
        )
    )
}

fn entry(agent: Agent, role: &str, kind: Kind, text: String, record: &Value) -> Entry {
    Entry {
        session: SessionKey {
            agent,
            id: String::new(),
        },
        index: 0,
        source_id: String::new(),
        timestamp: timestamp(&record["timestamp"]),
        timestamp_raw: string(record, "timestamp"),
        role: role.into(),
        kind,
        subtype: None,
        tool_name: None,
        automated: false,
        text,
    }
}

pub(crate) fn content_entries(
    agent: Agent,
    role: &str,
    value: &Value,
    record: &Value,
) -> Vec<Entry> {
    fn split(agent: Agent, role: &str, value: &Value, record: &Value, entries: &mut Vec<Entry>) {
        if let Value::Array(items) = value {
            for item in items {
                split(agent, role, item, record, entries);
            }
            return;
        }
        let text = flatten(value);
        if text.trim().is_empty() {
            return;
        }
        let kind = if tool_block(value) {
            Kind::Tool
        } else if matches!(value["type"].as_str(), Some("thinking" | "reasoning")) {
            Kind::Meta
        } else {
            Kind::Message
        };
        let name = (kind == Kind::Tool)
            .then(|| string(value, "name").or_else(|| string(value, "tool")))
            .flatten();
        if let Some(last) = entries.last_mut().filter(|last| last.kind == kind) {
            last.text.push('\n');
            last.text.push_str(&text);
            last.tool_name = last.tool_name.take().or(name);
        } else {
            let mut item = entry(agent, role, kind, text, record);
            item.tool_name = name;
            item.subtype = (kind == Kind::Meta).then(|| "reasoning".into());
            entries.push(item);
        }
    }
    let mut entries = Vec::new();
    split(agent, role, value, record, &mut entries);
    entries
}

fn metadata_entries(agent: Agent, record: &Value) -> Vec<Entry> {
    let payload = if agent == Agent::Codex && record["payload"].is_object() {
        &record["payload"]
    } else {
        record
    };
    string(payload, "summary")
        .or_else(|| string(payload, "text"))
        .filter(|text| !text.trim().is_empty())
        .map(|text| {
            let mut item = entry(agent, "meta", Kind::Meta, text, record);
            item.subtype = string(payload, "type").or_else(|| string(record, "type"));
            vec![item]
        })
        .unwrap_or_default()
}

fn tool_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        _ => Some(python_json(value)),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the ordered parsing states together"
)]
fn normalize(agent: Agent, record: &Value) -> Vec<Entry> {
    let kind = record["type"].as_str().unwrap_or("unknown");
    match agent {
        Agent::Claude => {
            let message = &record["message"];
            let mut found = content_entries(
                agent,
                kind,
                if message.is_object() {
                    &message["content"]
                } else {
                    message
                },
                record,
            );
            if let Some(text) = string(record, "toolUseResult").filter(|s| !s.trim().is_empty()) {
                found.push(entry(agent, kind, Kind::Tool, text, record));
            }
            if found.is_empty()
                && let Some(text) = string(record, "summary").or_else(|| string(record, "text"))
            {
                let mut item = entry(agent, kind, Kind::Meta, text, record);
                item.subtype = Some(kind.into());
                found.push(item);
            }
            let automated = record["isMeta"].as_bool().unwrap_or(false)
                || record["isCompactSummary"].as_bool().unwrap_or(false)
                || record["isSidechain"].as_bool().unwrap_or(false)
                || string(&record["origin"], "kind").is_some_and(|s| s != "human");
            for item in &mut found {
                item.automated = automated;
            }
            found
        }
        Agent::Codex => {
            let payload = &record["payload"];
            if kind == "turn_context" {
                return string(payload, "summary")
                    .filter(|s| !s.trim().is_empty())
                    .map(|text| vec![entry(agent, "summary", Kind::Meta, text, record)])
                    .unwrap_or_default();
            }
            if kind != "response_item" {
                return metadata_entries(agent, record);
            }
            let subtype = payload["type"].as_str().unwrap_or("response_item");
            let role = payload["role"].as_str().unwrap_or(subtype);
            if matches!(role, "developer" | "system") {
                return Vec::new();
            }
            let text = match subtype {
                "function_call" => [string(payload, "name"), tool_text(&payload["arguments"])]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n"),
                "custom_tool_call" => [string(payload, "name"), tool_text(&payload["input"])]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n"),
                "function_call_output" | "custom_tool_call_output" => {
                    tool_text(&payload["output"]).unwrap_or_default()
                }
                "reasoning" => {
                    let text = flatten(&payload["content"]);
                    if text.is_empty() {
                        flatten(&payload["summary"])
                    } else {
                        text
                    }
                }
                _ => {
                    let text = flatten(&payload["content"]);
                    if role == "user" && text.trim_start().starts_with("# AGENTS.md instructions") {
                        return Vec::new();
                    }
                    let found = content_entries(agent, role, &payload["content"], record);
                    return if found.is_empty() {
                        metadata_entries(agent, record)
                    } else {
                        found
                    };
                }
            };
            if text.trim().is_empty() {
                return Vec::new();
            }
            let mut item = entry(
                agent,
                role,
                if subtype == "reasoning" {
                    Kind::Meta
                } else {
                    Kind::Tool
                },
                text,
                record,
            );
            item.subtype = Some(subtype.into());
            item.tool_name = string(payload, "name");
            vec![item]
        }
        Agent::Droid | Agent::Pi if kind == "message" && record["message"].is_object() => {
            let message = &record["message"];
            let role = message["role"].as_str().unwrap_or("message");
            let mut found = content_entries(agent, role, &message["content"], record);
            for item in &mut found {
                item.automated = string(message, "hookEventName").is_some()
                    || message["automated"].as_bool().unwrap_or(false);
            }
            if matches!(role, "toolResult" | "tool") {
                for item in &mut found {
                    item.kind = Kind::Tool;
                    item.tool_name = string(message, "toolName");
                }
            }
            found
        }
        _ => metadata_entries(agent, record),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_cache_reuses_final_map_and_tracks_revisions_and_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            roots: crate::resolve_roots(dir.path(), &BTreeMap::new()).unwrap(),
            index_dir: dir.path().join("index"),
            cwd: dir.path().into(),
            now_ms: 0,
            time_zone: "UTC".into(),
        };
        fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
        for id in ["one", "two"] {
            fs::write(
                ctx.roots.codex_sessions.join(format!("{id}.jsonl")),
                format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n"),
            )
            .unwrap();
        }
        let names = &ctx.roots.codex_session_index;
        fs::write(names, "{\"id\":\"one\",\"thread_name\":\"earlier\"}\n{\"id\":\"one\",\"thread_name\":\"last\"}\n{\"id\":\"two\",\"thread_name\":\"title\"}\n{\"id\":\"two\",\"thread_name\":\" \"}\nmalformed\n").unwrap();
        let reader = FileReader::new(Agent::Codex, &ctx);
        let discovery = reader
            .discover(
                &ctx,
                &DiscoveryCache {
                    by_source: BTreeMap::new(),
                },
            )
            .unwrap();
        let first = reader
            .parse(&discovery.sessions[0], &mut |_| Ok(()))
            .unwrap();
        let cached = Arc::clone(&reader.titles.borrow().as_ref().unwrap().1);
        let second = reader
            .parse(&discovery.sessions[1], &mut |_| Ok(()))
            .unwrap();
        assert!(Arc::ptr_eq(
            &cached,
            &reader.titles.borrow().as_ref().unwrap().1
        ));
        assert_eq!(first.session.title.as_deref(), Some("last"));
        assert_eq!(second.session.title, None);
        assert_eq!(first.diagnostics, second.diagnostics);
        assert_eq!(first.diagnostics[0].code, "malformed_metadata");
        fs::write(names, "{\"id\":\"one\",\"thread_name\":\"revised\"}\n").unwrap();
        let revised = reader
            .parse(&discovery.sessions[0], &mut |_| Ok(()))
            .unwrap();
        assert_eq!(revised.session.title.as_deref(), Some("revised"));
        assert!(revised.complete);
        assert!(!Arc::ptr_eq(
            &cached,
            &reader.titles.borrow().as_ref().unwrap().1
        ));
        fs::remove_file(names).unwrap();
        fs::create_dir(names).unwrap();
        let unavailable = reader
            .parse(&discovery.sessions[0], &mut |_| Ok(()))
            .unwrap();
        assert_eq!(unavailable.session.title, None);
        assert_eq!(unavailable.diagnostics[0].code, "source_unavailable");
    }
}
