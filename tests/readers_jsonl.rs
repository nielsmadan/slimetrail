use serde_json::{Value, json};
use slimetrail::{
    sources::{Source, jsonl::fingerprint, source_for},
    *,
};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

fn context(base: &Path) -> Context {
    Context {
        roots: resolve_roots(
            base,
            &BTreeMap::from([
                (
                    "CODEX_HOME".into(),
                    base.join("codex").to_string_lossy().into(),
                ),
                (
                    "XDG_DATA_HOME".into(),
                    base.join("data").to_string_lossy().into(),
                ),
            ]),
        )
        .unwrap(),
        index_dir: base.join("index"),
        now_ms: 0,
        cwd: "/fixture/project".into(),
        time_zone: "UTC".into(),
    }
}
fn root(ctx: &Context, agent: Agent) -> &Path {
    match agent {
        Agent::Claude => &ctx.roots.claude_projects,
        Agent::Codex => &ctx.roots.codex_sessions,
        Agent::Droid => &ctx.roots.droid_sessions,
        Agent::Pi => &ctx.roots.pi_sessions,
        _ => panic!("file source"),
    }
}
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn fixture(agent: Agent) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/jsonl/{agent}.jsonl")),
    )
    .unwrap()
}
fn discover(source: &dyn Source, ctx: &Context) -> Discovery {
    source
        .discover(
            ctx,
            &DiscoveryCache {
                by_source: BTreeMap::new(),
                verify: false,
            },
        )
        .unwrap()
}
fn parse(source: &dyn Source, raw: &RawSession) -> (ParsedSession, Vec<Entry>) {
    let mut entries = Vec::new();
    let parsed = source
        .parse(raw, &mut |entry| {
            entries.push(entry);
            Ok(())
        })
        .unwrap();
    (parsed, entries)
}

#[test]
fn checked_in_reference_goldens_match_four_sources() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for agent in [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Pi] {
        write(
            &root(&ctx, agent).join("project/mismatching-filename.jsonl"),
            &fixture(agent),
        );
        let source = source_for(&ctx, agent).unwrap();
        let discovery = discover(source.as_ref(), &ctx);
        assert!(
            discovery.diagnostics.is_empty(),
            "{:?}",
            discovery.diagnostics
        );
        assert_eq!(discovery.sessions.len(), 1);
        let raw = &discovery.sessions[0];
        assert_eq!(raw.key.id, format!("{agent}-canonical"));
        let (parsed, entries) = parse(source.as_ref(), raw);
        assert!(parsed.complete);
        assert_eq!(parsed.session.key, raw.key);
        assert!(
            entries
                .iter()
                .all(|entry| entry.session == parsed.session.key)
        );
        let actual:Vec<Value>=entries.iter().map(|entry|json!({"role":entry.role,"kind":entry.kind,"text":entry.text,"timestamp":entry.timestamp_raw})).collect();
        let expected: Vec<Value> = serde_json::from_str(
            &fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("tests/fixtures/jsonl/{agent}.golden.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected, "{agent}");
        assert_eq!(parsed.session.cwd, Some("/fixture/project".into()));
        assert_eq!(entries[0].timestamp, Some(1_786_356_180_000));
        assert_eq!(
            parsed.session.title_origin,
            Some(if agent == Agent::Codex {
                TitleOrigin::Derived
            } else {
                TitleOrigin::Native
            })
        );
        if agent == Agent::Codex {
            assert_eq!(parsed.session.parent.unwrap().id, "root-thread");
            assert!(entries.iter().all(|e| e.automated));
            assert_eq!(entries[3].subtype.as_deref(), Some("reasoning"));
        }
    }
}

#[test]
fn malformed_and_truncated_records_preserve_good_entries_and_redact_content() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for agent in [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Pi] {
        let path = root(&ctx, agent).join("session.jsonl");
        write(
            &path,
            &format!(
                "secret malformed\n{}\n[1]\n{{\"secret unfinished\"",
                fixture(agent)
            ),
        );
        let source = source_for(&ctx, agent).unwrap();
        let discovery = discover(source.as_ref(), &ctx);
        let (parsed, entries) = parse(source.as_ref(), &discovery.sessions[0]);
        assert_ne!(entries, [] as [slimetrail::Entry; 0]);
        assert!(!parsed.complete);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].message,
            "3 nonempty records were not valid JSON objects"
        );
        assert_eq!(entries[0].session.id, format!("{agent}-canonical"));
    }
}

