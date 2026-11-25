mod scan;
mod titles;

use crate::{
    Agent, CachedSource, Context, Diagnostic, Discovery, DiscoveryCache, Entry, Error, Fingerprint,
    Kind, ParsedSession, RawSession, Result, Searchable, Session, SessionKey, SourceState,
    Timestamp, TitleOrigin,
};
use scan::{appendable, leading, open_regular, recent, scan_file};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::BufReader,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use titles::Names;

pub use scan::{Tally, records};
pub use titles::drifted;
#[cfg(test)]
pub(crate) use titles::{FAILING_TITLE_READS, hashed};

pub struct FileReader {
    agent: Agent,
    archives: Vec<PathBuf>,
    shared: Option<PathBuf>,
    names: Option<Names>,
    now_ms: Timestamp,
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
        title_mark: None,
    })
}

struct Dependencies<'a> {
    transcript: &'a PathBuf,
    companions: &'a [PathBuf],
}

impl<'a> Dependencies<'a> {
    fn of(raw: &'a RawSession) -> Option<Self> {
        let (transcript, companions) = raw.dependencies.split_first()?;
        Some(Self {
            transcript,
            companions,
        })
    }
}

pub fn transcript(raw: &RawSession) -> Option<&Path> {
    Dependencies::of(raw).map(|dependencies| dependencies.transcript.as_path())
}

pub fn diagnostic(agent: Agent, path: &Path, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        agent: Some(agent),
        source: Some(path.to_string_lossy().into_owned()),
        code: code.into(),
        message: message.into(),
    }
}

fn resume_rejected() -> Error {
    Error::InvalidArgument("append resume state no longer matches the source".into())
}

fn compressed(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "zst")
        && path
            .file_stem()
            .is_some_and(|s| Path::new(s).extension().is_some_and(|x| x == "jsonl"))
}

fn readable(path: PathBuf) -> PathBuf {
    let plain = path.with_extension("");
    if compressed(&path) && fs::symlink_metadata(&plain).is_ok_and(|m| m.is_file()) {
        plain
    } else {
        path
    }
}

fn walk(root: &Path, files: &mut Vec<PathBuf>, zst: bool) -> Result<()> {
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
            } else if ty.is_file()
                && (entry.path().extension().is_some_and(|x| x == "jsonl")
                    || (zst && compressed(&entry.path())))
            {
                files.push(entry.path());
            }
        }
    }
    Ok(())
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

fn header_record(agent: Agent, path: &Path, header: &mut Header, record: &Value) -> bool {
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
                return false;
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
            return false;
        }
        Agent::Droid if kind == "session_start" => {
            header.id = string(record, "id");
            header.parent = string(record, "callingSessionId");
            header.cwd = string(record, "cwd").map(PathBuf::from);
            header.automated = header.parent.is_some();
            return false;
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
            return false;
        }
        _ => {}
    }
    true
}

