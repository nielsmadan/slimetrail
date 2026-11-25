#[allow(dead_code)]
mod query_support;
#[allow(dead_code)]
mod support;
use query_support::*;
use serde_json::json;
use slimetrail::*;
use std::{
    collections::BTreeSet,
    fs,
    time::{Duration, SystemTime},
};

fn opts(since: &str, until: &str) -> TimelineOptions {
    TimelineOptions {
        filter: filter(Agent::Claude),
        since: ts(since),
        until: ts(until),
        idle_ms: 1_800_000,
    }
}

#[test]
fn window_is_half_open_idle_boundary_inclusive_and_all_activity_counts() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[
            event("user", "outside before", Some("2026-08-09T23:59:59Z")),
            event("user", "human", Some("2026-08-10T00:00:00Z")),
            json!({"type":"user","isMeta":true,"timestamp":"2026-08-10T00:10:00Z","message":"automated"}),
            json!({"type":"assistant","timestamp":"2026-08-10T00:30:00Z","message":{"content":[{"type":"tool_result","content":"tool"}]}}),
            event("assistant", "exact idle", Some("2026-08-10T01:00:00Z")),
            event("user", "new span", Some("2026-08-10T01:30:00.001Z")),
            event("user", "outside until", Some("2026-08-11T00:00:00Z")),
        ],
    );
    let mut opt = opts("2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z");
    opt.filter.kinds = BTreeSet::from([Kind::Meta]);
    let result = timeline(&ctx, &opt, &RefreshOptions::QUERY).unwrap();
    assert!(!result.incomplete);
    let result = result.data.unwrap();
    let spans = &result.sessions[0].spans;
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].human_prompts, 1);
    assert_eq!(spans[0].end, ts("2026-08-10T01:00:00Z"));
    assert_eq!(spans[1].human_prompts, 1);
    assert_eq!(result.sessions[0].active_ms, 3_600_000);
    assert_eq!(result.daily_active_ms["2026-08-10"], 3_600_000);
}

#[test]
fn unordered_events_and_simultaneous_sessions_union_daily_time() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for (id, start, end) in [
        ("b", "2026-08-10T00:30:00Z", "2026-08-10T01:30:00Z"),
        ("a", "2026-08-10T00:00:00Z", "2026-08-10T01:00:00Z"),
    ] {
        claude(
            &ctx,
            id,
            None,
            None,
            &[
                event("assistant", "end", Some(end)),
                event("user", "start", Some(start)),
                event("user", "duplicate instant", Some(start)),
            ],
        );
    }
    let mut opt = opts("2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z");
    opt.idle_ms = 3_600_000;
    let result = timeline(&ctx, &opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.sessions[0].session.key.id, "a");
    assert_eq!(result.sessions[0].spans[0].human_prompts, 2);
    assert_eq!(
        result.sessions.iter().map(|s| s.active_ms).sum::<u64>(),
        7_200_000
    );
    assert_eq!(result.daily_active_ms["2026-08-10"], 5_400_000);
}

