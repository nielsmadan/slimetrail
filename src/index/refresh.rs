use super::{IndexSession, store};
use crate::{
    Agent, Diagnostic, DiscoveryCache, Envelope, Error, IndexStats, RawSession, Result,
    sources::{self, Source, jsonl::fingerprint},
};
use rusqlite::params;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

fn diagnostic(agent: Agent, source: Option<String>, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        agent: Some(agent),
        source,
        code: code.into(),
        message: message.into(),
    }
}

enum Outcome {
    Stable {
        complete: bool,
        diagnostics: Vec<Diagnostic>,
    },
    Changed,
    Failed,
}

fn ingest(
    index: &mut IndexSession,
    source: &dyn Source,
    raw: &RawSession,
    generation: i64,
) -> Result<Outcome> {
    if fingerprint(&raw.dependencies).ok().as_ref() != Some(&raw.fingerprint) {
        return Ok(Outcome::Changed);
    }
    let transaction = index.connection.savepoint()?;
    store::source(
        &transaction,
        &index.namespace,
        raw,
        "pending",
        &[],
        generation,
    )?;
    transaction.execute(
        "DELETE FROM sessions WHERE namespace=?1 AND agent=?2 AND (id=?3 OR source=?4)",
        params![
            index.namespace,
            raw.key.agent.to_string(),
            raw.key.id,
            raw.source
        ],
    )?;
    let mut write_failed = false;
    let mut expected_index = 0;
    let parsed = source.parse(raw, &mut |entry| {
        if entry.index != expected_index {
            return Err(Error::InvalidArgument(
                "source emitted a non-dense entry index".into(),
            ));
        }
        expected_index += 1;
        let result = store::entry(&transaction, &index.namespace, raw, &entry);
        write_failed |= result.is_err();
        result
    });
    if write_failed {
        return Err(parsed.err().unwrap_or_else(|| {
            Error::InvalidArgument("source swallowed an index write failure".into())
        }));
    }
    if fingerprint(&raw.dependencies).ok().as_ref() != Some(&raw.fingerprint) {
        return Ok(Outcome::Changed);
    }
    let Ok(mut parsed) = parsed else {
        return Ok(Outcome::Failed);
    };
    if parsed.session.key != raw.key || parsed.session.source != raw.source {
        return Ok(Outcome::Failed);
    }
    if !parsed.complete && parsed.diagnostics.is_empty() {
        parsed.diagnostics.push(diagnostic(
            raw.key.agent,
            Some(raw.source.clone()),
            "partial_source",
            "Source parsing was incomplete",
        ));
    }
    let complete = parsed.complete && parsed.diagnostics.is_empty();
    store::session(&transaction, &index.namespace, raw, &parsed.session)?;
    store::source(
        &transaction,
        &index.namespace,
        raw,
        if complete { "fresh" } else { "partial" },
        &parsed.diagnostics,
        generation,
    )?;
    transaction.commit()?;
    Ok(Outcome::Stable {
        complete,
        diagnostics: parsed.diagnostics,
    })
}

pub(super) fn run(
    index: &mut IndexSession,
    agents: &BTreeSet<Agent>,
) -> Result<Envelope<IndexStats>> {
    let readers = sources::registry_in(&index.context, &index.staging);
    run_sources(index, agents, readers)
}

