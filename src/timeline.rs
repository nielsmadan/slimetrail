use crate::{
    Agent, Context, Diagnostic, Error, Result, Session, Span, TimelineOptions, TimelineResult,
    TimelineSession, Timestamp, api::accepts, index::IndexSession,
};
use jiff::{Timestamp as Instant, tz::TimeZone};
use std::{collections::BTreeMap, path::Path};

pub(crate) fn run(
    ctx: &Context,
    index: &IndexSession,
    options: &TimelineOptions,
    zone: &TimeZone,
) -> Result<(TimelineResult, Vec<Diagnostic>)> {
    let mut sessions = Vec::new();
    let mut filter = options.filter.clone();
    filter.days = None;
    let mut omissions = BTreeMap::<(Agent, String), u64>::new();
    index.visit_sessions(&mut |session| {
        if !accepts(ctx, &filter, &session)
            || session
                .source_updated_at
                .is_none_or(|time| time < options.since)
            || excluded_child(&session)
        {
            return Ok(());
        }
        let mut events = Vec::new();
        index.visit_events(&session.key, options.since, options.until, &mut |entry| {
            match entry.timestamp {
                Some(time) if options.since <= time && time < options.until => {
                    events.push((time, entry.human_prompt));
                }
                None => {
                    *omissions
                        .entry((session.key.agent, session.source.clone()))
                        .or_default() += 1;
                }
                _ => {}
            }
            Ok(())
        })?;
        let spans = activity_spans(events, options.idle_ms);
        if !spans.is_empty() {
            let active_ms = spans.iter().map(|span| span.end.abs_diff(span.start)).sum();
            sessions.push(TimelineSession {
                session,
                spans,
                active_ms,
            });
        }
        Ok(())
    })?;
    sessions.sort_by(|a, b| {
        a.spans[0]
            .start
            .cmp(&b.spans[0].start)
            .then_with(|| a.session.key.agent.cmp(&b.session.key.agent))
            .then_with(|| a.session.key.id.cmp(&b.session.key.id))
    });
    let daily_active_ms = daily(sessions.iter().flat_map(|s| s.spans.iter()), zone)?;
    let diagnostics = omissions
        .into_iter()
        .map(|((agent, source), count)| Diagnostic {
            agent: Some(agent),
            source: Some(source),
            code: "timeline_missing_timestamps".into(),
            message: format!("Skipped {count} entries without a usable timestamp"),
        })
        .collect();
    Ok((
        TimelineResult {
            sessions,
            daily_active_ms,
        },
        diagnostics,
    ))
}

fn excluded_child(session: &Session) -> bool {
    matches!(
        session.key.agent,
        Agent::Claude | Agent::Cursor | Agent::Devin
    ) && (session.parent.is_some()
        || Path::new(&session.source)
            .components()
            .any(|p| p.as_os_str() == "subagents"))
}

fn activity_spans(mut events: Vec<(Timestamp, bool)>, idle_ms: u64) -> Vec<Span> {
    events.sort_unstable();
    let mut spans: Vec<Span> = Vec::new();
    for (time, prompt) in events {
        if let Some(last) = spans
            .last_mut()
            .filter(|last| time.abs_diff(last.end) <= idle_ms)
        {
            last.end = time;
            last.human_prompts += u64::from(prompt);
        } else {
            spans.push(Span {
                start: time,
                end: time,
                human_prompts: u64::from(prompt),
            });
        }
    }
    spans
}

fn daily<'a>(
    spans: impl Iterator<Item = &'a Span>,
    zone: &TimeZone,
) -> Result<BTreeMap<String, u64>> {
    let mut intervals: Vec<_> = spans.map(|s| (s.start, s.end)).collect();
    intervals.sort_unstable();
    let mut merged: Vec<(Timestamp, Timestamp)> = Vec::new();
    for (start, end) in intervals {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut totals = BTreeMap::new();
    for (mut cursor, end) in merged {
        while cursor < end {
            let local = Instant::from_millisecond(cursor)?.to_zoned_datetime(zone.clone());
            let next = local
                .date()
                .tomorrow()?
                .to_zoned_datetime(zone.clone())?
                .start_of_day()?
                .timestamp()
                .as_millisecond();
            let piece_end = end.min(next);
            if piece_end <= cursor {
                return Err(Error::InvalidArgument(
                    "time zone did not advance to the next civil day".into(),
                ));
            }
            *totals.entry(local.date().to_string()).or_default() += piece_end.abs_diff(cursor);
            cursor = piece_end;
        }
    }
    Ok(totals)
}
