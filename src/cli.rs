use clap::{
    Args, Parser, Subcommand,
    builder::{PossibleValuesParser, TypedValueParser},
};
use jiff::{Timestamp, civil, tz::TimeZone};
use serde::Serialize;
use slimetrail::{
    Admission, Agent, BuildPolicy, CaseMode, Context, Coverage, Diagnostic, Entry, Envelope, Error,
    Filter, JobEstimate, Kind, Observer, ReadMode, ReadOptions, RefreshOptions, Result, Scope,
    SearchOptions, Session, SessionKey, Sort, TimelineOptions, config,
    dto::{EntryDto, ReadDataDto, SearchDataDto, SessionDto, SessionsDataDto, TimelineDataDto},
    entries, read, refresh, resolve_roots, search, sessions, timeline,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

mod progress;
mod prompt;
mod search_text;

const INTERACTIVE_BUILD_MS: u64 = 120_000;
const BATCH_BUILD_MS: u64 = 20_000;

struct Terminal<'a> {
    tty: bool,
    stderr: bool,
    input: &'a mut dyn BufRead,
    prompt: &'a mut dyn Write,
}

struct Gate<'a> {
    config: config::Config,
    path: PathBuf,
    terminal: Terminal<'a>,
}

#[derive(Parser)]
#[command(version, about = "Read-only search across local coding-agent sessions")]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Clap models independent agent selectors as flags"
)]
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
    devin: bool,
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
        help = "Select AGENT_HARNESS (claude/codex/cursor/devin/droid/opencode/pi)"
    )]
    current: bool,
}

const CASE_FLAGS: [&str; 3] = ["ignore_case", "case_sensitive", "smart_case"];
const SYNTAX_FLAGS: [&str; 2] = ["fixed_strings", "regex"];

#[derive(Args)]
struct SearchArgs {
    query: String,
    #[command(flatten)]
    case: CaseArgs,
    #[command(flatten)]
    syntax: SyntaxArgs,
    #[arg(
        short = 'w',
        long,
        help = "Only count matches bounded by non-word characters"
    )]
    word_regexp: bool,
    #[arg(
        short = 'l',
        long,
        help = "Print only the ref of each returned session, capped by --max-results (text output)"
    )]
    refs_only: bool,
    #[arg(long, default_value = "score", value_parser = ["score", "recent"])]
    sort: String,
    #[arg(long, default_value_t = 50)]
    max_results: usize,
    #[arg(long, default_value_t = 3)]
    max_snippets: usize,
    #[arg(long, default_value_t = 160)]
    context: usize,
}

#[derive(Args)]
struct CaseArgs {
    #[arg(
        short = 'i',
        long,
        overrides_with_all = CASE_FLAGS,
        help = "Match case-insensitively"
    )]
    ignore_case: bool,
    #[arg(
        short = 's',
        long,
        overrides_with_all = CASE_FLAGS,
        help = "Match case-sensitively"
    )]
    case_sensitive: bool,
    #[arg(
        short = 'S',
        long,
        overrides_with_all = CASE_FLAGS,
        help = "Match case-sensitively only if the query has an uppercase letter (default)"
    )]
    smart_case: bool,
}

impl CaseArgs {
    const fn mode(&self) -> CaseMode {
        if self.ignore_case {
            CaseMode::Insensitive
        } else if self.case_sensitive {
            CaseMode::Sensitive
        } else {
            CaseMode::Smart
        }
    }
}

#[derive(Args)]
struct SyntaxArgs {
    #[arg(
        short = 'F',
        long,
        overrides_with_all = SYNTAX_FLAGS,
        help = "Treat the query as a literal string (default)"
    )]
    fixed_strings: bool,
    #[arg(
        short = 'E',
        long,
        overrides_with_all = SYNTAX_FLAGS,
        help = "Treat the query as a regular expression"
    )]
    regex: bool,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Search session titles and entries")]
    Search(SearchArgs),
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
    Index {
        #[arg(
            long,
            value_parser = PossibleValuesParser::new(["compact", "full"]).try_map(|v| v.parse::<Coverage>()),
            help = "Set search coverage in the config file and rebuild the search index to match"
        )]
        coverage: Option<Coverage>,
    },
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