#[test]
fn ids_indices_and_metadata_stay_stable_across_rereads_and_append() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for agent in [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Pi] {
        let path = root(&ctx, agent).join("session.jsonl");
        write(&path, &fixture(agent));
        let source = source_for(&ctx, agent).unwrap();
        let discovery = discover(source.as_ref(), &ctx);
        let raw = &discovery.sessions[0];
        let (first, entries) = parse(source.as_ref(), raw);
        let expected_ids: &[&str] = match agent {
            Agent::Claude => &["u1:0", "u1:1", "record:2:0", "record:4:0"],
            Agent::Codex => &[
                "record:2:0",
                "record:3:0",
                "record:4:0",
                "record:5:0",
                "record:6:0",
            ],
            Agent::Droid => &["m1:0", "m1:1", "record:3:0"],
            Agent::Pi => &["p1:0", "p1:1", "record:3:0"],
            _ => unreachable!(),
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.source_id.as_str())
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(
            entries.iter().map(|entry| entry.index).collect::<Vec<_>>(),
            (0..entries.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(parse(source.as_ref(), raw), (first, entries.clone()));
        let appended = match agent {
            Agent::Claude => json!({"type":"user","message":"New"}),
            Agent::Codex => {
                json!({"type":"response_item","payload":{"id":"native-codex","type":"message","role":"user","content":"New"}})
            }
            _ => json!({"type":"message","message":{"role":"user","content":"New"}}),
        };
        writeln!(
            OpenOptions::new().append(true).open(&path).unwrap(),
            "{appended}"
        )
        .unwrap();
        let (_, after) = parse(source.as_ref(), raw);
        assert_eq!(&after[..entries.len()], entries);
        assert_eq!(after.last().unwrap().index, entries.len() as u64);
        assert_eq!(
            after.last().unwrap().source_id,
            if agent == Agent::Codex {
                "native-codex:0".into()
            } else {
                format!("record:{}:0", fixture(agent).lines().count() + 1)
            }
        );
        let ids: std::collections::BTreeSet<_> = after.iter().map(|e| &e.source_id).collect();
        assert_eq!(ids.len(), after.len());
    }
}

#[test]
fn claude_child_identity_parent_and_automation_are_retained() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    write(
        &ctx.roots
            .claude_projects
            .join("project/parent/subagents/agent-child.jsonl"),
        "{\"type\":\"user\",\"sessionId\":\"parent\",\"agentId\":\"child\",\"message\":\"Task\"}\n",
    );
    let source = source_for(&ctx, Agent::Claude).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let (p, e) = parse(source.as_ref(), &d.sessions[0]);
    assert_eq!(p.session.key.id, "child");
    assert_eq!(p.session.parent.unwrap().id, "parent");
    assert!(e[0].automated);
}

#[test]
fn archive_duplicates_prefer_live_then_newest_then_path() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for agent in [Agent::Claude, Agent::Codex] {
        let live = root(&ctx, agent).join("project/live.jsonl");
        write(&live, &fixture(agent));
        let archive = if agent == Agent::Claude {
            ctx.roots.archive.as_ref().unwrap()
        } else {
            &ctx.roots.codex_archived_sessions
        };
        let copy = archive.join("project/archive.jsonl");
        write(&copy, &fixture(agent));
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        fs::File::open(&live).unwrap().set_modified(old).unwrap();
        let source = source_for(&ctx, agent).unwrap();
        let d = discover(source.as_ref(), &ctx);
        assert_eq!(d.sessions.len(), 1);
        assert_eq!(d.sessions[0].source, live.to_string_lossy());
        assert!(!parse(source.as_ref(), &d.sessions[0]).0.session.archived);
        fs::remove_file(&live).unwrap();
        let d = discover(source.as_ref(), &ctx);
        assert_eq!(d.sessions.len(), 1);
        assert!(parse(source.as_ref(), &d.sessions[0]).0.session.archived);
        let a = archive.join("project/a.jsonl");
        write(&a, &fixture(agent));
        fs::File::open(&a).unwrap().set_modified(old).unwrap();
        fs::File::open(&copy).unwrap().set_modified(old).unwrap();
        let d = discover(source.as_ref(), &ctx);
        assert_eq!(d.sessions[0].source, a.to_string_lossy());
    }
}

