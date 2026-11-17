use clap::{Args, Parser, Subcommand};
use jiff::{Timestamp, civil, tz::TimeZone};
use serde::Serialize;
use slimetrail::{dto::*, *};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(version, about = "Read-only search across local coding-agent sessions")]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    #[arg(long, global = true, default_value = "text", value_parser = ["text", "json"])]
    format: String,
    #[arg(long, global = true)]
    index_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    time_zone: Option<String>,
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    #[arg(long, global = true, default_value = "siblings", value_parser = ["current", "siblings", "all"])]
    scope: String,
    #[arg(long, global = true)]
    app: Option<String>,
    #[arg(long, global = true)]
    days: Option<u32>,
    #[arg(long, global = true, default_value = "all", value_delimiter = ',', value_parser = ["all", "message", "tool", "meta"])]
    kind: Vec<String>,
    #[arg(long, global = true)]
    claude: bool,
    #[arg(long, global = true)]
    codex: bool,
    #[arg(long, global = true)]
    cursor: bool,
    #[arg(long, global = true)]
    droid: bool,
    #[arg(long, global = true)]
    opencode: bool,
    #[arg(long, global = true)]
    pi: bool,
    #[arg(long, global = true)]
    all: bool,
    #[arg(
        long,
        global = true,
        help = "Select AGENT_HARNESS (claude/codex/cursor/droid/opencode/pi)"
    )]
    current: bool,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Search session titles and entries")]
    Search {
        query: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        case_sensitive: bool,
        #[arg(long, default_value = "score", value_parser = ["score", "recent"])]
        sort: String,
        #[arg(long, default_value_t = 50)]
        max_results: usize,
        #[arg(long, default_value_t = 3)]
        max_snippets: usize,
        #[arg(long, default_value_t = 160)]
        context: usize,
    },
    #[command(about = "Read a session transcript")]
    Read {
        session: SessionKey,
        #[arg(long, default_value = "lite", value_parser = ["lite", "full", "log"])]
        mode: String,
        #[arg(long, default_value_t = 200000, value_parser = positive_usize)]
        max_chars: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    #[command(about = "Summarize session activity over time")]
    Timeline {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 30, value_parser = positive_u64)]
        idle_minutes: u64,
    },
    #[command(about = "Refresh the local search index")]
    Index,
    #[command(about = "List session metadata")]
    Sessions,
    #[command(about = "Stream entries from a session")]
    Entries { session: SessionKey },
}

