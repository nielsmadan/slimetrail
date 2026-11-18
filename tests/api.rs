#[allow(dead_code)]
mod query_support;
use query_support::*;
use serde_json::{Value, json};
use slimetrail::*;
use std::{collections::BTreeSet, fs};

#[test]
#[expect(
    clippy::cast_precision_loss,
    reason = "The Python goldens store fixture scores as floats"
)]
fn six_agent_reference_search_read_and_timeline_goldens() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    six(&ctx);
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/query/six.golden.json")).unwrap();
    for agent in [
        Agent::Claude,
        Agent::Codex,
        Agent::Cursor,
        Agent::Droid,
        Agent::Opencode,
        Agent::Pi,
    ] {
        let selected = filter(agent);
        let list = sessions(&ctx, &selected).unwrap();
        assert!(!list.incomplete, "{:?}", list.diagnostics);
        let session = &list.data.unwrap()[0];
        for query in ["needle", "café", "Native", "title"] {
            let mut opt = options(query);
            opt.filter = selected.clone();
            let result = search(&ctx, &opt).unwrap().data.unwrap();
            let actual = result.results.first().map_or(Value::Null, |hit|json!({"score":hit.score as f64,"matches":hit.matches,"by_kind":hit.by_kind,"snippets":hit.snippets}));
            assert_eq!(
                actual,
                golden[agent.to_string()]["search"][query],
                "{agent} {query}"
            );
        }
        for (name, mode) in [
            ("lite", ReadMode::Lite),
            ("full", ReadMode::Full),
            ("log", ReadMode::Log),
        ] {
            let page = read(
                &ctx,
                &session.key,
                &ReadOptions {
                    mode,
                    offset: 0,
                    max_chars: 100_000,
                },
            )
            .unwrap()
            .data
            .unwrap();
            assert_eq!(
                page.text,
                golden[agent.to_string()]["read"][name],
                "{agent} {name}"
            );
        }
        let result = timeline(
            &ctx,
            &TimelineOptions {
                filter: selected,
                since: ts("2026-08-10T00:00:00Z"),
                until: ts("2026-08-11T00:00:00Z"),
                idle_ms: 1_800_000,
            },
        )
        .unwrap()
        .data
        .unwrap();
        assert_eq!(
            serde_json::to_value(&result.sessions[0].spans).unwrap(),
            golden[agent.to_string()]["spans"],
            "{agent}"
        );
        assert_eq!(
            serde_json::to_value(&result.daily_active_ms).unwrap(),
            golden[agent.to_string()]["daily"],
            "{agent}"
        );
    }
}

#[test]
fn partial_sources_preserve_results_and_status_across_all_apis() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let path = claude(
        &ctx,
        "one",
        None,
        None,
        &[event("user", "needle", Some("2026-11-16T00:00:00Z"))],
    );
    let valid = fs::read_to_string(&path).unwrap();
    write(&path, &format!("{valid}malformed\n"));
    let mut opt = options("needle");
    opt.filter.agents.insert(Agent::Pi);
    let found = search(&ctx, &opt).unwrap();
    assert!(found.incomplete);
    assert_eq!(found.data.unwrap().count, 1);
    assert_eq!(found.diagnostics.len(), 2);
    let list = sessions(&ctx, &opt.filter).unwrap();
    assert!(list.incomplete);
    assert_eq!(list.data.unwrap().len(), 1);
    let key = "claude:one".parse().unwrap();
    let mut seen = Vec::new();
    let streamed = entries(&ctx, &key, &mut |e| {
        seen.push(e.text);
        Ok(())
    })
    .unwrap();
    assert!(streamed.incomplete);
    assert_eq!(seen, ["needle"]);
    let page = read(
        &ctx,
        &key,
        &ReadOptions {
            mode: ReadMode::Lite,
            offset: 0,
            max_chars: 100,
        },
    )
    .unwrap();
    assert!(page.incomplete);
    assert!(page.data.is_some());
    let unknown = entries(&ctx, &"pi:missing".parse().unwrap(), &mut |_| {
        panic!("unavailable source emitted entry")
    })
    .unwrap();
    assert!(unknown.incomplete);
    assert!(unknown.data.is_none());
    let activity = timeline(
        &ctx,
        &TimelineOptions {
            filter: opt.filter.clone(),
            since: ts("2026-11-16T00:00:00Z"),
            until: ts("2026-11-17T00:00:00Z"),
            idle_ms: 60_000,
        },
    )
    .unwrap();
    assert!(activity.incomplete);
    assert_eq!(
        activity
            .diagnostics
            .iter()
            .map(|d| d.code.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["malformed_records", "source_unavailable"])
    );
    let data = activity.data.unwrap();
    assert_eq!(data.sessions.len(), 1);
    assert_eq!(data.sessions[0].session.key, key);
    assert_eq!(data.sessions[0].spans[0].human_prompts, 1);
    fs::create_dir_all(&ctx.roots.pi_sessions).unwrap();
    assert!(entries(&ctx, &"pi:missing".parse().unwrap(), &mut |_| Ok(())).is_err());
}

