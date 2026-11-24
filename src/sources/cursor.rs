use super::{
    Source,
    jsonl::{ArchiveKind, archive_kind, content_entries, diagnostic, fingerprint, records, string},
    sqlite,
};
use crate::{
    Agent, Context, Diagnostic, Discovery, DiscoveryCache, Entry, Error, Fingerprint,
    ParsedSession, PrivateSnapshot, RawSession, Result, Session, SessionKey, TitleOrigin,
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Cursor {
    staging: Option<PathBuf>,
    archive: Option<PathBuf>,
}

struct CursorDependencies<'a> {
    transcript: &'a Path,
    own_metadata: &'a Path,
    parent_metadata: &'a Path,
    database: &'a Path,
}

impl<'a> CursorDependencies<'a> {
    fn from_paths(paths: &'a [PathBuf]) -> Result<Self> {
        let [transcript, own_metadata, parent_metadata, database, _, _, _] = paths else {
            return Err(Error::InvalidArgument(
                "Cursor source has invalid dependencies".into(),
            ));
        };
        Ok(Self {
            transcript,
            own_metadata,
            parent_metadata,
            database,
        })
    }

    fn paths(&self) -> Vec<PathBuf> {
        [self.transcript, self.own_metadata, self.parent_metadata]
            .into_iter()
            .map(Path::to_path_buf)
            .chain(sqlite::dependencies(self.database))
            .collect()
    }
}

impl Cursor {
    pub fn new(ctx: &Context) -> Self {
        Self {
            staging: None,
            archive: ctx.roots.archive.clone(),
        }
    }

    pub(crate) fn with_staging(ctx: &Context, staging: &Path) -> Self {
        Self {
            staging: Some(staging.into()),
            archive: ctx.roots.archive.clone(),
        }
    }

    fn archived(&self, path: &Path) -> bool {
        self.archive.as_ref().is_some_and(|a| path.starts_with(a))
    }
}