#[test]
fn civil_midnights_respect_spring_and_autumn_dst() {
    for (start, end, day, hours) in [
        (
            "2026-03-28T23:00:00Z",
            "2026-03-29T22:00:00Z",
            "2026-03-29",
            23,
        ),
        (
            "2026-10-24T22:00:00Z",
            "2026-10-25T23:00:00Z",
            "2026-10-25",
            25,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.time_zone = "Europe/Berlin".into();
        claude(
            &ctx,
            "one",
            None,
            None,
            &[
                event("user", "start", Some(start)),
                event("assistant", "end", Some(end)),
            ],
        );
        let mut opt = opts(start, "2026-10-27T00:00:00Z");
        opt.idle_ms = 26 * 3_600_000;
        let result = timeline(&ctx, &opt, &RefreshOptions::QUERY)
            .unwrap()
            .data
            .unwrap();
        assert_eq!(result.daily_active_ms.len(), 1);
        assert_eq!(result.daily_active_ms[day], hours * 3_600_000);
        assert_eq!(result.sessions[0].active_ms, hours * 3_600_000);
    }
}

#[test]
fn spans_split_at_midnight_in_explicit_zone() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = context(dir.path());
    ctx.time_zone = "Asia/Kolkata".into();
    claude(
        &ctx,
        "one",
        None,
        None,
        &[
            event("user", "start", Some("2026-08-10T18:20:00Z")),
            event("assistant", "end", Some("2026-08-10T18:50:00Z")),
        ],
    );
    let result = timeline(
        &ctx,
        &opts("2026-08-10T00:00:00Z", "2026-08-12T00:00:00Z"),
        &RefreshOptions::QUERY,
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(result.daily_active_ms["2026-08-10"], 600_000);
    assert_eq!(result.daily_active_ms["2026-08-11"], 1_200_000);
}

#[test]
fn missing_timestamps_are_counted_once_per_source_without_text() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    claude(
        &ctx,
        "one",
        None,
        None,
        &[
            event("user", "secret one", None),
            event("assistant", "secret two", Some("bad secret date")),
            event("user", "valid", Some("2026-08-10T00:00:00Z")),
        ],
    );
    let result = timeline(
        &ctx,
        &opts("2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z"),
        &RefreshOptions::QUERY,
    )
    .unwrap();
    assert!(result.incomplete);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].code, "timeline_missing_timestamps");
    assert_eq!(
        result.diagnostics[0].message,
        "Skipped 2 entries without a usable timestamp"
    );
    assert_eq!(result.data.unwrap().sessions.len(), 1);
}

#[test]
fn historical_until_uses_since_for_source_cutoff_not_today() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    for (id, modified) in [
        ("fresh", "2026-08-10T01:00:00Z"),
        ("old", "2026-08-09T23:59:59Z"),
    ] {
        let path = claude(
            &ctx,
            id,
            None,
            None,
            &[event("user", "event", Some("2026-08-10T00:00:00Z"))],
        );
        fs::File::open(path)
            .unwrap()
            .set_modified(
                SystemTime::UNIX_EPOCH
                    + Duration::from_millis(u64::try_from(ts(modified)).unwrap()),
            )
            .unwrap();
    }
    let mut opt = opts("2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z");
    opt.filter.days = Some(1);
    let result = timeline(&ctx, &opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].session.key.id, "fresh");
}

#[test]
fn claude_cursor_children_excluded_while_other_agent_children_remain() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    six(&ctx);
    let child = ctx
        .roots
        .claude_projects
        .join("project/parent/subagents/agent-child.jsonl");
    records(
        &child,
        &[
            json!({"type":"user","sessionId":"parent","agentId":"child","timestamp":"2026-08-10T10:03:00Z","message":"needle child"}),
        ],
    );
    write(
        &ctx.roots
            .cursor_projects
            .join("slug/agent-transcripts/main/subagents/child.jsonl"),
        include_str!("fixtures/jsonl/cursor.jsonl"),
    );
    let mut search_options = options("needle");
    search_options.filter.agents.clear();
    let all = search(&ctx, &search_options, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert!(
        all.results
            .iter()
            .any(|r| r.session.key.agent == Agent::Claude && r.session.key.id == "child")
    );
    assert!(
        all.results
            .iter()
            .any(|r| r.session.key.agent == Agent::Cursor && r.session.key.id == "child")
    );
    let mut opt = opts("2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z");
    opt.filter.agents.clear();
    let result = timeline(&ctx, &opt, &RefreshOptions::QUERY)
        .unwrap()
        .data
        .unwrap();
    assert_eq!(result.sessions.len(), 6);
    let codex = result
        .sessions
        .iter()
        .find(|s| s.session.key.agent == Agent::Codex)
        .unwrap();
    assert!(codex.session.parent.is_some());
    assert_eq!(codex.spans[0].human_prompts, 0);
}

#[test]
fn invalid_windows_zones_and_idle_values_fail_before_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = context(dir.path());
    let mut opt = opts("2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z");
    ctx.time_zone = "Not/AZone".into();
    assert!(timeline(&ctx, &opt, &RefreshOptions::QUERY).is_err());
    ctx.time_zone = "UTC".into();
    opt.idle_ms = 0;
    assert!(timeline(&ctx, &opt, &RefreshOptions::QUERY).is_err());
    opt.idle_ms = 1;
    opt.until = opt.since;
    assert!(timeline(&ctx, &opt, &RefreshOptions::QUERY).is_err());
}