#[test]
fn default_selection_refreshes_all_and_callbacks_can_stop() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    six(&ctx);
    assert_eq!(
        refresh(&ctx, &BTreeSet::new())
            .unwrap()
            .data
            .unwrap()
            .discovered,
        6
    );
    let mut f = filter(Agent::Claude);
    f.agents.clear();
    assert_eq!(sessions(&ctx, &f).unwrap().data.unwrap().len(), 6);
    let mut count = 0;
    let result = entries(
        &ctx,
        &"claude:claude-canonical".parse().unwrap(),
        &mut |_| {
            count += 1;
            Err(Error::InvalidArgument("stopped".into()))
        },
    );
    assert!(result.is_err());
    assert_eq!(count, 1);
}

#[test]
fn dto_conversions_preserve_wire_shapes_and_fractional_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[
            event("user", "needle", Some("2026-11-16T01:00:00+01:00")),
            event("assistant", "done", Some("2026-11-16T00:00:30Z")),
        ],
    );
    let found = search(&ctx, &options("needle")).unwrap().data.unwrap();
    let data = serde_json::to_value(dto::SearchDataDto::try_from(&found).unwrap()).unwrap();
    assert_eq!(data["results"][0]["ref"], "claude:one");
    assert_eq!(data["count"], 1);
    let key = "claude:one".parse().unwrap();
    entries(&ctx, &key, &mut |e| {
        if e.index == 0 {
            assert_eq!(
                dto::EntryDto::try_from(&e)?.timestamp.as_deref(),
                Some("2026-11-16T01:00:00+01:00")
            );
        }
        Ok(())
    })
    .unwrap();
    let page = read(
        &ctx,
        &key,
        &ReadOptions {
            mode: ReadMode::Log,
            offset: 1,
            max_chars: 3,
        },
    )
    .unwrap()
    .data
    .unwrap();
    let wire = dto::ReadDataDto::try_from(&page).unwrap();
    assert_eq!(wire.body, "026");
    assert_eq!(wire.next_offset, Some(4));
    let result = timeline(
        &ctx,
        &TimelineOptions {
            filter: filter(Agent::Claude),
            since: ts("2026-11-16T00:00:00Z"),
            until: ts("2026-11-17T00:00:00Z"),
            idle_ms: 60000,
        },
    )
    .unwrap()
    .data
    .unwrap();
    let wire = dto::TimelineDataDto::try_from(&result).unwrap();
    assert_eq!(wire.sessions[0].active_minutes, 0.5);
    assert_eq!(wire.daily_active_minutes["2026-11-16"], 0.5);
}

#[test]
fn thinking_blocks_keep_order_ids_kind_weights_and_read_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for agent in [Agent::Claude, Agent::Pi] {
        let blocks = json!([
            {"type":"thinking", "thinking":"reason needle"},
            {"type":"text", "text":"visible needle"},
            {"type":"reasoning", "text":"later needle"},
            {"type":"tool_use", "name":"Shell", "input":"tool needle"}
        ]);
        let path = match agent {
            Agent::Claude => ctx.roots.claude_projects.join("thinking.jsonl"),
            _ => ctx.roots.pi_sessions.join("thinking.jsonl"),
        };
        let values = match agent {
            Agent::Claude => vec![
                json!({"type":"assistant", "sessionId":"thinking", "uuid":"native", "message":{"content":blocks}}),
            ],
            _ => vec![
                json!({"type":"session", "id":"thinking"}),
                json!({"type":"message", "id":"native", "message":{"role":"assistant", "content":blocks}}),
            ],
        };
        records(&path, &values);
        let key = SessionKey {
            agent,
            id: "thinking".into(),
        };
        let mut seen = Vec::new();
        entries(&ctx, &key, &mut |entry| {
            seen.push(entry);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen.iter()
                .map(|entry| (
                    entry.index,
                    entry.source_id.as_str(),
                    entry.kind,
                    entry.subtype.as_deref()
                ))
                .collect::<Vec<_>>(),
            [
                (0, "native:0", Kind::Meta, Some("reasoning")),
                (1, "native:1", Kind::Message, None),
                (2, "native:2", Kind::Meta, Some("reasoning")),
                (3, "native:3", Kind::Tool, None),
            ]
        );
        let mut opt = options("needle");
        opt.filter = filter(agent);
        let found = search(&ctx, &opt).unwrap().data.unwrap();
        assert_eq!((found.results[0].score, found.results[0].matches), (8, 4));
        opt.filter.kinds = BTreeSet::from([Kind::Meta]);
        let meta = search(&ctx, &opt).unwrap().data.unwrap();
        assert_eq!((meta.results[0].score, meta.results[0].matches), (4, 2));
        let page = |mode| {
            read(
                &ctx,
                &key,
                &ReadOptions {
                    mode,
                    offset: 0,
                    max_chars: 1000,
                },
            )
            .unwrap()
            .data
            .unwrap()
            .text
        };
        assert_eq!(
            page(ReadMode::Lite),
            "[assistant/message]\nvisible needle\n\n"
        );
        assert_eq!(
            page(ReadMode::Full),
            "[assistant/meta]\nreason needle\n\n[assistant/message]\nvisible needle\n\n[assistant/meta]\nlater needle\n\n"
        );
    }
}