fn header(agent: Agent, path: &Path, file: &File) -> Result<Header> {
    let mut header = Header::default();
    scan_file(path, file, false, &mut |_, record| {
        Ok(header_record(agent, path, &mut header, record))
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
    let plain = path.with_extension("");
    let path = if compressed(path) { &plain } else { path };
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
    leading(path, &mut |record| {
        let kind = record["type"].as_str().unwrap_or("");
        let agent = match kind {
            "session_meta" if record["payload"].is_object() => Some(Agent::Codex),
            "session_start" => Some(Agent::Droid),
            "session" => Some(Agent::Pi),
            _ => None,
        };
        if let Some(agent) = agent {
            found = ArchiveKind::Agent(agent);
            return false;
        }
        if record.get("type").is_none()
            && record["role"].is_string()
            && record["message"].is_object()
        {
            found = ArchiveKind::Agent(Agent::Cursor);
            return false;
        }
        if matches!(kind, "session_meta" | "session_start" | "session")
            || (kind == "message" && record["message"]["role"].is_string())
            || record["role"].is_string()
        {
            return false;
        }
        if matches!(kind, "custom-title" | "ai-title")
            || (matches!(kind, "user" | "assistant")
                && (record["message"].is_object() || record["message"].is_string()))
        {
            found = ArchiveKind::Agent(Agent::Claude);
            return false;
        }
        true
    })?;
    Ok(found)
}

struct Candidate {
    live: bool,
    modified: i128,
    raw: RawSession,
}

impl Candidate {
    fn beats(&self, other: &Self) -> bool {
        match (self.live, self.modified).cmp(&(other.live, other.modified)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => self.raw.source < other.raw.source,
            std::cmp::Ordering::Less => false,
        }
    }
}

fn record_id(record: &Value, line: u64) -> String {
    string(record, "uuid")
        .or_else(|| string(record, "id"))
        .or_else(|| string(&record["payload"], "id"))
        .unwrap_or_else(|| format!("record:{line}"))
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
            names: (agent == Agent::Codex)
                .then(|| Names::new(ctx.roots.codex_session_index.clone(), ctx.now_ms)),
            now_ms: ctx.now_ms,
        }
    }

    fn unavailable(&self, path: &Path, message: &str) -> Diagnostic {
        diagnostic(self.agent, path, "source_unavailable", message)
    }

    fn archived(&self, path: &Path) -> bool {
        self.archives
            .iter()
            .any(|archive| path.starts_with(archive))
    }

    fn enumerate(&self, root: &Path) -> (Vec<PathBuf>, Vec<Diagnostic>) {
        let zst = self.agent == Agent::Codex;
        let mut files = Vec::new();
        let mut diagnostics = Vec::new();
        if walk(root, &mut files, zst).is_err() {
            diagnostics
                .push(self.unavailable(root, "Required session store could not be enumerated"));
        }
        for archive in &self.archives {
            let failed = match fs::symlink_metadata(archive) {
                Ok(metadata) if metadata.is_dir() => walk(archive, &mut files, zst).is_err(),
                Ok(_) => true,
                Err(e) => e.kind() != std::io::ErrorKind::NotFound,
            };
            if failed {
                diagnostics.push(self.unavailable(archive, "Archive could not be enumerated"));
            }
        }
        let mut files: Vec<_> = files.into_iter().map(readable).collect();
        files.sort();
        files.dedup();
        (files, diagnostics)
    }

    fn admitted(&self, path: &Path) -> std::result::Result<bool, Diagnostic> {
        if !self.shared.as_ref().is_some_and(|a| path.starts_with(a)) {
            return Ok(true);
        }
        let zst = compressed(path);
        let owner = self.agent == if zst { Agent::Codex } else { Agent::Claude };
        match archive_kind(path) {
            Ok(ArchiveKind::Agent(agent)) if agent == self.agent => Ok(true),
            Ok(kind) if owner && (zst || matches!(kind, ArchiveKind::Unknown)) => Err(diagnostic(
                self.agent,
                path,
                "archive_format_unknown",
                "Archive transcript format could not be identified",
            )),
            Err(_) if owner => Err(self.unavailable(path, "Archive transcript could not be read")),
            _ => Ok(false),
        }
    }

    fn candidate(
        &self,
        path: &Path,
        marks: Option<&titles::Marks>,
        previous: &DiscoveryCache,
    ) -> std::result::Result<Option<Candidate>, Diagnostic> {
        let unreadable = || self.unavailable(path, "Session metadata could not be read");
        let source = path.to_string_lossy().into_owned();
        let mut dependencies = vec![path.to_path_buf()];
        if self.agent == Agent::Droid {
            dependencies.push(path.with_extension("settings.json"));
        }
        let base = fingerprint(&dependencies).map_err(|_| unreadable())?;
        dependencies.extend(self.names.iter().map(|names| names.path().to_path_buf()));
        let previous: Option<&CachedSource> = previous
            .by_source
            .get(&source)
            .filter(|cached| cached.key.agent == self.agent);
        let signed = |id: &str| marks.map_or_else(|| base.clone(), |m| m.sign(&base, id, previous));
        let cached = previous.filter(|cached| cached.fingerprint == signed(&cached.key.id));
        if cached.is_none() && !self.admitted(path)? {
            return Ok(None);
        }
        let identify = || -> Result<(SessionKey, i128)> {
            let key = match cached {
                Some(cached) => cached.key.clone(),
                None => SessionKey {
                    agent: self.agent,
                    id: header(self.agent, path, &open_regular(path)?)?
                        .id
                        .unwrap_or_else(|| fallback_id(self.agent, path)),
                },
            };
            let m = fs::metadata(path).map_err(|e| io(path, e))?;
            Ok((
                key,
                i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ))
        };
        let (key, modified) = identify().map_err(|_| unreadable())?;
        Ok(Some(Candidate {
            live: !self.archived(path),
            modified,
            raw: RawSession {
                fingerprint: signed(&key.id),
                key,
                source,
                dependencies,
                snapshot: None,
                resume: None,
            },
        }))
    }

    pub fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
        let root = match self.agent {
            Agent::Claude => &ctx.roots.claude_projects,
            Agent::Codex => &ctx.roots.codex_sessions,
            Agent::Droid => &ctx.roots.droid_sessions,
            Agent::Pi => &ctx.roots.pi_sessions,
            _ => return Err(Error::InvalidArgument("not a JSONL agent".into())),
        };
        let (files, mut diagnostics) = self.enumerate(root);
        let marks = self.names.as_ref().map(Names::marks);
        let mut chosen: BTreeMap<String, Candidate> = BTreeMap::new();
        for path in files {
            match self.candidate(&path, marks.as_ref(), previous) {
                Ok(Some(candidate)) => {
                    if chosen
                        .get(&candidate.raw.key.id)
                        .is_none_or(|old| candidate.beats(old))
                    {
                        chosen.insert(candidate.raw.key.id.clone(), candidate);
                    }
                }
                Ok(None) => {}
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        let complete_agents = if diagnostics.is_empty() {
            BTreeSet::from([self.agent])
        } else {
            BTreeSet::new()
        };
        Ok(Discovery {
            sessions: chosen
                .into_values()
                .map(|candidate| candidate.raw)
                .collect(),
            diagnostics,
            advisories: marks.map(|marks| marks.advisories).unwrap_or_default(),
            complete_agents,
        })
    }

    fn others(&self, raw: &RawSession) -> Result<String> {
        let rest: Vec<PathBuf> = Dependencies::of(raw)
            .map_or(&[][..], |dependencies| dependencies.companions)
            .iter()
            .filter(|path| {
                self.names
                    .as_ref()
                    .is_none_or(|names| names.path() != *path)
            })
            .cloned()
            .collect();
        Ok(if rest.is_empty() {
            String::new()
        } else {
            fingerprint(&rest)?.signature
        })
    }

    fn settings(&self, path: &Path) -> Result<(bool, Option<Diagnostic>)> {
        if self.agent != Agent::Droid {
            return Ok((false, None));
        }
        let settings = path.with_extension("settings.json");
        fingerprint(std::slice::from_ref(&settings))?;
        Ok(match File::open(&settings) {
            Ok(file) => match serde_json::from_reader::<_, Value>(BufReader::new(file)) {
                Ok(value) if value.is_object() => (
                    value["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.iter().any(|tag| tag["name"] == "exec")),
                    None,
                ),
                _ => (
                    false,
                    Some(diagnostic(
                        self.agent,
                        &settings,
                        "malformed_metadata",
                        "Settings are not a JSON object",
                    )),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, None),
            Err(_) => (
                false,
                Some(self.unavailable(&settings, "Settings could not be read")),
            ),
        })
    }

    fn session(&self, raw: &RawSession, path: &Path, key: SessionKey, header: Header) -> Session {
        Session {
            key,
            title: None,
            title_origin: None,
            source_updated_at: None,
            cwd: header.cwd,
            branch: header.branch,
            started_at: None,
            updated_at: None,
            parent: header.parent.map(|id| SessionKey {
                agent: self.agent,
                id,
            }),
            source: raw.source.clone(),
            archived: self.archived(path),
        }
    }

    pub fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession> {
        let path = Path::new(&raw.source);
        let resume = raw.resume.as_ref();
        if resume.is_some() && compressed(path) {
            return Err(resume_rejected());
        }
        let file = open_regular(path)?;
        let h = header(self.agent, path, &file)?;
        let key = SessionKey {
            agent: self.agent,
            id: h
                .id
                .clone()
                .unwrap_or_else(|| fallback_id(self.agent, path)),
        };
        let metadata = file.metadata().map_err(|e| io(path, e))?;
        let others = self.others(raw)?;
        if let Some(resume) = resume.map(|resume| &resume.state)
            && ((resume.dev, resume.ino) != (metadata.dev(), metadata.ino())
                || metadata.len() < resume.consumed
                || resume.others != others
                || resume.session.key != key)
        {
            return Err(resume_rejected());
        }
        let automated = h.automated;
        let mut session = resume.map_or_else(
            || self.session(raw, path, key, h),
            |resume| resume.state.session.clone(),
        );
        session.source_updated_at =
            Some(metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000);
        let (exec, settings) = self.settings(path)?;
        let automated = automated || exec;
        let lookup = self
            .names
            .as_ref()
            .map(|names| names.lookup(&session.key.id));
        let mut index = resume.map_or(0, |resume| resume.state.entries);
        let mut checkpoint = None;
        let mut visit = |line: u64, record: &Value, terminated: bool| {
            if !terminated {
                checkpoint = Some((session.clone(), index));
            }
            update_metadata(&mut session, record);
            let base = record_id(record, line);
            for (ordinal, mut item) in normalize(self.agent, record).into_iter().enumerate() {
                super::derive_title(&mut session, &item.role, item.kind, &item.text);
                item.session = session.key.clone();
                item.index = index;
                item.source_id = format!("{base}:{ordinal}");
                item.automated |= automated;
                index += 1;
                emit(item)?;
            }
            Ok(true)
        };
        let recent = recent(&metadata, self.now_ms);
        let (tally, state) = if compressed(path) {
            let tally = scan_file(path, &file, recent, &mut |line, record| {
                visit(line, record, true)
            })?;
            (tally, None)
        } else {
            let scanned = appendable(path, &file, recent, resume, &mut visit)?;
            let (boundary, entries) = checkpoint.unwrap_or_else(|| (session.clone(), index));
            let tally = scanned.tally();
            let state = scanned.into_state(boundary, entries, &metadata, others);
            (tally, Some(SourceState::Append(state)))
        };
        let mut diagnostics: Vec<_> = settings.into_iter().collect();
        let mut recheck = tally.recheck;
        if let Some(lookup) = lookup {
            if let Some(title) = lookup.title {
                session.title = Some(title);
                session.title_origin = Some(TitleOrigin::Native);
            }
            diagnostics.extend(lookup.diagnostics);
            recheck |= lookup.recheck;
        }
        if tally.malformed > 0 {
            diagnostics.push(diagnostic(
                self.agent,
                path,
                "malformed_records",
                &format!(
                    "{} nonempty records were not valid JSON objects",
                    tally.malformed
                ),
            ));
        }
        Ok(ParsedSession {
            session,
            complete: diagnostics.is_empty(),
            diagnostics,
            state,
            recheck,
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

fn tool_result_block(value: &Value) -> bool {
    matches!(
        value["type"].as_str(),
        Some("tool_result" | "tool-result" | "toolResult")
    )
}

fn repeats_tool_result_block(found: &[Entry], text: &str) -> bool {
    found
        .iter()
        .any(|e| e.searchable == Searchable::ToolResult && e.text.trim() == text.trim())
}

fn codex_tool_output(subtype: &str) -> bool {
    matches!(subtype, "function_call_output" | "custom_tool_call_output")
}

fn tool_result_role(role: &str) -> bool {
    matches!(role, "toolResult" | "tool")
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
        searchable: Searchable::Text,
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
        let searchable = if tool_result_block(value) {
            Searchable::ToolResult
        } else {
            Searchable::Text
        };
        if let Some(last) = entries.last_mut().filter(|last| last.kind == kind) {
            last.text.push('\n');
            last.text.push_str(&text);
            last.tool_name = last.tool_name.take().or(name);
            if last.searchable != searchable {
                last.searchable = Searchable::Text;
            }
        } else {
            let mut item = entry(agent, role, kind, text, record);
            item.tool_name = name;
            item.searchable = searchable;
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
                let duplicate = repeats_tool_result_block(&found, &text);
                let mut item = entry(agent, kind, Kind::Tool, text, record);
                item.searchable = if duplicate {
                    Searchable::Duplicate
                } else {
                    Searchable::ToolResult
                };
                found.push(item);
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
                output if codex_tool_output(output) => {
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
            if codex_tool_output(subtype) {
                item.searchable = Searchable::ToolResult;
            }
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
            if tool_result_role(role) {
                for item in &mut found {
                    item.kind = Kind::Tool;
                    item.tool_name = string(message, "toolName");
                    item.searchable = Searchable::ToolResult;
                }
            }
            found
        }
        _ => metadata_entries(agent, record),
    }
}