fn positive_usize(value: &str) -> std::result::Result<usize, String> {
    value
        .parse()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| "must be a positive integer".into())
}
fn positive_u64(value: &str) -> std::result::Result<u64, String> {
    value
        .parse()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

pub fn run() -> i32 {
    let cli = Cli::parse();
    let env = std::env::vars().collect::<BTreeMap<_, _>>();
    let mut stdout = io::stdout().lock();
    match execute(&cli, &env, &mut stdout) {
        Ok(code) => code,
        Err(error) if broken_pipe(&error) => 0,
        Err(error) => {
            let diagnostic = fatal(&error);
            let _ = writeln!(
                io::stderr().lock(),
                "{}: {}",
                diagnostic.code,
                diagnostic.message
            );
            if cli.common.format == "json" {
                let envelope: Envelope<()> = Envelope {
                    version: 1,
                    incomplete: true,
                    diagnostics: vec![diagnostic],
                    data: None,
                };
                if let Err(error) = json(&mut stdout, &envelope)
                    && broken_pipe(&error)
                {
                    return 0;
                }
            }
            2
        }
    }
}

fn execute(cli: &Cli, env: &BTreeMap<String, String>, out: &mut dyn Write) -> Result<i32> {
    let home = env
        .get("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidArgument("HOME is required to resolve store roots".into()))?;
    let cwd = std::env::current_dir().map_err(output_error)?;
    let zone = zone(cli.common.time_zone.as_deref(), env)?;
    let index_dir = cli.common.index_dir.clone().unwrap_or_else(|| {
        env.get("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("slimetrail")
    });
    let ctx = Context {
        roots: resolve_roots(&home, env)?,
        index_dir,
        now_ms: Timestamp::now().as_millisecond(),
        cwd: cwd.clone(),
        time_zone: zone,
    };
    let filter = filter(&cli.common, env, &home, &cwd)?;
    let is_json = cli.common.format == "json";
    match &cli.command {
        Command::Search {
            query,
            regex,
            case_sensitive,
            sort,
            max_results,
            max_snippets,
            context,
        } => {
            let result = search(
                &ctx,
                &SearchOptions {
                    filter,
                    query: query.clone(),
                    regex: *regex,
                    case_sensitive: *case_sensitive,
                    sort: if sort == "score" {
                        Sort::Score
                    } else {
                        Sort::Recent
                    },
                    max_results: *max_results,
                    max_snippets: *max_snippets,
                    context_chars: *context,
                },
            )?;
            emit(
                out,
                convert(result, |v| SearchDataDto::try_from(v))?,
                is_json,
            )
        }
        Command::Read {
            session,
            mode,
            max_chars,
            offset,
        } => {
            validate_key(session, &filter.agents)?;
            let result = read(
                &ctx,
                session,
                &ReadOptions {
                    mode: match mode.as_str() {
                        "full" => ReadMode::Full,
                        "log" => ReadMode::Log,
                        _ => ReadMode::Lite,
                    },
                    max_chars: *max_chars,
                    offset: *offset,
                },
            )?;
            let result = convert(result, |v| ReadDataDto::try_from(v))?;
            if is_json {
                emit(out, result, true)
            } else {
                diagnostics(&result.diagnostics);
                if let Some(data) = result.data {
                    write!(out, "{}", data.body).map_err(output_error)?;
                    if let Some(next_offset) = data.next_offset {
                        eprintln!("Transcript truncated; continue with --offset {next_offset}");
                    }
                }
                Ok(i32::from(result.incomplete))
            }
        }
        Command::Timeline {
            since,
            until,
            idle_minutes,
        } => {
            let (since, until) = window(
                since.as_deref(),
                until.as_deref(),
                filter.days,
                ctx.now_ms,
                &TimeZone::get(&ctx.time_zone)?,
            )?;
            let idle_ms = idle_minutes
                .checked_mul(60_000)
                .ok_or_else(|| Error::InvalidArgument("idle-minutes is too large".into()))?;
            emit(
                out,
                convert(
                    timeline(
                        &ctx,
                        &TimelineOptions {
                            filter,
                            since,
                            until,
                            idle_ms,
                        },
                    )?,
                    |v| TimelineDataDto::try_from(v),
                )?,
                is_json,
            )
        }
        Command::Index => emit(out, refresh(&ctx, &filter.agents)?, is_json),
        Command::Sessions => emit(
            out,
            convert(sessions(&ctx, &filter)?, |s| {
                SessionsDataDto::try_from(s.as_slice())
            })?,
            is_json,
        ),
        Command::Entries { session } => {
            validate_key(session, &filter.agents)?;
            stream_entries(&ctx, session, out, is_json)
        }
    }
}

fn zone(explicit: Option<&str>, env: &BTreeMap<String, String>) -> Result<String> {
    let name = explicit
        .or_else(|| env.get("TZ").map(String::as_str))
        .filter(|s| !s.is_empty());
    let zone = match name {
        Some(name) => TimeZone::get(name).map_err(|_| {
            Error::InvalidArgument("invalid IANA time zone; use --time-zone Area/Location".into())
        })?,
        None => TimeZone::try_system().map_err(|_| {
            Error::InvalidArgument("cannot determine system time zone; use --time-zone".into())
        })?,
    };
    zone.iana_name().map(String::from).ok_or_else(|| {
        Error::InvalidArgument("system time zone has no IANA name; use --time-zone".into())
    })
}

fn filter(
    common: &Common,
    env: &BTreeMap<String, String>,
    home: &Path,
    cwd: &Path,
) -> Result<Filter> {
    let mut agents = [
        (common.claude, Agent::Claude),
        (common.codex, Agent::Codex),
        (common.cursor, Agent::Cursor),
        (common.droid, Agent::Droid),
        (common.opencode, Agent::Opencode),
        (common.pi, Agent::Pi),
    ]
    .into_iter()
    .filter_map(|(chosen, agent)| (chosen || common.all).then_some(agent))
    .collect::<BTreeSet<_>>();
    if common.current {
        agents.insert(
            env.get("AGENT_HARNESS")
                .ok_or_else(|| Error::InvalidArgument("--current requires AGENT_HARNESS".into()))?
                .to_lowercase()
                .parse()
                .map_err(|_| {
                    Error::InvalidArgument(
                        "AGENT_HARNESS must name claude, codex, cursor, droid, opencode, or pi"
                            .into(),
                    )
                })?,
        );
    }
    let target = common.cwd.as_deref().unwrap_or(cwd);
    let target = if target == Path::new("~") {
        home.to_path_buf()
    } else if let Ok(rest) = target.strip_prefix("~/") {
        home.join(rest)
    } else {
        target.to_path_buf()
    };
    Ok(Filter {
        agents,
        cwd: target,
        scope: match common.scope.as_str() {
            "current" => Scope::Current,
            "all" => Scope::All,
            _ => Scope::Siblings,
        },
        app: common.app.clone(),
        days: common.days,
        kinds: common
            .kind
            .iter()
            .filter_map(|kind| match kind.as_str() {
                "message" => Some(Kind::Message),
                "tool" => Some(Kind::Tool),
                "meta" => Some(Kind::Meta),
                _ => None,
            })
            .collect(),
    })
}

fn validate_key(key: &SessionKey, agents: &BTreeSet<Agent>) -> Result<()> {
    if !agents.is_empty() && !agents.contains(&key.agent) {
        return Err(Error::InvalidArgument(
            "session reference conflicts with agent selection".into(),
        ));
    }
    Ok(())
}

fn boundary(value: &str, until: bool, zone: &TimeZone) -> Result<i64> {
    if let Ok(timestamp) = value.parse::<Timestamp>() {
        return Ok(timestamp.as_millisecond());
    }
    let dt = if value.len() == 10 {
        let date = value
            .parse::<civil::Date>()
            .map_err(|_| Error::InvalidArgument("expected ISO date or datetime".into()))?;
        let date = if until { date.tomorrow()? } else { date };
        date.at(0, 0, 0, 0)
    } else {
        value
            .parse::<civil::DateTime>()
            .map_err(|_| Error::InvalidArgument("expected ISO date or datetime".into()))?
    };
    zone.to_ambiguous_zoned(dt)
        .unambiguous()
        .map(|z| z.timestamp().as_millisecond())
        .map_err(|_| {
            Error::InvalidArgument(
                "ambiguous or nonexistent local datetime; supply an explicit UTC offset".into(),
            )
        })
}

fn window(
    since: Option<&str>,
    until: Option<&str>,
    days: Option<u32>,
    now: i64,
    zone: &TimeZone,
) -> Result<(i64, i64)> {
    if since.is_some() && days.is_some() {
        return Err(Error::InvalidArgument(
            "use either --since or --days".into(),
        ));
    }
    let until = until
        .map(|v| boundary(v, true, zone))
        .transpose()?
        .unwrap_or(now);
    let since = match (since, days) {
        (Some(value), _) => boundary(value, false, zone)?,
        (_, Some(days)) if days > 0 => until
            .checked_sub(i64::from(days) * 86_400_000)
            .ok_or_else(|| Error::InvalidArgument("days is too large".into()))?,
        _ => {
            return Err(Error::InvalidArgument(
                "timeline requires --since or positive --days".into(),
            ));
        }
    };
    if since >= until {
        return Err(Error::InvalidArgument("since must precede until".into()));
    }
    Ok((since, until))
}

fn convert<T, U>(
    value: Envelope<T>,
    transform: impl FnOnce(&T) -> Result<U>,
) -> Result<Envelope<U>> {
    Ok(Envelope {
        version: 1,
        incomplete: value.incomplete,
        diagnostics: value.diagnostics,
        data: value.data.as_ref().map(transform).transpose()?,
    })
}
fn emit<T: Serialize>(out: &mut dyn Write, value: Envelope<T>, is_json: bool) -> Result<i32> {
    diagnostics(&value.diagnostics);
    if is_json {
        json(out, &value)?;
    } else if let Some(data) = &value.data {
        serde_json::to_writer_pretty(&mut *out, data)?;
        writeln!(out).map_err(output_error)?;
    }
    Ok(i32::from(value.incomplete))
}
fn json(out: &mut dyn Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    writeln!(out).map_err(output_error)
}
fn diagnostics(values: &[Diagnostic]) {
    let mut stderr = io::stderr().lock();
    for diagnostic in values {
        let identity = diagnostic
            .agent
            .map(|agent| agent.to_string())
            .into_iter()
            .chain(diagnostic.source.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(
            stderr,
            "{}: {}{}",
            diagnostic.code,
            if identity.is_empty() {
                String::new()
            } else {
                format!("[{identity}] ")
            },
            diagnostic.message
        );
    }
}
fn fatal(error: &Error) -> Diagnostic {
    Diagnostic {
        agent: None,
        source: None,
        code: "fatal".into(),
        message: error.to_string(),
    }
}
fn output_error(source: io::Error) -> Error {
    Error::Io {
        path: "<output>".into(),
        source,
    }
}
fn broken_pipe(error: &Error) -> bool {
    match error {
        Error::Io { source, .. } => source.kind() == io::ErrorKind::BrokenPipe,
        Error::Json(e) => e.io_error_kind() == Some(io::ErrorKind::BrokenPipe),
        _ => false,
    }
}

fn stream_entries(
    ctx: &Context,
    key: &SessionKey,
    out: &mut dyn Write,
    is_json: bool,
) -> Result<i32> {
    write_entries(out, is_json, |emit| entries(ctx, key, emit))
}

fn write_entries(
    out: &mut dyn Write,
    is_json: bool,
    read: impl FnOnce(&mut dyn FnMut(Entry) -> Result<()>) -> Result<Envelope<Session>>,
) -> Result<i32> {
    let mut started = false;
    let result = read(&mut |entry| {
        let dto = EntryDto::try_from(&entry)?;
        if is_json {
            if started {
                write!(out, ",").map_err(output_error)?;
            } else {
                write!(out, "{{\"version\":1,\"data\":{{\"entries\":[").map_err(output_error)?;
                started = true;
            }
            serde_json::to_writer(&mut *out, &dto)?;
        } else {
            writeln!(out, "{}", dto.text).map_err(output_error)?;
            started = true;
        }
        Ok(())
    });
    if let Err(error) = &result
        && (broken_pipe(error) || !started)
    {
        return result.map(|_| 0);
    }
    let result = result.and_then(|value| convert(value, |s| SessionDto::try_from(s)));
    let (session, incomplete, diagnostics, code) = match result {
        Ok(value) => (
            value.data,
            value.incomplete,
            value.diagnostics,
            i32::from(value.incomplete),
        ),
        Err(error) => (None, true, vec![fatal(&error)], 2),
    };
    self::diagnostics(&diagnostics);
    if is_json {
        if !started {
            write!(out, "{{\"version\":1,\"data\":{{\"entries\":[").map_err(output_error)?;
        }
        write!(out, "],\"session\":").map_err(output_error)?;
        serde_json::to_writer(&mut *out, &session)?;
        write!(out, "}},\"incomplete\":{incomplete},\"diagnostics\":").map_err(output_error)?;
        serde_json::to_writer(&mut *out, &diagnostics)?;
        writeln!(out, "}}").map_err(output_error)?;
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_dates_follow_dst_and_days_remain_elapsed_duration() {
        let zone = TimeZone::get("Europe/Berlin").unwrap();
        for (date, hours) in [("2026-03-29", 23), ("2026-10-25", 25)] {
            let (start, end) = window(Some(date), Some(date), None, 0, &zone).unwrap();
            assert_eq!(end - start, hours * 3_600_000);
            let (start, end) = window(None, Some(date), Some(1), 0, &zone).unwrap();
            assert_eq!(end - start, 86_400_000);
        }
    }

    #[test]
    fn late_entry_failure_finalizes_partial_json() {
        let mut out = Vec::new();
        let code = write_entries(&mut out, true, |emit| {
            emit(Entry {
                session: "codex:synthetic".parse().unwrap(),
                index: 0,
                source_id: "item".into(),
                timestamp: Some(0),
                timestamp_raw: None,
                role: "user".into(),
                kind: Kind::Message,
                subtype: None,
                tool_name: None,
                automated: false,
                text: "synthetic text".into(),
            })?;
            Err(Error::Sqlite(rusqlite::Error::InvalidQuery))
        })
        .unwrap();
        assert_eq!(code, 2);
        let envelope: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["data"]["entries"][0]["text"], "synthetic text");
        assert_eq!(envelope["data"]["session"], serde_json::Value::Null);
        assert_eq!(envelope["incomplete"], true);
        assert_eq!(envelope["diagnostics"][0]["code"], "fatal");
    }

    #[test]
    fn early_entry_failure_leaves_envelope_to_error_handler() {
        let mut out = Vec::new();
        let result = write_entries(&mut out, true, |_| {
            Err(Error::Sqlite(rusqlite::Error::InvalidQuery))
        });
        assert!(result.is_err());
        assert_eq!(out, b"");
    }
}
