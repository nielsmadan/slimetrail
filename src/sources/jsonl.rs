use crate::{
    Agent, AppendState, Context, Diagnostic, Discovery, DiscoveryCache, Entry, Error, Fingerprint,
    Kind, ParsedSession, RawSession, Result, Searchable, Session, SessionKey, SourceState,
    TitleOrigin, hex::hex,
};
use ruzstd::decoding::{
    BlockDecodingStrategy, FrameDecoder,
    errors::{FrameDecoderError, ReadFrameHeaderError},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
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
    recheck: bool,
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
    let recheck = status.as_ref().is_ok_and(|tally| tally.recheck);
    let diagnostic = match status {
        Ok(Tally { malformed: 0, .. }) => None,
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
        recheck,
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

const TITLE_MARK: &str = "\ntitle:";

pub fn drifted(raw: &RawSession) -> bool {
    match raw.fingerprint.signature.split_once(TITLE_MARK) {
        Some((base, _)) => {
            fingerprint(&raw.dependencies[..1]).map_or(true, |c| c.signature != base)
        }
        None => fingerprint(&raw.dependencies).map_or(true, |c| c != raw.fingerprint),
    }
}

pub fn diagnostic(agent: Agent, path: &Path, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        agent: Some(agent),
        source: Some(path.to_string_lossy().into_owned()),
        code: code.into(),
        message: message.into(),
    }
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

const RECENT: Duration = Duration::from_secs(600);

struct Scan {
    consumed: u64,
    lines: u64,
    malformed: u64,
    hasher: Option<Sha256>,
    recent: bool,
    torn: bool,
    recheck: bool,
}

pub struct Tally {
    pub malformed: u64,
    pub recheck: bool,
}

impl Scan {
    const fn new(recent: bool, hasher: Option<Sha256>) -> Self {
        Self {
            consumed: 0,
            lines: 0,
            malformed: 0,
            hasher,
            recent,
            torn: false,
            recheck: false,
        }
    }

    const fn malformed(&self) -> u64 {
        self.malformed + self.torn as u64
    }
}

fn recent(metadata: &fs::Metadata) -> bool {
    metadata.modified().is_ok_and(|modified| {
        SystemTime::now()
            .duration_since(modified)
            .map_or(true, |age| age < RECENT)
    })
}

fn scan(
    reader: &mut dyn BufRead,
    path: &Path,
    state: &mut Scan,
    visit: &mut dyn FnMut(u64, &Value, bool) -> Result<bool>,
) -> Result<()> {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| io(path, e))?
            == 0
        {
            break;
        }
        let line = state.lines + 1;
        let terminated = bytes.last() == Some(&b'\n');
        if terminated {
            state.lines = line;
            state.consumed += bytes.len() as u64;
            if let Some(hasher) = &mut state.hasher {
                hasher.update(&bytes);
            }
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => {
                if !visit(line, &value, terminated)? {
                    break;
                }
            }
            _ if terminated => state.malformed += 1,
            _ if state.recent => state.recheck = true,
            _ => state.torn = true,
        }
    }
    Ok(())
}

fn open_regular(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if !metadata.is_file() {
        return Err(Error::InvalidArgument(
            "transcript must be a regular file".into(),
        ));
    }
    File::open(path).map_err(|e| io(path, e))
}

pub fn records(path: &Path, visit: &mut dyn FnMut(u64, &Value) -> Result<bool>) -> Result<Tally> {
    let file = open_regular(path)?;
    let zst = compressed(path);
    let mut state = Scan::new(
        !zst && recent(&file.metadata().map_err(|e| io(path, e))?),
        None,
    );
    let file = BufReader::new(file);
    let mut reader: Box<dyn BufRead> = if zst {
        Box::new(BufReader::new(Frames {
            source: file,
            decoder: FrameDecoder::new(),
            active: false,
        }))
    } else {
        Box::new(file)
    };
    scan(&mut *reader, path, &mut state, &mut |line, record, _| {
        visit(line, record)
    })?;
    Ok(Tally {
        malformed: state.malformed(),
        recheck: state.recheck,
    })
}