fn walk(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let meta = fs::symlink_metadata(root).map_err(|source| Error::Io {
        path: root.into(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(Error::InvalidArgument(
            "Cursor root is not a regular directory".into(),
        ));
    }
    let mut directories = vec![root.to_owned()];
    while let Some(dir) = directories.pop() {
        for entry in fs::read_dir(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: dir.clone(),
                source,
            })?;
            let ty = entry.file_type().map_err(|source| Error::Io {
                path: entry.path(),
                source,
            })?;
            if ty.is_dir() {
                directories.push(entry.path());
            } else if ty.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(())
}

fn validate_companion(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| Error::InvalidArgument("Cursor companion is outside chat root".into()))?;
    let mut current = root.to_path_buf();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            current.push(component);
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::InvalidArgument(
                    "Cursor companion traverses a symlink".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(Error::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn snapshot_for(
    database: &Path,
    snapshots: &mut BTreeMap<PathBuf, (Fingerprint, Arc<PrivateSnapshot>)>,
    staging: Option<&Path>,
) -> Result<Arc<PrivateSnapshot>> {
    let current = fingerprint(&sqlite::dependencies(database))?;
    if let Some((_, snapshot)) = snapshots
        .get(database)
        .filter(|(captured, _)| *captured == current)
    {
        return Ok(Arc::clone(snapshot));
    }
    let (snapshot, captured) = sqlite::snapshot_at(database, staging)?;
    snapshots.insert(database.to_owned(), (captured, Arc::clone(&snapshot)));
    Ok(snapshot)
}

fn archived_transcript(path: &Path, previous: &DiscoveryCache) -> bool {
    let unchanged = previous
        .by_source
        .get(path.to_string_lossy().as_ref())
        .is_some_and(|c| {
            c.key.agent == Agent::Cursor
                && fingerprint(&c.dependencies).is_ok_and(|f| f == c.fingerprint)
        });
    unchanged || matches!(archive_kind(path), Ok(ArchiveKind::Agent(Agent::Cursor)))
}

fn parent(path: &Path) -> Option<String> {
    (path.parent()?.file_name()? == "subagents")
        .then(|| {
            path.parent()?
                .parent()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        })
        .flatten()
}

fn metadata(path: &Path) -> Result<Value> {
    fingerprint(&[path.into()])?;
    let file = fs::File::open(path).map_err(|source| Error::Io {
        path: path.into(),
        source,
    })?;
    let value: Value = serde_json::from_reader(std::io::BufReader::new(file))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::InvalidArgument(
            "Cursor metadata is not an object".into(),
        ))
    }
}

fn db_metadata(snapshot: &PrivateSnapshot) -> Result<Value> {
    let connection = sqlite::open(snapshot)?;
    let transaction = connection.unchecked_transaction()?;
    let encoded: Option<String> = transaction
        .query_row("SELECT value FROM meta WHERE key='0'", [], |r| r.get(0))
        .optional()?;
    let Some(encoded) = encoded else {
        return Err(Error::InvalidArgument(
            "Cursor store metadata is missing".into(),
        ));
    };
    let bytes = encoded.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(Error::InvalidArgument(
            "Cursor store metadata is malformed".into(),
        ));
    }
    let decoded = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let a = char::from(pair[0]).to_digit(16);
            let b = char::from(pair[1]).to_digit(16);
            a.zip(b)
                .and_then(|(a, b)| u8::try_from(a * 16 + b).ok())
                .ok_or_else(|| Error::InvalidArgument("Cursor store metadata is malformed".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let value: Value = serde_json::from_slice(&decoded)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::InvalidArgument(
            "Cursor store metadata is not an object".into(),
        ))
    }
}

fn prompt_time(text: &str) -> Option<jiff::Timestamp> {
    let (local, offset) = text.trim().strip_suffix(')')?.rsplit_once("(UTC")?;
    let mut local = local.trim();
    if let Some((weekday, rest)) = local.split_once(',')
        && !weekday.chars().any(|c| c.is_ascii_digit())
    {
        local = rest.trim();
    }
    let (date, time) = local.rsplit_once(',')?;
    let offset = if offset.is_empty() { "+0" } else { offset };
    let sign = offset.chars().next()?;
    if !matches!(sign, '+' | '-') {
        return None;
    }
    let (hours, minutes) = offset[1..]
        .split_once(':')
        .unwrap_or_else(|| (&offset[1..], "0"));
    let hours = hours.parse::<u8>().ok()?;
    let minutes = minutes.parse::<u8>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let input = format!(
        "{} {} {sign}{hours:02}{minutes:02}",
        date.trim(),
        time.replace(' ', "")
    );
    jiff::Timestamp::strptime("%b %d, %Y %I:%M%p %z", &input)
        .ok()
        .or_else(|| jiff::Timestamp::strptime("%B %d, %Y %I:%M%p %z", &input).ok())
}

fn clean_text(text: &str) -> (Option<jiff::Timestamp>, String) {
    let mut cleaned = text.to_owned();
    let mut timestamp = None;
    while let Some(start) = cleaned.find("<timestamp>") {
        let Some(end) = cleaned[start..].find("</timestamp>").map(|i| i + start) else {
            break;
        };
        if let Some(parsed) = prompt_time(&cleaned[start + 11..end]) {
            timestamp = timestamp.or(Some(parsed));
            let end = end + 12;
            let end = if cleaned[end..].starts_with('\n') {
                end + 1
            } else {
                end
            };
            cleaned.replace_range(start..end, "");
        } else {
            break;
        }
    }
    for tag in ["<user_query>", "</user_query>"] {
        cleaned = cleaned
            .replace(&format!("\n{tag}\n"), "\n")
            .replace(&format!("{tag}\n"), "\n")
            .replace(&format!("\n{tag}"), "\n")
            .replace(tag, "\n");
    }
    (timestamp, cleaned.trim().to_owned())
}

fn normalize(record: &Value) -> Vec<Entry> {
    let role = record["role"].as_str().unwrap_or("message");
    if !record["message"].is_object() {
        return Vec::new();
    }
    let mut content = record["message"]["content"].clone();
    let mut timestamp = None;
    if role == "user" {
        let blocks = match &mut content {
            Value::Array(blocks) => blocks.as_mut_slice(),
            other => std::slice::from_mut(other),
        };
        for block in blocks {
            let value = if block.is_string() {
                block
            } else if block["type"] == "text" {
                &mut block["text"]
            } else {
                continue;
            };
            if let Some(text) = value.as_str() {
                let (found, cleaned) = clean_text(text);
                timestamp = timestamp.or(found);
                *value = Value::String(cleaned);
            }
        }
    }
    let mut found = content_entries(Agent::Cursor, role, &content, record);
    if let Some(timestamp) = timestamp {
        for entry in &mut found {
            entry.timestamp = Some(timestamp.as_millisecond());
            entry.timestamp_raw = Some(timestamp.to_string());
        }
    }
    found
}

impl Source for Cursor {
    fn agent(&self) -> Agent {
        Agent::Cursor
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered parsing states together"
    )]
    fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
        let mut diagnostics = Vec::new();
        let mut files = Vec::new();
        if walk(&ctx.roots.cursor_projects, &mut files).is_err() {
            diagnostics.push(diagnostic(
                Agent::Cursor,
                &ctx.roots.cursor_projects,
                "source_unavailable",
                "Cursor transcripts could not be enumerated",
            ));
        }
        let mut chats = Vec::new();
        if walk(&ctx.roots.cursor_chats, &mut chats).is_err() {
            diagnostics.push(diagnostic(
                Agent::Cursor,
                &ctx.roots.cursor_chats,
                "metadata_unavailable",
                "Cursor chat metadata could not be enumerated",
            ));
        }
        let metadata: BTreeMap<_, _> = chats
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "meta.json"))
            .filter_map(|p| {
                Some((
                    p.parent()?.file_name()?.to_string_lossy().into_owned(),
                    p.clone(),
                ))
            })
            .collect();
        let mut archived = Vec::new();
        if let Some(archive) = &self.archive
            && !fs::symlink_metadata(archive)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            if walk(archive, &mut archived).is_err() {
                diagnostics.push(diagnostic(
                    Agent::Cursor,
                    archive,
                    "source_unavailable",
                    "Archive could not be enumerated",
                ));
            }
            archived.retain(|p| {
                p.extension().is_some_and(|e| e == "jsonl") && archived_transcript(p, previous)
            });
        }
        let mut snapshots = BTreeMap::<PathBuf, (Fingerprint, Arc<PrivateSnapshot>)>::new();
        let mut sessions = BTreeMap::<String, (bool, i128, RawSession)>::new();
        let live = files
            .into_iter()
            .filter(|p| p.components().any(|c| c.as_os_str() == "agent-transcripts"));
        for path in live.chain(archived) {
            let archived = self.archived(&path);
            if path.extension().is_some_and(|e| e == "txt") {
                diagnostics.push(diagnostic(
                    Agent::Cursor,
                    &path,
                    "unsupported_transcript",
                    "Legacy Cursor text transcripts are unsupported",
                ));
                continue;
            }
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let id = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let (chat_root, own, owner) = match &self.archive {
                Some(archive) if archived => {
                    let own = path.with_extension("").join("meta.json");
                    (archive, own.clone(), own)
                }
                _ => {
                    let own = metadata
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| ctx.roots.cursor_chats.join(&id).join("meta.json"));
                    let owner = parent(&path)
                        .and_then(|id| metadata.get(&id).cloned())
                        .unwrap_or_else(|| own.clone());
                    (&ctx.roots.cursor_chats, own, owner)
                }
            };
            let database = own.with_file_name("store.db");
            let dependencies = CursorDependencies {
                transcript: &path,
                own_metadata: &own,
                parent_metadata: &owner,
                database: &database,
            }
            .paths();
            let source = path.to_string_lossy().into_owned();
            let result: Result<(i128, RawSession)> = (|| {
                for companion in [&own, &owner] {
                    validate_companion(chat_root, companion)?;
                }
                let signature = fingerprint(&dependencies)?;
                let cached = previous
                    .by_source
                    .get(&source)
                    .filter(|c| c.key.agent == Agent::Cursor && c.fingerprint == signature);
                let snapshot = if database.exists() && cached.is_none() {
                    Some(snapshot_for(
                        &database,
                        &mut snapshots,
                        self.staging.as_deref(),
                    )?)
                } else {
                    None
                };
                if fingerprint(&dependencies)? != signature {
                    return Err(Error::InvalidArgument(
                        "Cursor dependencies changed during discovery".into(),
                    ));
                }
                let meta = fs::metadata(&path).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                Ok((
                    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                    RawSession {
                        key: SessionKey {
                            agent: Agent::Cursor,
                            id: id.clone(),
                        },
                        source,
                        dependencies,
                        fingerprint: signature,
                        snapshot,
                    },
                ))
            })();
            match result {
                Ok((time, raw)) => {
                    if !owner.exists() {
                        diagnostics.push(diagnostic(
                            Agent::Cursor,
                            &path,
                            "metadata_unavailable",
                            "Cursor companion metadata is missing",
                        ));
                    }
                    let rank = (!archived, time);
                    if sessions.get(&id).is_none_or(|(old_live, old_time, old)| {
                        rank > (*old_live, *old_time)
                            || rank == (*old_live, *old_time) && raw.source < old.source
                    }) {
                        sessions.insert(id, (!archived, time, raw));
                    }
                }
                Err(_) => diagnostics.push(diagnostic(
                    Agent::Cursor,
                    &path,
                    "source_unavailable",
                    "Cursor dependencies could not be read consistently",
                )),
            }
        }
        let complete_agents = if diagnostics.is_empty() {
            BTreeSet::from([Agent::Cursor])
        } else {
            BTreeSet::new()
        };
        Ok(Discovery {
            sessions: sessions.into_values().map(|(_, _, raw)| raw).collect(),
            diagnostics,
            complete_agents,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered parsing states together"
    )]
    fn parse(
        &self,
        raw: &RawSession,
        emit: &mut dyn FnMut(Entry) -> Result<()>,
    ) -> Result<ParsedSession> {
        let dependencies = CursorDependencies::from_paths(&raw.dependencies)?;
        let path = dependencies.transcript;
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        let own = match metadata(dependencies.own_metadata) {
            Ok(meta) => meta,
            Err(_)
                if dependencies.own_metadata != dependencies.parent_metadata
                    && !dependencies.own_metadata.exists() =>
            {
                Value::Null
            }
            Err(_) => {
                diagnostics.push(diagnostic(
                    Agent::Cursor,
                    path,
                    "metadata_unavailable",
                    "Cursor companion metadata is missing or malformed",
                ));
                Value::Null
            }
        };
        let owner = if dependencies.own_metadata == dependencies.parent_metadata {
            own.clone()
        } else if let Ok(meta) = metadata(dependencies.parent_metadata) {
            meta
        } else {
            diagnostics.push(diagnostic(
                Agent::Cursor,
                path,
                "metadata_unavailable",
                "Cursor parent metadata is missing or malformed",
            ));
            Value::Null
        };
        let db = if dependencies.database.exists() {
            let result = raw.snapshot.as_ref().map_or_else(
                || {
                    sqlite::snapshot_at(dependencies.database, self.staging.as_deref())
                        .and_then(|(snapshot, _)| db_metadata(&snapshot))
                },
                |snapshot| db_metadata(snapshot),
            );
            result.unwrap_or_else(|_| {
                diagnostics.push(diagnostic(
                    Agent::Cursor,
                    path,
                    "metadata_unavailable",
                    "Cursor database metadata could not be read",
                ));
                Value::Null
            })
        } else {
            Value::Null
        };
        let metadata = fs::metadata(path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
        let parent = parent(path).map(|id| SessionKey {
            agent: Agent::Cursor,
            id,
        });
        let automated = parent.is_some() || !db["subagentInfo"].is_null();
        let title = string(&own, "title").or_else(|| string(&db, "name"));
        let mut session = Session {
            key: raw.key.clone(),
            title_origin: title.as_ref().map(|_| TitleOrigin::Native),
            title,
            cwd: string(&owner, "cwd").map(PathBuf::from),
            branch: None,
            started_at: own["createdAtMs"].as_i64(),
            updated_at: own["createdAtMs"].as_i64(),
            source_updated_at: Some(metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000),
            parent,
            source: raw.source.clone(),
            archived: self.archived(path),
        };
        let mut pending = None;
        let mut index = 0;
        let malformed = records(path, &mut |line, record| {
            for (part, mut entry) in normalize(record).into_iter().enumerate() {
                entry.session = session.key.clone();
                entry.index = index;
                entry.source_id = format!("line:{line}:{part}");
                entry.automated |= automated;
                if let Some(ts) = entry.timestamp {
                    session.started_at = Some(session.started_at.map_or(ts, |old| old.min(ts)));
                    session.updated_at = Some(session.updated_at.map_or(ts, |old| old.max(ts)));
                }
                super::derive_title(&mut session, &entry.role, entry.kind, &entry.text);
                if let Some(previous) = pending.replace(entry) {
                    emit(previous)?;
                }
                index += 1;
            }
            Ok(true)
        })?;
        if let Some(mut entry) = pending {
            if entry.timestamp.is_none() {
                entry.timestamp = own["updatedAtMs"].as_i64();
            }
            if let Some(ts) = entry.timestamp {
                session.started_at = Some(session.started_at.map_or(ts, |old| old.min(ts)));
                session.updated_at = Some(session.updated_at.map_or(ts, |old| old.max(ts)));
            }
            emit(entry)?;
        }
        if malformed > 0 {
            diagnostics.push(diagnostic(
                Agent::Cursor,
                path,
                "malformed_records",
                "Cursor transcript contains malformed records",
            ));
        }
        Ok(ParsedSession {
            session,
            complete: diagnostics.is_empty(),
            diagnostics,
            state: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_snapshot_tracks_database_revision() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("store.db");
        let writer = rusqlite::Connection::open(&database).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES('0', '7b226e616d65223a226669727374227d');").unwrap();
        let mut snapshots = BTreeMap::new();
        let first = snapshot_for(&database, &mut snapshots, None).unwrap();
        assert_eq!(db_metadata(&first).unwrap()["name"], "first");
        writer
            .execute(
                "UPDATE meta SET value='7b226e616d65223a226c61746572227d'",
                [],
            )
            .unwrap();
        let later = snapshot_for(&database, &mut snapshots, None).unwrap();
        assert_eq!(db_metadata(&later).unwrap()["name"], "later");
        assert_eq!(
            snapshots[&database].0,
            fingerprint(&sqlite::dependencies(&database)).unwrap()
        );
        assert!(!Arc::ptr_eq(&first, &later));
    }
}
