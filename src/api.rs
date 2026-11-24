use crate::{
    Agent, Context, Entry, Envelope, Error, Filter, IndexStats, ReadOptions, ReadPage, Result,
    Scope, SearchOptions, SearchResult, Session, SessionKey, TimelineOptions, TimelineResult,
    index, index::IndexSession,
};
use std::{
    collections::BTreeSet,
    io::IsTerminal,
    path::{Component, Path, PathBuf},
};

pub fn refresh(
    ctx: &Context,
    agents: &BTreeSet<Agent>,
    verify: bool,
) -> Result<Envelope<IndexStats>> {
    // Placeholder until Context gains `progress`.
    index::refresh_noting(
        ctx,
        &selected(agents),
        std::io::stderr().is_terminal(),
        verify,
    )
}

pub fn sessions(ctx: &Context, filter: &Filter) -> Result<Envelope<Vec<Session>>> {
    let (index, status) = prepared(ctx, &filter.agents)?;
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
    emit: &mut dyn FnMut(Entry) -> Result<()>,
) -> Result<Envelope<Session>> {
    let (index, status) = prepared(ctx, &BTreeSet::from([key.agent]))?;
    let session = lookup(&index, key, status.incomplete)?;
    if session.is_some() {
        index.visit_entries(key, emit)?;
    }
    Ok(with_data(status, session))
}

pub fn search(ctx: &Context, options: &SearchOptions) -> Result<Envelope<SearchResult>> {
    let pattern = crate::search::pattern(options)?;
    let (index, status) = prepared(ctx, &options.filter.agents)?;
    let result = crate::search::run(ctx, &index, options, &pattern)?;
    Ok(with_data(status, Some(result)))
}

pub fn read(ctx: &Context, key: &SessionKey, options: &ReadOptions) -> Result<Envelope<ReadPage>> {
    if options.max_chars == 0 {
        return Err(Error::InvalidArgument("max_chars must be positive".into()));
    }
    let (index, status) = prepared(ctx, &BTreeSet::from([key.agent]))?;
    let result = lookup(&index, key, status.incomplete)?
        .map(|session| crate::read::run(&index, session, options))
        .transpose()?;
    Ok(with_data(status, result))
}

pub fn timeline(ctx: &Context, options: &TimelineOptions) -> Result<Envelope<TimelineResult>> {
    if options.since >= options.until {
        return Err(Error::InvalidArgument("since must precede until".into()));
    }
    if options.idle_ms == 0 {
        return Err(Error::InvalidArgument("idle_ms must be positive".into()));
    }
    let zone = jiff::tz::TimeZone::get(&ctx.time_zone)?;
    jiff::Timestamp::from_millisecond(options.since)?;
    jiff::Timestamp::from_millisecond(options.until)?;
    let (index, mut status) = prepared(ctx, &options.filter.agents)?;
    let (result, diagnostics) = crate::timeline::run(ctx, &index, options, &zone)?;
    status.incomplete |= !diagnostics.is_empty();
    status.diagnostics.extend(diagnostics);
    Ok(with_data(status, Some(result)))
}

fn prepared(
    ctx: &Context,
    agents: &BTreeSet<Agent>,
) -> Result<(IndexSession, Envelope<IndexStats>)> {
    let mut index = IndexSession::open(ctx)?;
    let status = index.refresh_for_query(&selected(agents))?;
    Ok((index, status))
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