fn hash_prefix(
    reader: &mut dyn BufRead,
    path: &Path,
    hasher: &mut Sha256,
    length: u64,
) -> Result<bool> {
    let mut remaining = length;
    while remaining > 0 {
        let buffer = reader.fill_buf().map_err(|e| io(path, e))?;
        if buffer.is_empty() {
            return Ok(false);
        }
        let take = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        hasher.update(&buffer[..take]);
        reader.consume(take);
        remaining -= take as u64;
    }
    Ok(true)
}

struct Frames<R> {
    source: R,
    decoder: FrameDecoder,
    active: bool,
}

impl<R: BufRead> Read for Frames<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.active {
                while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                    self.decoder
                        .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                        .map_err(std::io::Error::other)?;
                }
                let read = self.decoder.read(buf)?;
                if read > 0 || buf.is_empty() {
                    return Ok(read);
                }
                self.active = false;
            }
            if self.source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => self.active = true,
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let skipped = std::io::copy(
                        &mut (&mut self.source).take(length.into()),
                        &mut std::io::sink(),
                    )?;
                    if skipped < u64::from(length) {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                }
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }
    }
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

fn finish_header(agent: Agent, path: &Path, mut header: Header) -> Header {
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
    header
}

fn header(agent: Agent, path: &Path) -> Result<Header> {
    let mut header = Header::default();
    records(path, &mut |_, record| {
        Ok(header_record(agent, path, &mut header, record))
    })?;
    Ok(finish_header(agent, path, header))
}

