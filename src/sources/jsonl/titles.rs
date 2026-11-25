use super::{Dependencies, diagnostic, fingerprint, scan::records};
use crate::{
    Agent, CachedSource, Diagnostic, Error, Fingerprint, RawSession, Result, Timestamp, hex::hex,
};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(test)]
thread_local! {
    pub(crate) static FAILING_TITLE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct Titles {
    names: BTreeMap<String, Option<String>>,
    diagnostics: Vec<Diagnostic>,
    recheck: bool,
}

impl Titles {
    fn unavailable(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.code == "source_unavailable")
    }

    fn mark(&self, id: &str) -> String {
        hashed(
            &self
                .names
                .get(id)
                .cloned()
                .flatten()
                .map_or_else(|| "none".into(), |title| format!("some:{title}")),
        )
    }
}

fn unreadable(path: &Path) -> Diagnostic {
    diagnostic(
        Agent::Codex,
        path,
        "source_unavailable",
        "Session names could not be read; unchanged sessions keep their titles and reparsed sessions may lack titles until the index is readable",
    )
}

fn read_titles(path: &Path, now_ms: Timestamp) -> Titles {
    let mut names = BTreeMap::new();
    let status = records(path, now_ms, &mut |_, record| {
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
        Ok(tally) if tally.malformed == 0 => None,
        Ok(_) => Some(diagnostic(
            Agent::Codex,
            path,
            "malformed_metadata",
            "Session names contain malformed records",
        )),
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(unreadable(path)),
    };
    Titles {
        names,
        diagnostics: diagnostic.into_iter().collect(),
        recheck,
    }
}

pub(crate) fn hashed(title: &str) -> String {
    hex(Sha256::digest(title.as_bytes()))
}

pub(super) struct Names {
    path: PathBuf,
    now_ms: Timestamp,
    cache: RefCell<Option<(Fingerprint, Arc<Titles>)>>,
}

pub(super) struct Lookup {
    pub(super) title: Option<String>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) recheck: bool,
}

pub(super) struct Marks {
    titles: Option<Arc<Titles>>,
    pub(super) advisories: Vec<Diagnostic>,
}

impl Marks {
    pub(super) fn sign(
        &self,
        base: &Fingerprint,
        id: &str,
        previous: Option<&CachedSource>,
    ) -> Fingerprint {
        let mark = self.titles.as_ref().map_or_else(
            || {
                previous
                    .filter(|cached| cached.key.id == id)
                    .and_then(|cached| cached.fingerprint.title_mark.clone())
                    .unwrap_or_else(|| hashed("unavailable"))
            },
            |titles| titles.mark(id),
        );
        Fingerprint {
            signature: base.signature.clone(),
            title_mark: Some(mark),
        }
    }
}

impl Names {
    pub(super) const fn new(path: PathBuf, now_ms: Timestamp) -> Self {
        Self {
            path,
            now_ms,
            cache: RefCell::new(None),
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<Arc<Titles>> {
        #[cfg(test)]
        if FAILING_TITLE_READS.with(|left| left.replace(left.get().saturating_sub(1))) > 0 {
            return Err(Error::InvalidArgument(
                "synthetic title read failure".into(),
            ));
        }
        let names = std::slice::from_ref(&self.path);
        let signature = fingerprint(names)?;
        let cached = self
            .cache
            .borrow()
            .as_ref()
            .filter(|(old, _)| *old == signature)
            .map(|(_, titles)| Arc::clone(titles));
        if let Some(titles) = cached {
            return Ok(titles);
        }
        let titles = Arc::new(read_titles(&self.path, self.now_ms));
        if fingerprint(names)? != signature {
            return Err(Error::InvalidArgument(
                "Session names changed during parsing".into(),
            ));
        }
        if !titles.unavailable() {
            *self.cache.borrow_mut() = Some((signature, Arc::clone(&titles)));
        }
        Ok(titles)
    }

    pub(super) fn marks(&self) -> Marks {
        let read = || self.load().ok().filter(|titles| !titles.unavailable());
        let titles = read().or_else(read);
        Marks {
            advisories: titles.as_ref().map_or_else(
                || vec![unreadable(&self.path)],
                |titles| titles.diagnostics.clone(),
            ),
            titles,
        }
    }

    pub(super) fn lookup(&self, id: &str) -> Lookup {
        self.load().map_or_else(
            |_| Lookup {
                title: None,
                diagnostics: vec![unreadable(&self.path)],
                recheck: true,
            },
            |titles| Lookup {
                title: titles.names.get(id).cloned().flatten(),
                diagnostics: if titles.unavailable() {
                    titles.diagnostics.clone()
                } else {
                    Vec::new()
                },
                recheck: titles.recheck || titles.unavailable(),
            },
        )
    }
}

pub fn drifted(raw: &RawSession) -> bool {
    let current = match raw.fingerprint.title_mark {
        Some(_) => Dependencies::of(raw)
            .map(|dependencies| fingerprint(std::slice::from_ref(dependencies.transcript))),
        None => Some(fingerprint(&raw.dependencies)),
    };
    !matches!(current, Some(Ok(current)) if current.signature == raw.fingerprint.signature)
}

#[cfg(test)]
mod tests {
    use super::super::FileReader;
    use crate::{Agent, DiscoveryCache, testing::context};
    use std::{fs, sync::Arc};

    #[test]
    fn title_cache_reuses_final_map_and_tracks_revisions_and_diagnostics() {
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
        fs::write(names, "{\"id\":\"one\",\"thread_name\":\"earlier\"}\n{\"id\":\"one\",\"thread_name\":\"last\"}\n{\"id\":\"two\",\"thread_name\":\"title\"}\n{\"id\":\"two\",\"thread_name\":\" \"}\nmalformed\n").unwrap();
        let reader = FileReader::new(Agent::Codex, &ctx);
        let cache = &reader.names.as_ref().unwrap().cache;
        let discovery = reader.discover(&ctx, &DiscoveryCache::default()).unwrap();
        let first = reader
            .parse(&discovery.sessions[0], &mut |_| Ok(()))
            .unwrap();
        let cached = Arc::clone(&cache.borrow().as_ref().unwrap().1);
        let second = reader
            .parse(&discovery.sessions[1], &mut |_| Ok(()))
            .unwrap();
        assert!(Arc::ptr_eq(&cached, &cache.borrow().as_ref().unwrap().1));
        assert_eq!(first.session.title.as_deref(), Some("last"));
        assert_eq!(second.session.title, None);
        assert!(first.complete && second.complete);
        assert_eq!(discovery.diagnostics, []);
        assert_eq!(discovery.advisories.len(), 1);
        assert_eq!(discovery.advisories[0].code, "malformed_metadata");
        fs::write(names, "{\"id\":\"one\",\"thread_name\":\"revised\"}\n").unwrap();
        let revised = reader
            .parse(&discovery.sessions[0], &mut |_| Ok(()))
            .unwrap();
        assert_eq!(revised.session.title.as_deref(), Some("revised"));
        assert!(revised.complete);
        assert!(!Arc::ptr_eq(&cached, &cache.borrow().as_ref().unwrap().1));
        fs::remove_file(names).unwrap();
        fs::create_dir(names).unwrap();
        let unavailable = reader
            .parse(&discovery.sessions[0], &mut |_| Ok(()))
            .unwrap();
        assert_eq!(unavailable.session.title, None);
        assert_eq!(unavailable.diagnostics[0].code, "source_unavailable");
        assert!(unavailable.recheck);
        assert!(!revised.recheck);
    }
}