fn run_sources(
    index: &mut IndexSession,
    agents: &BTreeSet<Agent>,
    readers: Vec<Box<dyn Source>>,
) -> Result<Envelope<IndexStats>> {
    let started = Instant::now();
    let mut stats = IndexStats {
        discovered: 0,
        reused: 0,
        reparsed: 0,
        removed: 0,
        failed: 0,
        elapsed_ms: 0,
    };
    let mut diagnostics = Vec::new();
    let selected = readers
        .into_iter()
        .filter(|s| agents.is_empty() || agents.contains(&s.agent()));
    for source in selected {
        let agent = source.agent();
        index.refreshed.remove(&agent);
        index.connection.execute("INSERT INTO refresh_runs(namespace,agent,generation,status,diagnostics) VALUES(?1,?2,1,'incomplete','[]') ON CONFLICT(namespace,agent) DO UPDATE SET generation=generation+1,status='incomplete',diagnostics='[]'", (&index.namespace,agent.to_string()))?;
        let generation: i64 = index.connection.query_row(
            "SELECT generation FROM refresh_runs WHERE namespace=?1 AND agent=?2",
            (&index.namespace, agent.to_string()),
            |r| r.get(0),
        )?;
        let previous = store::cache(&index.connection, &index.namespace, agent)?;
        let discovered = source.discover(&index.context, &previous);
        let mut agent_diagnostics = Vec::new();
        let (sessions, mut enumeration_complete) = match discovered {
            Ok(discovery) => {
                let complete =
                    discovery.complete_agents.contains(&agent) && discovery.diagnostics.is_empty();
                agent_diagnostics.extend(discovery.diagnostics);
                (discovery.sessions, complete)
            }
            Err(_) => {
                agent_diagnostics.push(diagnostic(
                    agent,
                    None,
                    "source_unavailable",
                    "Selected source could not be enumerated",
                ));
                (Vec::new(), false)
            }
        };
        if !enumeration_complete && agent_diagnostics.is_empty() {
            agent_diagnostics.push(diagnostic(
                agent,
                None,
                "incomplete_inventory",
                "Selected source inventory is incomplete",
            ));
        }
        stats.discovered += sessions.len() as u64;
        let mut identities = BTreeSet::new();
        for mut raw in sessions {
            if raw.key.agent != agent || !identities.insert(raw.key.id.clone()) {
                enumeration_complete = false;
                stats.failed += 1;
                agent_diagnostics.push(diagnostic(
                    agent,
                    Some(raw.source),
                    "duplicate_identity",
                    "Source inventory contains an invalid or duplicate identity",
                ));
                continue;
            }
            let reusable = previous.by_source.get(&raw.source).is_some_and(|cached| {
                cached.key == raw.key
                    && cached.dependencies == raw.dependencies
                    && cached.fingerprint == raw.fingerprint
            });
            if reusable && fingerprint(&raw.dependencies).ok().as_ref() == Some(&raw.fingerprint) {
                index.connection.execute("UPDATE sources SET generation=?1 WHERE namespace=?2 AND agent=?3 AND source=?4 AND status='fresh'",params![generation,index.namespace,agent.to_string(),raw.source])?;
                stats.reused += 1;
                continue;
            }
            let mut outcome = ingest(index, source.as_ref(), &raw, generation)?;
            if matches!(outcome, Outcome::Changed) {
                enumeration_complete = false;
                agent_diagnostics.push(diagnostic(
                    agent,
                    Some(raw.source.clone()),
                    "source_changed",
                    "Source changed during refresh; inventory will be reconciled again",
                ));
                let empty = DiscoveryCache {
                    by_source: BTreeMap::new(),
                };
                if let Ok(discovery) = source.discover(&index.context, &empty)
                    && let Some(replacement) = discovery
                        .sessions
                        .into_iter()
                        .find(|s| s.source == raw.source)
                {
                    raw = replacement;
                    outcome = ingest(index, source.as_ref(), &raw, generation)?;
                }
            }
            match outcome {
                Outcome::Stable {
                    complete,
                    diagnostics: found,
                } => {
                    stats.reparsed += 1;
                    if !complete {
                        stats.failed += 1;
                    }
                    agent_diagnostics.extend(found);
                }
                Outcome::Changed | Outcome::Failed => {
                    stats.failed += 1;
                    let failed = diagnostic(
                        agent,
                        Some(raw.source.clone()),
                        "source_unavailable",
                        "Source could not be parsed consistently; previous entries are excluded",
                    );
                    store::source(
                        &index.connection,
                        &index.namespace,
                        &raw,
                        "failed",
                        std::slice::from_ref(&failed),
                        generation,
                    )?;
                    agent_diagnostics.push(failed);
                }
            }
        }
        let transaction = index.connection.transaction()?;
        if enumeration_complete {
            stats.removed += transaction.execute(
                "DELETE FROM sources WHERE namespace=?1 AND agent=?2 AND generation<>?3",
                params![index.namespace, agent.to_string(), generation],
            )? as u64;
        }
        let complete = enumeration_complete && agent_diagnostics.is_empty();
        transaction.execute(
            "UPDATE refresh_runs SET status=?1,diagnostics=?2 WHERE namespace=?3 AND agent=?4",
            params![
                if complete { "complete" } else { "incomplete" },
                store::encode(&agent_diagnostics)?,
                index.namespace,
                agent.to_string()
            ],
        )?;
        transaction.commit()?;
        diagnostics.extend(agent_diagnostics);
        index.refreshed.insert(agent);
    }
    stats.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    Ok(Envelope {
        version: 1,
        incomplete: !diagnostics.is_empty(),
        diagnostics,
        data: Some(stats),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Discovery, Entry, ParsedSession, SessionKey};
    use std::{
        cell::Cell,
        fs,
        path::{Path, PathBuf},
        rc::Rc,
    };

    struct ChangingSource {
        reader: Box<dyn Source>,
        path: PathBuf,
        changes: usize,
        calls: Rc<Cell<usize>>,
        fail: bool,
    }

    impl Source for ChangingSource {
        fn agent(&self) -> Agent {
            Agent::Claude
        }
        fn discover(&self, ctx: &Context, previous: &DiscoveryCache) -> Result<Discovery> {
            self.reader.discover(ctx, previous)
        }
        fn parse(
            &self,
            raw: &RawSession,
            emit: &mut dyn FnMut(Entry) -> Result<()>,
        ) -> Result<ParsedSession> {
            let parsed = self.reader.parse(raw, emit)?;
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() <= self.changes {
                write(&self.path, &format!("revision {}", self.calls.get()));
            }
            if self.fail {
                Err(Error::InvalidArgument("synthetic parse failure".into()))
            } else {
                Ok(parsed)
            }
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::json!({"type":"user","sessionId":"stable","message":text}).to_string()
                + "\n",
        )
        .unwrap();
    }

    fn context(base: &Path) -> Context {
        Context {
            roots: crate::resolve_roots(base, &BTreeMap::new()).unwrap(),
            index_dir: base.join("index"),
            now_ms: 0,
            cwd: base.into(),
            time_zone: "UTC".into(),
        }
    }

    #[test]
    fn changing_source_retries_once_and_excludes_unstable_old_rows() {
        for changes in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = context(dir.path());
            let path = ctx.roots.claude_projects.join("project/source.jsonl");
            write(&path, "original");
            let agents = BTreeSet::from([Agent::Claude]);
            let mut index = IndexSession::open(&ctx).unwrap();
            index.refresh_under_lease(&agents).unwrap();
            write(&path, "before change");
            let calls = Rc::new(Cell::new(0));
            let reader = ChangingSource {
                reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
                path: path.clone(),
                changes,
                calls: Rc::clone(&calls),
                fail: false,
            };
            let result = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
            assert!(result.incomplete);
            assert_eq!(calls.get(), 2);
            let key = SessionKey {
                agent: Agent::Claude,
                id: "stable".into(),
            };
            let mut texts = Vec::new();
            index
                .visit_entries(&key, &mut |e| {
                    texts.push(e.text);
                    Ok(())
                })
                .unwrap();
            if changes == 1 {
                assert_eq!(texts, ["revision 1"]);
            } else {
                assert!(texts.is_empty());
                assert_eq!(
                    index
                        .connection
                        .query_row("SELECT text FROM entries", [], |r| r.get::<_, String>(0))
                        .unwrap(),
                    "original"
                );
            }
            assert!(!index.refresh_under_lease(&agents).unwrap().incomplete);
            index.verify_integrity().unwrap();
        }
    }

    #[test]
    fn parser_failure_rolls_back_streamed_entries_and_excludes_prior_rows() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = ctx.roots.claude_projects.join("project/source.jsonl");
        write(&path, "original");
        let agents = BTreeSet::from([Agent::Claude]);
        let mut index = IndexSession::open(&ctx).unwrap();
        index.refresh_under_lease(&agents).unwrap();
        write(&path, "replacement");
        let reader = ChangingSource {
            reader: sources::source_for(&ctx, Agent::Claude).unwrap(),
            path,
            changes: 0,
            calls: Rc::new(Cell::new(0)),
            fail: true,
        };
        let failed = run_sources(&mut index, &agents, vec![Box::new(reader)]).unwrap();
        assert!(failed.incomplete);
        assert_eq!(failed.data.unwrap().failed, 1);
        assert_eq!(
            index
                .connection
                .query_row("SELECT text FROM entries", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "original"
        );
        assert!(
            index
                .session(&SessionKey {
                    agent: Agent::Claude,
                    id: "stable".into()
                })
                .unwrap()
                .is_none()
        );
        index.verify_integrity().unwrap();
    }
}