fn header_in(agent: Agent, path: &Path, file: &File) -> Result<Header> {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(0)).map_err(|e| io(path, e))?;
    let mut header = Header::default();
    scan(
        &mut reader,
        path,
        &mut Scan::new(true, None),
        &mut |_, record, _| Ok(header_record(agent, path, &mut header, record)),
    )?;
    Ok(finish_header(agent, path, header))
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
        let zst = self.agent == Agent::Codex;
        if walk(root, &mut files, zst).is_err() {
            diagnostics.push(diagnostic(
                self.agent,
                root,
                "source_unavailable",
                "Required session store could not be enumerated",
            ));
        }
        for archive in &self.archives {
            if fs::symlink_metadata(archive).is_ok_and(|m| m.is_dir()) {
                if walk(archive, &mut files, zst).is_err() {
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
        let mut files: Vec<_> = files.into_iter().map(readable).collect();
        files.sort();
        files.dedup();
        let titles = self.names.as_deref().map(|names| {
            let read = || {
                self.titles(names)
                    .ok()
                    .filter(|titles| !unavailable(titles))
            };
            read().or_else(read)
        });
        let title_diagnostics = match (&self.names, &titles) {
            (_, Some(Some(titles))) => titles.diagnostics.clone(),
            (Some(names), Some(None)) => vec![diagnostic(
                self.agent,
                names,
                "source_unavailable",
                "Session names could not be read; unchanged sessions keep their titles and reparsed sessions may lack titles until the index is readable",
            )],
            _ => Vec::new(),
        };
        let mut chosen: BTreeMap<String, (bool, i128, RawSession)> = BTreeMap::new();
        for path in files {
            let source = path.to_string_lossy().into_owned();
            let mut dependencies = vec![path.clone()];
            if self.agent == Agent::Droid {
                dependencies.push(path.with_extension("settings.json"));
            }
            let base = fingerprint(&dependencies);
            dependencies.extend(self.names.iter().cloned());
            let Ok(base) = base else {
                diagnostics.push(diagnostic(
                    self.agent,
                    &path,
                    "source_unavailable",
                    "Session metadata could not be read",
                ));
                continue;
            };
            let previous_title = previous.by_source.get(&source);
            let signed = |id: &str| {
                let mark = titles.as_ref().map(|titles| {
                    titles.as_ref().map_or_else(
                        || {
                            previous_title
                                .filter(|c| c.key.agent == self.agent && c.key.id == id)
                                .and_then(|c| c.fingerprint.signature.split_once(TITLE_MARK))
                                .map_or_else(|| hashed("unavailable"), |(_, mark)| mark.to_owned())
                        },
                        |titles| title_mark(titles, id),
                    )
                });
                with_title(&base, mark.as_deref())
            };
            let cached = previous
                .by_source
                .get(&source)
                .filter(|c| c.key.agent == self.agent && c.fingerprint == signed(&c.key.id));
            if self.shared.as_ref().is_some_and(|a| path.starts_with(a)) && cached.is_none() {
                let zst = compressed(&path);
                let owner = self.agent == if zst { Agent::Codex } else { Agent::Claude };
                match archive_kind(&path) {
                    Ok(ArchiveKind::Agent(agent)) if agent == self.agent => {}
                    Ok(kind) if owner && (zst || matches!(kind, ArchiveKind::Unknown)) => {
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
                        fingerprint: signed(&key.id),
                        key,
                        source: source.clone(),
                        dependencies,
                        snapshot: None,
                        resume: None,
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
            warnings: title_diagnostics,
            complete_agents,
        })
    }

    fn others(&self, raw: &RawSession) -> Result<String> {
        let rest: Vec<PathBuf> = raw
            .dependencies
            .iter()
            .skip(1)
            .filter(|path| self.names.as_ref() != Some(*path))
            .cloned()
            .collect();
        Ok(if rest.is_empty() {
            String::new()
        } else {
            fingerprint(&rest)?.signature
        })
    }

    fn settings(
        &self,
        path: &Path,
        automated: &mut bool,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<()> {
        let settings = path.with_extension("settings.json");
        fingerprint(std::slice::from_ref(&settings))?;
        match File::open(&settings) {
            Ok(file) => match serde_json::from_reader::<_, Value>(BufReader::new(file)) {
                Ok(value) if value.is_object() => {
                    *automated |= value["tags"]
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
        Ok(())
    }

    fn title(
        &self,
        id: &str,
        diagnostics: &mut Vec<Diagnostic>,
        recheck: &mut bool,
    ) -> Option<String> {
        let names = self.names.as_ref()?;
        let Ok(titles) = self.titles(names) else {
            diagnostics.push(diagnostic(
                self.agent,
                names,
                "source_unavailable",
                "Session names could not be read",
            ));
            return None;
        };
        if unavailable(&titles) {
            diagnostics.extend(titles.diagnostics.iter().cloned());
        }
        *recheck |= titles.recheck;
        titles.names.get(id).cloned().flatten()
    }

    fn titles(&self, names: &Path) -> Result<Arc<Titles>> {
        #[cfg(test)]
        if tests::FAILING_TITLE_READS.with(|left| left.replace(left.get().saturating_sub(1))) > 0 {
            return Err(Error::InvalidArgument(
                "synthetic title read failure".into(),
            ));
        }
        let names = [names.to_path_buf()];
        let signature = fingerprint(&names)?;
        let cached = self
            .titles
            .borrow()
            .as_ref()
            .filter(|(old, _)| *old == signature)
            .map(|(_, titles)| Arc::clone(titles));
        if let Some(titles) = cached {
            return Ok(titles);
        }
        let titles = Arc::new(read_titles(&names[0]));
        if fingerprint(&names)? != signature {
            return Err(Error::InvalidArgument(
                "Session names changed during parsing".into(),
            ));
        }
        if !unavailable(&titles) {
            *self.titles.borrow_mut() = Some((signature, Arc::clone(&titles)));
        }
        Ok(titles)
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
        let resume = raw.resume.as_ref();
        let file = if compressed(path) {
            if resume.is_some() {
                return Err(Error::ResumeRejected);
            }
            None
        } else {
            Some(open_regular(path)?)
        };
        let h = match &file {
            Some(file) => header_in(self.agent, path, file)?,
            None => header(self.agent, path)?,
        };
        let key = SessionKey {
            agent: self.agent,
            id: h.id.unwrap_or_else(|| fallback_id(self.agent, path)),
        };
        let metadata = file
            .as_ref()
            .map_or_else(|| fs::metadata(path), File::metadata)
            .map_err(|e| io(path, e))?;
        let others = self.others(raw)?;
        if let Some(resume) = resume
            && ((resume.dev, resume.ino) != (metadata.dev(), metadata.ino())
                || metadata.len() < resume.consumed
                || resume.others != others
                || resume.session.key != key)
        {
            return Err(Error::ResumeRejected);
        }
        let mut session = resume.map_or_else(
            || Session {
                key,
                title: None,
                title_origin: None,
                source_updated_at: None,
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
            },
            |resume| resume.session.clone(),
        );
        session.source_updated_at =
            Some(metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000);
        let mut automated = h.automated;
        let mut diagnostics = Vec::new();
        if self.agent == Agent::Droid {
            self.settings(path, &mut automated, &mut diagnostics)?;
        }
        let mut recheck = false;
        let title = self.title(&session.key.id, &mut diagnostics, &mut recheck);
        let mut index = resume.map_or(0, |resume| resume.entries);
        let mut checkpoint = None;
        let mut visit = |line: u64, record: &Value, terminated: bool| {
            if !terminated {
                checkpoint = Some((session.clone(), index));
            }
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
        };
        let (malformed, state) = if let Some(file) = &file {
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(0)).map_err(|e| io(path, e))?;
            let mut hasher = Sha256::new();
            let mut scanned = Scan::new(recent(&metadata), None);
            if let Some(resume) = resume {
                if !hash_prefix(&mut reader, path, &mut hasher, resume.consumed)?
                    || hex(hasher.clone().finalize()) != resume.prefix_sha256
                {
                    return Err(Error::ResumeRejected);
                }
                scanned.consumed = resume.consumed;
                scanned.lines = resume.lines;
                scanned.malformed = resume.malformed;
            }
            scanned.hasher = Some(hasher);
            scan(&mut reader, path, &mut scanned, &mut visit)?;
            let (boundary, entries) = checkpoint.unwrap_or_else(|| (session.clone(), index));
            let state = AppendState {
                consumed: scanned.consumed,
                prefix_sha256: hex(scanned.hasher.take().unwrap_or_default().finalize()),
                lines: scanned.lines,
                entries,
                malformed: scanned.malformed,
                dev: metadata.dev(),
                ino: metadata.ino(),
                others,
                session: boundary,
            };
            recheck |= scanned.recheck;
            (scanned.malformed(), Some(SourceState::Append(state)))
        } else {
            let tally = records(path, &mut |line, record| visit(line, record, true))?;
            recheck |= tally.recheck;
            (tally.malformed, None)
        };
        if let Some(title) = title {
            session.title = Some(title);
            session.title_origin = Some(TitleOrigin::Native);
        }
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
            state,
            recheck,
        })
    }
}

fn unavailable(titles: &Titles) -> bool {
    titles
        .diagnostics
        .iter()
        .any(|d| d.code == "source_unavailable")
}

fn hashed(title: &str) -> String {
    hex(Sha256::digest(title.as_bytes()))
}

fn title_mark(titles: &Titles, id: &str) -> String {
    hashed(
        &titles
            .names
            .get(id)
            .cloned()
            .flatten()
            .map_or_else(|| "none".into(), |title| format!("some:{title}")),
    )
}

fn with_title(base: &Fingerprint, mark: Option<&str>) -> Fingerprint {
    mark.map_or_else(
        || base.clone(),
        |mark| Fingerprint {
            signature: format!("{}{TITLE_MARK}{mark}", base.signature),
        },
    )
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
        let searchable = if matches!(
            value["type"].as_str(),
            Some("tool_result" | "tool-result" | "toolResult")
        ) {
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
                let duplicate = found.iter().any(|e| {
                    e.searchable == Searchable::ToolResult && e.text.trim() == text.trim()
                });
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
            if matches!(subtype, "function_call_output" | "custom_tool_call_output") {
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
            if matches!(role, "toolResult" | "tool") {
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

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static FAILING_TITLE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn context(dir: &Path) -> Context {
        Context {
            roots: crate::resolve_roots(dir, &BTreeMap::new()).unwrap(),
            index_dir: dir.join("index"),
            cwd: dir.into(),
            now_ms: 0,
            time_zone: "UTC".into(),
            coverage: crate::Coverage::Compact,
            build: crate::BuildPolicy::Inline,
            progress: false,
        }
    }

    #[test]
    fn title_read_failures_keep_each_sessions_cached_title() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
        for id in ["one", "two"] {
            fs::write(
                ctx.roots.codex_sessions.join(format!("{id}.jsonl")),
                format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n"),
            )
            .unwrap();
        }
        let names = &ctx.roots.codex_session_index;
        fs::write(names, "{\"id\":\"one\",\"thread_name\":\"Named\"}\n").unwrap();
        let agents = BTreeSet::from([Agent::Codex]);
        let refresh = || crate::index::refresh(&ctx, &agents, false).unwrap();
        let counts = |result: &crate::Envelope<crate::IndexStats>| {
            let stats = result.data.as_ref().unwrap();
            (stats.reused, stats.reparsed)
        };
        let codes = |result: &crate::Envelope<crate::IndexStats>| -> Vec<String> {
            result.diagnostics.iter().map(|d| d.code.clone()).collect()
        };
        assert_eq!(counts(&refresh()), (0, 2));
        let empty = DiscoveryCache {
            by_source: BTreeMap::new(),
            verify: false,
        };
        let reader = FileReader::new(Agent::Codex, &ctx);
        let readable = DiscoveryCache {
            by_source: reader
                .discover(&ctx, &empty)
                .unwrap()
                .sessions
                .into_iter()
                .map(|raw| {
                    let cached = crate::CachedSource {
                        key: raw.key,
                        source: raw.source.clone(),
                        dependencies: raw.dependencies,
                        fingerprint: raw.fingerprint,
                        state: None,
                    };
                    (raw.source, cached)
                })
                .collect(),
            verify: false,
        };
        FAILING_TITLE_READS.set(1);
        let retried = refresh();
        assert_eq!(FAILING_TITLE_READS.get(), 0);
        assert!(!retried.incomplete, "{:?}", retried.diagnostics);
        assert_eq!(counts(&retried), (2, 0));
        fs::remove_file(names).unwrap();
        fs::create_dir(names).unwrap();
        let mut index = crate::index::IndexSession::open(&ctx).unwrap();
        let failing = index.refresh_under_lease(&agents).unwrap();
        assert!(failing.incomplete);
        assert_eq!(codes(&failing), ["source_unavailable"]);
        assert_eq!(failing.diagnostics[0].source.as_deref(), names.to_str());
        assert_eq!(counts(&failing), (2, 0));
        let one = SessionKey {
            agent: Agent::Codex,
            id: "one".into(),
        };
        assert_eq!(
            index.session(&one).unwrap().unwrap().title.as_deref(),
            Some("Named")
        );
        let three = ctx.roots.codex_sessions.join("three.jsonl");
        fs::write(
            &three,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"three\"}}\n",
        )
        .unwrap();
        let outage = FileReader::new(Agent::Codex, &ctx)
            .discover(&ctx, &readable)
            .unwrap();
        fs::remove_file(&three).unwrap();
        assert_eq!(outage.sessions.len(), 3);
        for raw in &outage.sessions {
            let mark = raw.fingerprint.signature.split_once(TITLE_MARK).unwrap().1;
            match readable.by_source.get(&raw.source) {
                Some(cached) => assert_eq!(cached.fingerprint, raw.fingerprint),
                None => assert_eq!(
                    (raw.key.id.as_str(), mark),
                    ("three", hashed("unavailable").as_str())
                ),
            }
        }
        drop(index);
        fs::remove_dir(names).unwrap();
        fs::write(names, "{\"id\":\"one\",\"thread_name\":\"Named\"}\n").unwrap();
        let recovered = refresh();
        assert!(!recovered.incomplete, "{:?}", recovered.diagnostics);
        assert_eq!(counts(&recovered), (2, 0));
    }

    fn write_codex(ctx: &Context, id: &str, lines: &[&str]) -> PathBuf {
        let path = ctx.roots.codex_sessions.join(format!("{id}.jsonl"));
        let meta = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}");
        let text: String = std::iter::once(meta.as_str())
            .chain(lines.iter().copied())
            .flat_map(|line| [line, "\n"])
            .collect();
        fs::create_dir_all(&ctx.roots.codex_sessions).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn codes(result: &crate::Envelope<crate::IndexStats>) -> Vec<&str> {
        result.diagnostics.iter().map(|d| d.code.as_str()).collect()
    }

    fn key(id: &str) -> SessionKey {
        SessionKey {
            agent: Agent::Codex,
            id: id.into(),
        }
    }

    #[test]
    fn malformed_title_lines_do_not_keep_deleted_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        write_codex(&ctx, "one", &[]);
        let two = write_codex(&ctx, "two", &[]);
        let names = &ctx.roots.codex_session_index;
        fs::write(
            names,
            "{\"id\":\"one\",\"thread_name\":\"Named\"}\nmalformed\n",
        )
        .unwrap();
        File::options()
            .write(true)
            .open(names)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
        let agents = BTreeSet::from([Agent::Codex]);
        let first = crate::index::refresh(&ctx, &agents, false).unwrap();
        assert_eq!(codes(&first), ["malformed_metadata"]);
        fs::remove_file(two).unwrap();
        let mut index = crate::index::IndexSession::open(&ctx).unwrap();
        let removed = index.refresh_under_lease(&agents).unwrap();
        assert!(removed.incomplete);
        assert_eq!(codes(&removed), ["malformed_metadata"]);
        assert_eq!(removed.data.as_ref().unwrap().removed, 1);
        assert_eq!(index.session(&key("two")).unwrap(), None);
        assert_eq!(
            index
                .session(&key("one"))
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("Named")
        );
    }

    #[test]
    fn rollouts_appended_during_a_title_outage_are_indexed_as_partial() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let reply = |text: &str| {
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":text}}).to_string()
        };
        let first = reply("first message");
        write_codex(&ctx, "one", &[&first]);
        fs::write(
            &ctx.roots.codex_session_index,
            "{\"id\":\"one\",\"thread_name\":\"Named\"}\n",
        )
        .unwrap();
        let agents = BTreeSet::from([Agent::Codex]);
        crate::index::refresh(&ctx, &agents, false).unwrap();
        FAILING_TITLE_READS.set(usize::MAX);
        let appended = reply("outage needle");
        write_codex(&ctx, "one", &[&first, &appended]);
        let status = || -> String {
            rusqlite::Connection::open(ctx.index_dir.join(crate::index::DATABASE))
                .unwrap()
                .query_row("SELECT status FROM sources WHERE id='one'", [], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        let mut index = crate::index::IndexSession::open(&ctx).unwrap();
        let outage = index.refresh_under_lease(&agents).unwrap();
        assert!(outage.incomplete);
        assert_eq!(codes(&outage), ["source_unavailable", "source_unavailable"]);
        let counts = outage.data.as_ref().unwrap();
        assert_eq!((counts.reparsed, counts.failed), (1, 1));
        let session = index.session(&key("one")).unwrap().unwrap();
        assert_ne!(session.title_origin, Some(TitleOrigin::Native));
        let mut found = 0;
        index
            .visit_search_documents(Some(("outage needle", false)), &mut |doc| {
                found += usize::from(doc.text.contains("outage needle"));
                Ok(())
            })
            .unwrap();
        assert_eq!(found, 1);
        drop(index);
        assert_eq!(status(), "partial");
        FAILING_TITLE_READS.set(0);
        let mut index = crate::index::IndexSession::open(&ctx).unwrap();
        let recovered = index.refresh_under_lease(&agents).unwrap();
        assert!(!recovered.incomplete, "{:?}", recovered.diagnostics);
        assert_eq!(
            index
                .session(&key("one"))
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("Named")
        );
        drop(index);
        assert_eq!(status(), "fresh");
    }

    #[test]
    fn title_cache_reuses_final_map_and_tracks_revisions_and_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            roots: crate::resolve_roots(dir.path(), &BTreeMap::new()).unwrap(),
            index_dir: dir.path().join("index"),
            cwd: dir.path().into(),
            now_ms: 0,
            time_zone: "UTC".into(),
            coverage: crate::Coverage::Compact,
            build: crate::BuildPolicy::Inline,
            progress: false,
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
                    verify: false,
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
        assert!(first.complete && second.complete);
        assert_eq!(discovery.diagnostics, []);
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(discovery.warnings[0].code, "malformed_metadata");
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