#[test]
fn droid_settings_fingerprint_and_parent_drive_automation() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = ctx.roots.droid_sessions.join("btw/child.jsonl");
    write(&path, &fixture(Agent::Droid));
    let source = source_for(&ctx, Agent::Droid).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let before = &d.sessions[0];
    assert!(!parse(source.as_ref(), before).1[0].automated);
    write(
        &path.with_extension("settings.json"),
        "{\"tags\":[{\"name\":\"exec\"}]}",
    );
    let d = discover(source.as_ref(), &ctx);
    assert_ne!(before.fingerprint, d.sessions[0].fingerprint);
    assert!(parse(source.as_ref(), &d.sessions[0]).1[0].automated);
    write(
        &path,
        "{\"type\":\"session_start\",\"id\":\"child\",\"callingSessionId\":\"parent\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"Task\"}}\n",
    );
    let d = discover(source.as_ref(), &ctx);
    let (p, e) = parse(source.as_ref(), &d.sessions[0]);
    assert_eq!(p.session.parent.unwrap().id, "parent");
    assert!(e[0].automated);
}

#[test]
fn pi_renames_and_nested_sessions_are_included() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    write(
        &ctx.roots
            .pi_sessions
            .join("project/subagents/date_wrong.jsonl"),
        &format!(
            "{}{{\"type\":\"session_info\",\"name\":\" Renamed\\n session \"}}\n",
            fixture(Agent::Pi)
        ),
    );
    let source = source_for(&ctx, Agent::Pi).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let (p, _) = parse(source.as_ref(), &d.sessions[0]);
    assert_eq!(p.session.key.id, "pi-canonical");
    assert_eq!(p.session.title.as_deref(), Some("Renamed  session"));
}

#[test]
fn missing_required_store_is_incomplete_and_symlinks_are_not_followed() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let source = source_for(&ctx, Agent::Pi).unwrap();
    let d = discover(source.as_ref(), &ctx);
    assert_eq!(d.diagnostics[0].code, "source_unavailable");
    assert!(d.complete_agents.is_empty());
    fs::create_dir_all(&ctx.roots.pi_sessions).unwrap();
    let outside = dir.path().join("outside");
    write(&outside.join("session.jsonl"), &fixture(Agent::Pi));
    symlink(&outside, ctx.roots.pi_sessions.join("linked")).unwrap();
    symlink(&ctx.roots.pi_sessions, ctx.roots.pi_sessions.join("loop")).unwrap();
    let d = discover(source.as_ref(), &ctx);
    assert_eq!(d.sessions.len(), 0);
    assert!(d.complete_agents.contains(&Agent::Pi));
    assert!(fingerprint(&[ctx.roots.pi_sessions.join("linked")]).is_err());
}

#[test]
fn changed_header_invalidates_cached_identity() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = ctx.roots.pi_sessions.join("date_wrong.jsonl");
    write(&path, &fixture(Agent::Pi));
    let source = source_for(&ctx, Agent::Pi).unwrap();
    let mut d = discover(source.as_ref(), &ctx);
    let raw = d.sessions.remove(0);
    let cache = DiscoveryCache {
        by_source: BTreeMap::from([(
            raw.source.clone(),
            CachedSource {
                key: raw.key,
                source: raw.source,
                dependencies: raw.dependencies,
                fingerprint: raw.fingerprint,
                state: None,
            },
        )]),
        verify: false,
    };
    let same = source.discover(&ctx, &cache).unwrap();
    assert_eq!(same.sessions[0].key.id, "pi-canonical");
    write(
        &path,
        &fixture(Agent::Pi).replace("pi-canonical", "new-identity"),
    );
    let changed = source.discover(&ctx, &cache).unwrap();
    assert_eq!(changed.sessions[0].key.id, "new-identity");
    assert!(
        parse(source.as_ref(), &changed.sessions[0])
            .1
            .iter()
            .all(|e| e.session.id == "new-identity")
    );
}

#[test]
fn callback_failure_stops_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    write(
        &ctx.roots.pi_sessions.join("session.jsonl"),
        &fixture(Agent::Pi),
    );
    let source = source_for(&ctx, Agent::Pi).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let mut count = 0;
    let result = source.parse(&d.sessions[0], &mut |_| {
        count += 1;
        Err(Error::InvalidArgument("stop".into()))
    });
    assert!(result.is_err());
    assert_eq!(count, 1);
}