pub(crate) fn run() -> i32 {
    let cli = Cli::parse();
    let env = std::env::vars().collect::<BTreeMap<_, _>>();
    let mut stdout = io::stdout().lock();
    let stderr = io::stderr().is_terminal();
    let terminal = Terminal {
        tty: io::stdin().is_terminal() && stderr,
        stderr,
        input: &mut io::stdin().lock(),
        prompt: &mut io::stderr(),
    };
    match execute(&cli, &env, build_limit(&env), &mut stdout, terminal) {
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

#[expect(clippy::too_many_lines, reason = "Keep command dispatch together")]
fn execute(
    cli: &Cli,
    env: &BTreeMap<String, String>,
    build_limit: Option<u64>,
    out: &mut dyn Write,
    terminal: Terminal<'_>,
) -> Result<i32> {
    let home = env
        .get("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidArgument("HOME is required to resolve store roots".into()))?;
    let cwd = std::env::current_dir().map_err(output_error)?;
    let time_zone = zone(cli.common.time_zone.as_deref(), env)?;
    let config_path = config::path(&home, env);
    let config = configure(&cli.command, &config_path)?;
    let is_json = cli.common.format == "json";
    let indexing = matches!(cli.command, Command::Index { .. });
    let interactive = !indexing && !is_json && !env.contains_key("CI") && terminal.tty;
    let ctx = context(&cli.common, env, &home, &cwd, time_zone, config)?;
    let stderr = progress::Stderr::new(
        indexing && terminal.stderr,
        if indexing {
            terminal.stderr
        } else {
            interactive
        },
    );
    let refreshing = refresh_options(indexing, interactive, build_limit, &stderr);
    let mut gate = Gate {
        config,
        path: config_path,
        terminal,
    };
    let filter = filter(&cli.common, env, &home, &cwd)?;
    match &cli.command {
        Command::Search(args) => {
            run_search(&ctx, refreshing, &mut gate, filter, args, is_json, out)
        }
        Command::Read {
            session,
            mode,
            max_chars,
            offset,
        } => {
            validate_key(session, &filter.agents)?;
            let options = ReadOptions {
                mode: match mode.as_str() {
                    "full" => ReadMode::Full,
                    "log" => ReadMode::Log,
                    _ => ReadMode::Lite,
                },
                max_chars: *max_chars,
                offset: *offset,
            };
            let result = gated(&ctx, refreshing, &mut gate, |ctx, refreshing| {
                convert(read(ctx, session, &options, refreshing)?, |v| {
                    ReadDataDto::try_from(v)
                })
            })?;
            if is_json {
                emit(out, &result, true)
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
            let options = TimelineOptions {
                filter,
                since,
                until,
                idle_ms,
            };
            let result = gated(&ctx, refreshing, &mut gate, |ctx, refreshing| {
                convert(timeline(ctx, &options, refreshing)?, |v| {
                    TimelineDataDto::try_from(v)
                })
            })?;
            emit(out, &result, is_json)
        }
        Command::Index { .. } => emit(out, &refresh(&ctx, &filter.agents, &refreshing)?, is_json),
        Command::Sessions => {
            let result = gated(&ctx, refreshing, &mut gate, |ctx, refreshing| {
                convert(sessions(ctx, &filter, refreshing)?, |s| {
                    SessionsDataDto::try_from(s.as_slice())
                })
            })?;
            emit(out, &result, is_json)
        }
        Command::Entries { session } => {
            validate_key(session, &filter.agents)?;
            stream_entries(&ctx, refreshing, &mut gate, session, out, is_json)
        }
    }
}

#[cfg(feature = "test-hooks")]
fn build_limit(env: &BTreeMap<String, String>) -> Option<u64> {
    env.get("SLIMETRAIL_TEST_BUILD_LIMIT_MS")?.parse().ok()
}

#[cfg(not(feature = "test-hooks"))]
const fn build_limit(_: &BTreeMap<String, String>) -> Option<u64> {
    None
}

fn configure(command: &Command, path: &Path) -> Result<config::Config> {
    if let Command::Index {
        coverage: Some(coverage),
    } = command
    {
        config::write_coverage(path, *coverage)?;
        return Ok(config::Config {
            coverage: *coverage,
            explicit: true,
        });
    }
    config::load(path)
}

fn context(
    common: &Common,
    env: &BTreeMap<String, String>,
    home: &Path,
    cwd: &Path,
    time_zone: String,
    config: config::Config,
) -> Result<Context> {
    let index_dir = common.index_dir.clone().unwrap_or_else(|| {
        env.get("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map_or_else(|| home.join(".local/share"), PathBuf::from)
            .join("slimetrail")
    });
    Ok(Context::new(
        resolve_roots(home, env)?,
        index_dir,
        cwd.to_path_buf(),
        Timestamp::now().as_millisecond(),
        time_zone,
    )
    .with_coverage(config.coverage, config.explicit))
}

fn refresh_options(
    indexing: bool,
    interactive: bool,
    build_limit: Option<u64>,
    observer: &dyn Observer,
) -> RefreshOptions<'_> {
    if indexing {
        return RefreshOptions {
            observer: Some(observer),
            ..RefreshOptions::FULL
        };
    }
    let (max_ms, admission) = if interactive {
        (INTERACTIVE_BUILD_MS, Admission::AllOrNothing)
    } else {
        (BATCH_BUILD_MS, Admission::PerAgent)
    };
    RefreshOptions {
        build: BuildPolicy::Limit {
            max_ms: build_limit.unwrap_or(max_ms),
            admission,
        },
        observer: Some(observer),
        ..RefreshOptions::QUERY
    }
}

fn rerun<'a>(
    ctx: &Context,
    refreshing: RefreshOptions<'a>,
    gate: &mut Gate<'_>,
    estimate: &JobEstimate,
) -> Result<(Context, RefreshOptions<'a>)> {
    let limit_ms = match refreshing.build {
        BuildPolicy::Limit { max_ms, .. } => Some(max_ms),
        BuildPolicy::Inline => None,
    };
    let choice = prompt::ask(
        estimate,
        gate.config,
        limit_ms,
        gate.terminal.input,
        gate.terminal.prompt,
    )?;
    let Some(choice) = choice else {
        let build = match refreshing.build {
            BuildPolicy::Limit { max_ms, .. } => BuildPolicy::Limit {
                max_ms,
                admission: Admission::PerAgent,
            },
            BuildPolicy::Inline => BuildPolicy::Inline,
        };
        return Ok((
            ctx.clone(),
            RefreshOptions {
                build,
                ..refreshing
            },
        ));
    };
    if choice.save {
        config::write_coverage(&gate.path, choice.coverage)?;
    }
    let locked = estimate.coverage_locked;
    let ctx = ctx.clone().with_coverage(
        if locked {
            ctx.coverage
        } else {
            choice.coverage
        },
        ctx.coverage_explicit || !locked,
    );
    let refreshing = RefreshOptions {
        build: BuildPolicy::Inline,
        ..refreshing
    };
    Ok((ctx, refreshing))
}

fn gated<T>(
    ctx: &Context,
    refreshing: RefreshOptions<'_>,
    gate: &mut Gate<'_>,
    run: impl Fn(&Context, &RefreshOptions<'_>) -> Result<Envelope<T>>,
) -> Result<Envelope<T>> {
    match run(ctx, &refreshing) {
        Err(Error::IndexRequired(estimate)) => {
            let (ctx, refreshing) = rerun(ctx, refreshing, gate, &estimate)?;
            run(&ctx, &refreshing)
        }
        other => other,
    }
}

fn run_search(
    ctx: &Context,
    refreshing: RefreshOptions<'_>,
    gate: &mut Gate<'_>,
    filter: Filter,
    args: &SearchArgs,
    is_json: bool,
    out: &mut dyn Write,
) -> Result<i32> {
    if args.refs_only && is_json {
        return Err(Error::InvalidArgument(
            "--refs-only applies to text output; JSON results already include ref".into(),
        ));
    }
    let options = SearchOptions {
        filter,
        query: args.query.clone(),
        regex: args.syntax.regex,
        case_mode: args.case.mode(),
        whole_word: args.word_regexp,
        sort: if args.sort == "score" {
            Sort::Score
        } else {
            Sort::Recent
        },
        max_results: args.max_results,
        max_snippets: if args.refs_only { 0 } else { args.max_snippets },
        context_chars: args.context,
    };
    let result = gated(ctx, refreshing, gate, |ctx, refreshing| {
        convert(search(ctx, &options, refreshing)?, |v| {
            SearchDataDto::try_from(v)
        })
    })?;
    if is_json {
        return emit(out, &result, true);
    }
    diagnostics(&result.diagnostics);
    if args.refs_only {
        search_text::render_refs(out, &result)
    } else {
        search_text::render(out, &result)
    }
    .map_err(output_error)?;
    Ok(i32::from(result.incomplete))
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
        (common.devin, Agent::Devin),
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
                        "AGENT_HARNESS must name claude, codex, cursor, devin, droid, opencode, or pi"
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
    zone.to_ambiguous_zoned_datetime(dt)
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
fn emit<T: Serialize>(out: &mut dyn Write, value: &Envelope<T>, is_json: bool) -> Result<i32> {
    diagnostics(&value.diagnostics);
    if is_json {
        json(out, value)?;
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
    refreshing: RefreshOptions<'_>,
    gate: &mut Gate<'_>,
    key: &SessionKey,
    out: &mut dyn Write,
    is_json: bool,
) -> Result<i32> {
    let (ctx, refreshing) =
        match write_entries(out, is_json, |emit| entries(ctx, key, &refreshing, emit)) {
            Err(Error::IndexRequired(estimate)) => rerun(ctx, refreshing, gate, &estimate)?,
            other => return other,
        };
    write_entries(out, is_json, |emit| entries(&ctx, key, &refreshing, emit))
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
    use slimetrail::index;
    use std::fs;

    struct Run {
        code: i32,
        stdout: String,
        prompt: String,
    }

    fn codex(root: &Path, text: &str) {
        let records = [
            serde_json::json!({"type":"session_meta","payload":{"id":"sample","cwd":"/fixture/app"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-10-25T00:30:00Z","payload":{"type":"message","role":"user","content":text}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-10-25T00:40:00Z","payload":{"type":"message","role":"assistant","content":"Second reply"}}),
        ];
        let path = root.join("codex/sessions/sample.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            records
                .iter()
                .map(|r| r.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
    }

    fn invoke(root: &Path, args: &[&str], limit: u64, answer: &str) -> Run {
        let index_dir = root.join("index");
        let cli = Cli::try_parse_from(
            ["slimetrail", "--index-dir", index_dir.to_str().unwrap()]
                .into_iter()
                .chain(["--scope", "all"])
                .chain(args.iter().copied()),
        )
        .unwrap();
        let path = |name: &str| root.join(name).to_string_lossy().into_owned();
        let env = BTreeMap::from([
            ("HOME".into(), path("")),
            ("CODEX_HOME".into(), path("codex")),
            ("XDG_DATA_HOME".into(), path("data")),
            ("SLIMETRAIL_CONFIG".into(), path("config.toml")),
            ("TZ".into(), "UTC".into()),
        ]);
        let (mut stdout, mut prompt) = (Vec::new(), Vec::new());
        let code = execute(
            &cli,
            &env,
            Some(limit),
            &mut stdout,
            Terminal {
                tty: true,
                stderr: false,
                input: &mut answer.as_bytes(),
                prompt: &mut prompt,
            },
        )
        .unwrap();
        Run {
            code,
            stdout: String::from_utf8(stdout).unwrap(),
            prompt: String::from_utf8(prompt).unwrap(),
        }
    }

    fn settings<T: rusqlite::types::FromSql>(root: &Path, column: &str) -> T {
        rusqlite::Connection::open(root.join("index").join(index::DATABASE))
            .unwrap()
            .query_row(&format!("SELECT {column} FROM settings"), [], |r| r.get(0))
            .unwrap()
    }

    fn stored_coverage(root: &Path) -> String {
        settings(root, "coverage")
    }

    fn published_then_changed(root: &Path) {
        codex(root, "Published needle");
        assert_eq!(invoke(root, &["--codex", "index"], 0, "").code, 0);
        codex(root, "Changed needle that is not indexed yet");
    }

    const SEARCH: [&str; 3] = ["--codex", "search", "needle"];

    #[cfg(feature = "test-hooks")]
    fn only_pi_fits(root: &Path, limit: u64) {
        let ctx = Context::new(
            resolve_roots(root, &BTreeMap::new()).unwrap(),
            root.join("index"),
            root.into(),
            0,
            "UTC".into(),
        );
        let estimate = index::IndexSession::open(&ctx)
            .unwrap()
            .estimate(&BTreeSet::from([Agent::Claude, Agent::Pi]))
            .unwrap();
        let millis = |agent| {
            estimate
                .agents
                .iter()
                .find(|a| a.agent == agent)
                .unwrap()
                .compact
                .millis
        };
        assert!(millis(Agent::Pi) <= limit && millis(Agent::Claude) > limit);
    }

    #[test]
    fn interactive_query_with_nothing_to_build_runs_once_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        codex(root, "Published needle");
        assert_eq!(invoke(root, &["--codex", "index"], 0, "").code, 0);
        let before: i64 = settings(root, "refresh_seq");
        #[cfg(feature = "test-hooks")]
        let discovered = index::discoveries();
        let run = invoke(root, &SEARCH, 0, "y\n");
        #[cfg(feature = "test-hooks")]
        assert_eq!(index::discoveries() - discovered, 1);
        assert_eq!(run.code, 0);
        assert_eq!(run.prompt, "");
        assert_eq!(settings::<i64>(root, "refresh_seq"), before + 1);
        assert!(
            run.stdout.starts_with("Returned sessions: 1\n"),
            "{}",
            run.stdout
        );
    }

    #[test]
    fn interactive_query_while_another_process_writes_answers_stale_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        published_then_changed(root);
        let lock = fs::File::options()
            .read(true)
            .write(true)
            .open(root.join("index/writer.lock"))
            .unwrap();
        lock.lock().unwrap();
        let started = std::time::Instant::now();
        let run = invoke(root, &SEARCH, 0, "c\n");
        assert!(started.elapsed() >= std::time::Duration::from_secs(5));
        drop(lock);
        assert_eq!(run.code, 1);
        assert_eq!(run.prompt, "");
        assert!(run.stdout.contains("Published needle"), "{}", run.stdout);
        assert!(!root.join("config.toml").exists());
    }

    #[test]
    fn interactive_query_during_a_pending_rebuild_answers_stale_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        published_then_changed(root);
        rusqlite::Connection::open(root.join("index").join(index::DATABASE))
            .unwrap()
            .execute("UPDATE settings SET coverage_pending='full'", [])
            .unwrap();
        let run = invoke(root, &SEARCH, 0, "f\n");
        assert_eq!(run.code, 1);
        assert_eq!(run.prompt, "");
        assert!(run.stdout.contains("Published needle"), "{}", run.stdout);
        assert!(!root.join("config.toml").exists());
        assert_eq!(settings::<String>(root, "coverage_pending"), "full");
    }

    #[test]
    fn interactive_first_build_takes_the_chosen_coverage_when_only_small_agents_fit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pi = root.join(".pi/agent/sessions/project/session.jsonl");
        fs::create_dir_all(pi.parent().unwrap()).unwrap();
        fs::write(&pi, include_str!("../tests/fixtures/jsonl/pi.jsonl")).unwrap();
        let claude = root.join(".claude/projects/app/large.jsonl");
        fs::create_dir_all(claude.parent().unwrap()).unwrap();
        let text = "large backlog text ".repeat(100_000);
        fs::write(
            &claude,
            serde_json::json!({"type":"user","sessionId":"large","cwd":"/fixture/app","message":{"content":text}}).to_string() + "\n",
        )
        .unwrap();
        #[cfg(feature = "test-hooks")]
        only_pi_fits(root, 20);
        let run = invoke(root, &["--claude", "--pi", "sessions"], 20, "f\n");
        assert_eq!(run.code, 0, "{}", run.prompt);
        assert!(run.prompt.contains("\n  compact "), "{}", run.prompt);
        assert!(run.prompt.contains("\n  full "), "{}", run.prompt);
        assert!(
            run.prompt
                .ends_with("Build now? [c]ompact or [y]es / [f]ull / [N]o: ")
        );
        assert_eq!(stored_coverage(root), "full");
        assert_eq!(
            fs::read_to_string(root.join("config.toml")).unwrap(),
            "coverage = \"full\"\n"
        );
        let data: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
        assert_eq!(data["count"], 2);
    }

    #[test]
    fn interactive_prompt_on_a_used_index_offers_only_its_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        published_then_changed(root);
        fs::write(root.join("config.toml"), "coverage = \"full\"\n").unwrap();
        let declined = invoke(root, &SEARCH, 0, "\n");
        assert_eq!(declined.code, 1);
        let lines: Vec<_> = declined.prompt.lines().collect();
        assert_eq!(lines.len(), 5, "{}", declined.prompt);
        assert!(lines[1].starts_with("  compact "), "{}", lines[1]);
        assert_eq!(
            lines[2],
            "The config asks for full coverage but the index is compact; run `slimetrail index` to rebuild it."
        );
        assert_eq!(
            lines[3],
            "No builds only what fits in 1 s; `slimetrail index` builds the rest."
        );
        assert_eq!(lines[4], "Build now? [y/N]: ");
        assert_eq!(
            declined
                .stdout
                .matches("Returned sessions: 1 (search incomplete)\n")
                .count(),
            1,
            "{}",
            declined.stdout
        );
        assert!(
            declined.stdout.contains("Published needle"),
            "{}",
            declined.stdout
        );
        let accepted = invoke(root, &SEARCH, 0, "y\n");
        assert_eq!(accepted.code, 0);
        assert!(
            accepted.stdout.contains("Changed needle"),
            "{}",
            accepted.stdout
        );
        assert_eq!(stored_coverage(root), "compact");
        assert_eq!(
            fs::read_to_string(root.join("config.toml")).unwrap(),
            "coverage = \"full\"\n"
        );
    }

    #[test]
    fn interactive_entries_stream_once_after_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        codex(root, "Gated needle");
        let entries = ["entries", "codex:sample"];
        let built = invoke(root, &entries, 0, "c\n");
        assert_eq!(built.code, 0);
        assert!(built.prompt.contains("[c]ompact"), "{}", built.prompt);
        assert_eq!(built.stdout, "Gated needle\nSecond reply\n");
        codex(root, "Changed text that is not indexed yet");
        let declined = invoke(root, &entries, 0, "n\n");
        assert_eq!(declined.code, 1);
        assert!(declined.prompt.ends_with("Build now? [y/N]: "));
        assert_eq!(declined.stdout, "Gated needle\nSecond reply\n");
        let accepted = invoke(root, &entries, 0, "y\n");
        assert_eq!(accepted.code, 0);
        assert_eq!(
            accepted.stdout,
            "Changed text that is not indexed yet\nSecond reply\n"
        );
    }

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
                searchable: slimetrail::Searchable::Text,
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
