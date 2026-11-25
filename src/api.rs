use crate::{
    Agent, Context, Entry, Envelope, Error, Filter, IndexStats, ReadOptions, ReadPage,
    RefreshOptions, Result, Scope, SearchOptions, SearchResult, Session, SessionKey,
    TimelineOptions, TimelineResult,
    index::{IndexSession, Refreshed},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

pub fn refresh(
    ctx: &Context,
    agents: &BTreeSet<Agent>,
    options: &RefreshOptions<'_>,
) -> Result<Envelope<IndexStats>> {
    IndexSession::open(ctx)?.refresh(&selected(agents), options)
}

pub fn sessions(
    ctx: &Context,
    filter: &Filter,
    refresh: &RefreshOptions<'_>,
) -> Result<Envelope<Vec<Session>>> {
    let (index, refreshed) = prepared(ctx, &filter.agents, refresh)?;
    if !refreshed.available {
        return Ok(with_data(refreshed.status, Some(Vec::new())));
    }
    let status = refreshed.status;
    let mut sessions = Vec::new();
    index.visit_sessions(&mut |session| {
        if accepts(ctx, filter, &session) {
            sessions.push(session);
        }
        Ok(())
    })?;
    Ok(with_data(status, Some(sessions)))
}

pub fn entries(
    ctx: &Context,
    key: &SessionKey,
    refresh: &RefreshOptions<'_>,
    emit: &mut dyn FnMut(Entry) -> Result<()>,
) -> Result<Envelope<Session>> {
    let (index, refreshed) = prepared(ctx, &BTreeSet::from([key.agent]), refresh)?;
    if !refreshed.available {
        return Ok(with_data(refreshed.status, None));
    }
    let status = refreshed.status;
    let session = lookup(&index, key, status.incomplete)?;
    if session.is_some() {
        index.visit_entries(key, emit)?;
    }
    Ok(with_data(status, session))
}

pub fn search(
    ctx: &Context,
    options: &SearchOptions,
    refresh: &RefreshOptions<'_>,
) -> Result<Envelope<SearchResult>> {
    let pattern = crate::search::pattern(options)?;
    let (index, refreshed) = prepared(ctx, &options.filter.agents, refresh)?;
    if !refreshed.available {
        let empty = SearchResult {
            query: options.query.clone(),
            agents: selected(&options.filter.agents).into_iter().collect(),
            sort: options.sort,
            count: 0,
            results: Vec::new(),
        };
        return Ok(with_data(refreshed.status, Some(empty)));
    }
    let status = refreshed.status;
    let result = crate::search::run(ctx, &index, options, &pattern)?;
    let mut envelope = with_data(status, Some(result));
    envelope.diagnostics.extend(crate::search::coverage_notes(
        index.coverage()?,
        ctx.coverage,
        ctx.coverage_explicit,
        &options.filter.kinds,
    ));
    Ok(envelope)
}

pub fn read(
    ctx: &Context,
    key: &SessionKey,
    options: &ReadOptions,
    refresh: &RefreshOptions<'_>,
) -> Result<Envelope<ReadPage>> {
    if options.max_chars == 0 {
        return Err(Error::InvalidArgument("max_chars must be positive".into()));
    }
    let (index, refreshed) = prepared(ctx, &BTreeSet::from([key.agent]), refresh)?;
    if !refreshed.available {
        return Ok(with_data(refreshed.status, None));
    }
    let status = refreshed.status;
    let result = lookup(&index, key, status.incomplete)?
        .map(|session| crate::read::run(&index, session, options))
        .transpose()?;
    Ok(with_data(status, result))
}

pub fn timeline(
    ctx: &Context,
    options: &TimelineOptions,
    refresh: &RefreshOptions<'_>,
) -> Result<Envelope<TimelineResult>> {
    if options.since >= options.until {
        return Err(Error::InvalidArgument("since must precede until".into()));
    }
    if options.idle_ms == 0 {
        return Err(Error::InvalidArgument("idle_ms must be positive".into()));
    }
    let zone = jiff::tz::TimeZone::get(&ctx.time_zone)?;
    jiff::Timestamp::from_millisecond(options.since)?;
    jiff::Timestamp::from_millisecond(options.until)?;
    let (index, refreshed) = prepared(ctx, &options.filter.agents, refresh)?;
    if !refreshed.available {
        let empty = TimelineResult {
            sessions: Vec::new(),
            daily_active_ms: BTreeMap::new(),
        };
        return Ok(with_data(refreshed.status, Some(empty)));
    }
    let mut status = refreshed.status;
    let (result, diagnostics) = crate::timeline::run(ctx, &index, options, &zone)?;
    status.incomplete |= !diagnostics.is_empty();
    status.diagnostics.extend(diagnostics);
    Ok(with_data(status, Some(result)))
}

fn prepared(
    ctx: &Context,
    agents: &BTreeSet<Agent>,
    refresh: &RefreshOptions<'_>,
) -> Result<(IndexSession, Refreshed)> {
    let mut index = IndexSession::open(ctx)?;
    let refreshed = index.refresh_gated(&selected(agents), refresh)?;
    Ok((index, refreshed))
}

fn lookup(index: &IndexSession, key: &SessionKey, incomplete: bool) -> Result<Option<Session>> {
    let session = index.session(key)?;
    if session.is_none() && !incomplete {
        return Err(Error::InvalidArgument("session not found".into()));
    }
    Ok(session)
}

fn with_data<T>(status: Envelope<IndexStats>, data: Option<T>) -> Envelope<T> {
    Envelope {
        version: 1,
        incomplete: status.incomplete,
        diagnostics: status.diagnostics,
        data,
    }
}

pub(crate) fn selected(agents: &BTreeSet<Agent>) -> BTreeSet<Agent> {
    if agents.is_empty() {
        BTreeSet::from([
            Agent::Claude,
            Agent::Codex,
            Agent::Cursor,
            Agent::Devin,
            Agent::Droid,
            Agent::Opencode,
            Agent::Pi,
        ])
    } else {
        agents.clone()
    }
}

pub(crate) fn accepts(ctx: &Context, filter: &Filter, session: &Session) -> bool {
    if !filter.agents.is_empty() && !filter.agents.contains(&session.key.agent) {
        return false;
    }
    if let Some(days) = filter.days {
        let cutoff = ctx.now_ms.saturating_sub(i64::from(days) * 86_400_000);
        if session.source_updated_at.is_none_or(|time| time < cutoff) {
            return false;
        }
    }
    let app = filter.app.as_deref().filter(|s| !s.is_empty());
    let Some(cwd) = session.cwd.as_ref().filter(|s| !s.as_os_str().is_empty()) else {
        return filter.scope == Scope::All && app.is_none();
    };
    let actual = normalized(cwd, &ctx.cwd);
    if let Some(app) = app {
        return actual
            .to_string_lossy()
            .to_lowercase()
            .contains(&app.to_lowercase());
    }
    let target = normalized(&filter.cwd, &ctx.cwd);
    match filter.scope {
        Scope::All => true,
        Scope::Current => actual == target,
        Scope::Siblings => actual == target || actual.parent() == target.parent(),
    }
}

fn normalized(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut output = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                output.pop();
            }
            Component::CurDir => {}
            component => output.push(component),
        }
    }
    output
}