#[test]
fn completed_last_record_recovers_and_unknown_metadata_stays_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = ctx.roots.claude_projects.join("project/session.jsonl");
    write(
        &path,
        "{\"type\":\"user\",\"sessionId\":\"canonical\",\"message\":\"Hello\"}\n{\"type\":\"future_kind\",\"text\":\"Future metadata\"}\n{\"type\":\"assistant\",\"message\":",
    );
    let source = source_for(&ctx, Agent::Claude).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let (p, e) = parse(source.as_ref(), &d.sessions[0]);
    assert!(!p.complete);
    assert_eq!(e[1].kind, Kind::Meta);
    assert_eq!(e[1].text, "Future metadata");
    write!(
        OpenOptions::new().append(true).open(&path).unwrap(),
        "\"Done\"}}"
    )
    .unwrap();
    let d = discover(source.as_ref(), &ctx);
    let (p, e) = parse(source.as_ref(), &d.sessions[0]);
    assert!(p.complete);
    assert_eq!(e.last().unwrap().text, "Done");
}

#[test]
fn droid_hooks_are_automated_and_malformed_settings_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = ctx.roots.droid_sessions.join("project/session.jsonl");
    write(
        &path,
        "{\"type\":\"session_start\",\"id\":\"session\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"Hook output\",\"hookEventName\":\"SessionStart\"}}\n",
    );
    write(&path.with_extension("settings.json"), "malformed secret");
    let source = source_for(&ctx, Agent::Droid).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let (p, e) = parse(source.as_ref(), &d.sessions[0]);
    assert!(e[0].automated);
    assert!(!p.complete);
    assert_eq!(p.diagnostics[0].code, "malformed_metadata");
    assert_eq!(p.diagnostics[0].message, "Settings are not a JSON object");
}

#[test]
fn pi_native_tool_result_has_tool_classification_and_name() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    write(
        &ctx.roots.pi_sessions.join("project/session.jsonl"),
        "{\"type\":\"session\",\"id\":\"native\",\"parentSession\":\"/fixture/2026_parent.jsonl\"}\n{\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"toolName\":\"read\",\"content\":[{\"type\":\"text\",\"text\":\"File contents\"}]}}\n",
    );
    let source = source_for(&ctx, Agent::Pi).unwrap();
    let d = discover(source.as_ref(), &ctx);
    let (p, e) = parse(source.as_ref(), &d.sessions[0]);
    assert_eq!(p.session.parent.unwrap().id, "parent");
    assert_eq!(e[0].kind, Kind::Tool);
    assert_eq!(e[0].tool_name.as_deref(), Some("read"));
    assert_eq!(e[0].text, "File contents");
}

#[test]
fn mixed_ringleader_archive_only_admits_identified_claude_records() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    fs::create_dir_all(&ctx.roots.claude_projects).unwrap();
    let archive = ctx.roots.archive.as_ref().unwrap();
    for agent in [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Pi] {
        write(
            &archive.join(format!("project/{agent}.jsonl")),
            &fixture(agent),
        );
    }
    let source = source_for(&ctx, Agent::Claude).unwrap();
    let d = discover(source.as_ref(), &ctx);
    assert_eq!(d.sessions.len(), 1);
    assert_eq!(d.sessions[0].key.id, "claude-canonical");
    assert_eq!(d.diagnostics, [] as [slimetrail::Diagnostic; 0]);
    assert!(d.complete_agents.contains(&Agent::Claude));
    write(
        &archive.join("project/unknown.jsonl"),
        "{\"type\":\"unrecognized\",\"text\":\"Unknown source\"}\n",
    );
    write(
        &archive.join("project/broken.jsonl"),
        "broken private content",
    );
    let d = discover(source.as_ref(), &ctx);
    assert_eq!(d.sessions.len(), 1);
    assert_eq!(d.diagnostics.len(), 2);
    assert!(
        d.diagnostics
            .iter()
            .all(|d| d.code == "archive_format_unknown")
    );
    assert!(d.complete_agents.is_empty());
}

#[test]
fn codex_native_names_refresh_for_live_and_archived_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for archived in [false, true] {
        let path = if archived {
            &ctx.roots.codex_archived_sessions
        } else {
            &ctx.roots.codex_sessions
        }
        .join("session.jsonl");
        write(&path, &fixture(Agent::Codex));
        let source = source_for(&ctx, Agent::Codex).unwrap();
        let before = discover(source.as_ref(), &ctx).sessions.remove(0);
        assert!(parse(source.as_ref(), &before).0.complete);
        for name in ["Named thread", "Renamed thread", "", "   "] {
            write(
                &ctx.roots.codex_session_index,
                &format!(
                    "{}\n{}\n",
                    json!({"id":"codex-canonical","thread_name":"Old name"}),
                    json!({"id":"codex-canonical","thread_name":name})
                ),
            );
            let raw = discover(source.as_ref(), &ctx).sessions.remove(0);
            assert_ne!(raw.fingerprint, before.fingerprint);
            let (parsed, _) = parse(source.as_ref(), &raw);
            assert!(parsed.complete);
            if name.trim().is_empty() {
                assert_eq!(parsed.session.title_origin, Some(TitleOrigin::Derived));
            } else {
                assert_eq!(parsed.session.title.as_deref(), Some(name));
                assert_eq!(parsed.session.title_origin, Some(TitleOrigin::Native));
            }
        }
        fs::remove_file(&ctx.roots.codex_session_index).unwrap();
        assert_eq!(
            parse(source.as_ref(), &before).0.session.title_origin,
            Some(TitleOrigin::Derived)
        );
        write(&ctx.roots.codex_session_index, "malformed secret\n");
        let parsed = parse(source.as_ref(), &before).0;
        assert!(!parsed.complete);
        assert_eq!(parsed.diagnostics[0].code, "malformed_metadata");
        fs::remove_file(&ctx.roots.codex_session_index).unwrap();
        fs::create_dir(&ctx.roots.codex_session_index).unwrap();
        let parsed = parse(source.as_ref(), &before).0;
        assert!(!parsed.complete);
        assert_eq!(parsed.diagnostics[0].code, "source_unavailable");
        fs::remove_dir(&ctx.roots.codex_session_index).unwrap();
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn codex_structured_tool_values_remain_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for (field, subtype) in [
        ("output", "function_call_output"),
        ("output", "custom_tool_call_output"),
        ("arguments", "function_call"),
        ("input", "custom_tool_call"),
    ] {
        for (value, expected) in [
            (
                json!([{"text":"Structured tool output"}]),
                "[{\"text\": \"Structured tool output\"}]",
            ),
            (
                json!({"z":"Object output", "a":false}),
                "{\"z\": \"Object output\", \"a\": false}",
            ),
            (json!(42), "42"),
            (json!(true), "true"),
        ] {
            let record = json!({"type":"response_item","payload":{"type":subtype,field:value}});
            write(
                &ctx.roots.codex_sessions.join("session.jsonl"),
                &format!("{}\n{record}\n", fixture(Agent::Codex)),
            );
            let source = source_for(&ctx, Agent::Codex).unwrap();
            let raw = discover(source.as_ref(), &ctx).sessions.remove(0);
            let (parsed, entries) = parse(source.as_ref(), &raw);
            assert!(parsed.complete);
            assert_eq!(entries.last().unwrap().kind, Kind::Tool);
            assert_eq!(entries.last().unwrap().text, expected);
        }
    }
}

#[test]
fn meaningful_metadata_is_preserved_across_agents() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for agent in [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Pi] {
        let records = [
            json!({"type":"compaction","summary":"Retained summary"}),
            json!({"type":"future","text":"Retained text"}),
            json!({"type":"usage","tokens":42}),
            json!({"type":"structural","id":"ignored"}),
        ];
        let extra = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        write(
            &root(&ctx, agent).join("session.jsonl"),
            &format!("{}\n{extra}\n", fixture(agent)),
        );
        let source = source_for(&ctx, agent).unwrap();
        let raw = discover(source.as_ref(), &ctx).sessions.remove(0);
        let (_, entries) = parse(source.as_ref(), &raw);
        let meta: Vec<_> = entries
            .iter()
            .filter(|e| e.text.starts_with("Retained"))
            .collect();
        assert_eq!(meta.len(), 2);
        assert!(meta.iter().all(|e| e.kind == Kind::Meta));
        assert_eq!(entries.last().unwrap().text, "Retained text");
    }
}

#[test]
fn pi_empty_and_whitespace_names_clear_native_titles() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for name in ["", "  \n\r  "] {
        write(
            &ctx.roots.pi_sessions.join("session.jsonl"),
            &format!(
                "{}\n{}\n",
                fixture(Agent::Pi),
                json!({"type":"session_info","name":name})
            ),
        );
        let source = source_for(&ctx, Agent::Pi).unwrap();
        let raw = discover(source.as_ref(), &ctx).sessions.remove(0);
        let (parsed, _) = parse(source.as_ref(), &raw);
        assert_eq!(parsed.session.title, None);
        assert_eq!(parsed.session.title_origin, None);
    }
}
