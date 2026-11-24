# slimetrail

Read and search local coding-agent sessions from Claude Code, Codex, Cursor, Devin, Droid, OpenCode and Pi. A Rust CLI normalizes their supported formats into a private incremental SQLite index. Agent stores are read-only. Each query first refreshes the selected agents, building inline only as much as fits an estimated time limit; larger builds are left to `slimetrail index` (see [stores and index](#stores-and-index)).

## Build and install

Rust 1.89 or newer is required (edition 2024); development uses the toolchain pinned in `rust-toolchain.toml`. SQLite is bundled. The implementation uses Unix filesystem permissions and locks; CI runs on macOS and Linux. Windows is not supported. Release automation builds macOS/Linux binaries; Homebrew distribution is separate.

```sh
timeout 900 just build
./target/release/slimetrail --help
timeout 900 just install
```

`just install` installs into Cargo's installation directory (normally `~/.cargo/bin`) and replaces an existing binary even when the package version is unchanged. Add that directory to `PATH`. For a custom prefix, use `CARGO_INSTALL_ROOT=/path/to/prefix timeout 900 just install`; the binary goes in that prefix's `bin`. Use the same environment override with `just uninstall` to remove it. The shell examples require `timeout`; install coreutils if it is unavailable.

Run `just` to list commands. `just setup` installs pinned analysis tools and Git hooks, fetches locked dependencies and runs `just doctor`. `just check` runs formatting, lint, tests and a secret scan; `just check-all` adds dependency, workflow and documentation checks. `just build` builds the host binary; `just build-release` assembles cross-platform release artifacts. See [development and releases](docs/development-and-releases.md) for prerequisites, coverage, benchmarks and `just release`.

## Use

```sh
slimetrail search 'connection refused' --scope all
slimetrail search -i 'Result<T>' --codex
slimetrail search -w rust --scope all
slimetrail search 'timeout|retry' --regex --claude --pi --days 7
slimetrail search 'migration' --app slimetrail --kind message,meta --sort recent
slimetrail search -l 'flaky test' --scope all | head -n 1 | xargs slimetrail read --mode log
slimetrail sessions --scope all --format json
slimetrail read codex:SESSION_ID --mode log --max-chars 4000 --format json
slimetrail read codex:SESSION_ID --mode log --offset 4000 --max-chars 4000 --format json
slimetrail entries codex:SESSION_ID --format json
slimetrail timeline --since 2026-11-01 --until 2026-11-15 --scope all --time-zone Europe/Berlin
slimetrail index --all --format json
```

Use a returned `ref` as the exact `agent:id` for `read` or `entries`. `read` defaults to messages (`lite`); `full` includes metadata/reasoning and `log` includes tools too. JSON read responses expose `next_offset`; offsets count Unicode scalar values, not bytes. Text read stdout contains only the requested transcript slice. A truncated text page prints `Transcript truncated; continue with --offset N` to stderr; the final page prints no continuation notice. Keep the same read mode when continuing.

No selector means all seven agents. Named selectors combine; `--all` selects all; `--current` adds the agent named by `AGENT_HARNESS` and requires one of `claude`, `codex`, `cursor`, `devin`, `droid`, `opencode`, `pi`. Missing or inaccessible selected stores make a response incomplete; stderr identifies their agent and source. Unselected unavailable stores do not abort a healthy selection. Select the agents installed on your machine to avoid those diagnostics.

Search defaults to **literal substring** matching with smart case: a query without uppercase letters matches case-insensitively, and one containing an uppercase letter matches case-sensitively; with `--regex` only literal characters count, so `\W` stays case-insensitive and `\x41` is case-sensitive. `-i`/`--ignore-case` and `-s`/`--case-sensitive` force either mode, `-S`/`--smart-case` selects the default explicitly, and the last of the three wins. `-F`/`--fixed-strings` selects literal matching and `--regex` opts into Rust regular expressions (the last of the two wins); look-around and backreferences are unsupported. `-w`/`--word-regexp` counts only matches whose neighbouring characters are not word characters, so `-w rust` skips `trust` and `rust_` while `-w 'Result<T>'` still matches. `-l`/`--refs-only` prints only the returned refs. Put `--` before a query that starts with `-`, as in `slimetrail search -w -- '-foo'`. There is no fuzzy or semantic search. Results are grouped by session and ranked by non-overlapping occurrences: title 5, message 3, metadata 2, tool 1. Only native titles contribute title matches; a title derived from a prompt is display-only. See [search and compatibility details](docs/compatibility.md).

The default scope is `siblings`: the current directory and sessions whose cwd has the same parent. `--scope current` requires the same cwd; `--scope all` includes sessions with unknown cwd. `--cwd` changes the target directory, with `~` expanded at the CLI boundary. `--app` is a case-insensitive substring of stored cwd and overrides scope selection. Search and sessions `--days` filters by source freshness, not conversation timestamps.

Search defaults: `--max-results 50`, `--max-snippets 3`, `--context 160`, `--sort score`, `--kind all`. Read defaults: `--mode lite`, `--offset 0`, `--max-chars 200000`. Timeline requires `--since` or positive `--days`, accepts `--until`, and defaults to `--idle-minutes 30`. Global flags are accepted around the subcommand, but cwd/app/date/kind filters do not narrow an explicit `read`/`entries` reference; agent selectors still must agree with that reference. `index` refreshes selected agents without applying those query filters.

Search text output shows one block per returned session: its full reference, available title and cwd, occurrence count, score and labelled snippets. The header counts returned sessions after the result limit. Empty searches say `No matches.`; partial searches explicitly say `search incomplete`, including when no matches were found in available sources. Terminal controls and bidirectional formatting controls are escaped for display, with ordinary Unicode preserved. With `-l`/`--refs-only`, stdout holds only each returned session's ref on its own line in result order, escaped the same way, with no header and nothing at all when there are no matches; stderr diagnostics and exit codes are unchanged, and it cannot be combined with `--format json`. Use `--format json` for automation and original unescaped field values; its version-1 protocol is unchanged.

## Stores and index

The default index is `$XDG_DATA_HOME/slimetrail`, falling back to `~/.local/share/slimetrail`. `--index-dir PATH` overrides it. The index contains transcript text and should be treated as sensitive: its directory must be mode 0700, files 0600. Existing insecure destinations, symlink destinations and overlap with source roots are rejected. The current private index is v3 (`index-v3.sqlite3`). A v2 index from slimetrail 0.1.0 is kept until v3 holds published sessions, so an older binary still finds it, and is then pruned once no other slimetrail process is running. The first v3 build happens inline when the stores are small; otherwise queries report `index_required` (or prompt on a terminal) until `slimetrail index` runs. CLI JSON remains version 1. The database runs in WAL mode, so `index-v3.sqlite3-wal` and `index-v3.sqlite3-shm` sidecars sit beside it. Concurrent slimetrail processes share an index: reads run side by side and one process at a time refreshes. A query that waits more than 5 s for another refresh answers from the last published index with exit 1 and a `refresh_in_progress` diagnostic; `slimetrail index` waits for the other refresh to finish. Stop all slimetrail processes before manually deleting an index; the same size gate then applies to the rebuild.

Queries refresh the index before answering, but only up to an estimated cost. Before indexing any session, a query estimates how long each selected agent's pending work will take, from its source bytes and the measured or learned indexing rate, and admits agents smallest first while the total stays within the limit. Agents over the limit are left alone. An agent with an earlier build answers from that build; one without contributes nothing. Each gets an `index_required` diagnostic and the query exits 1. When nothing selected has data to answer from (no admitted agent has pending sessions and no selected agent was built before), the result is the command's empty data shape with those diagnostics. The limit is 20 s for scripts, `--format json` and runs with `CI` set. It is 120 s at a terminal (stdin and stderr are TTYs), where admitted work shows a progress line on stderr. At a terminal, the command runs once and answers directly when every selected agent's pending work fits the limit together. If their combined pending work is over it, the command stops before indexing anything or printing a result and asks, showing the session count, log size and projected index size and time. If no agent has been built yet and the config does not set `coverage`, the prompt offers `compact` or `full` (`y` picks compact) and saves the choice. Otherwise it shows only the coverage the index will use, and, when the config asks for a different one, says so and that `slimetrail index` rebuilds it. Answering yes runs the command again, building inline with progress; no, an empty answer or end of input runs it again under the 120 s limit as described above, building only the agents that fit, with `index_required` for the agents left out. A query that finds another process refreshing, or an unfinished coverage rebuild, never prompts; it answers from the last published index with exit 1. `slimetrail index` always builds everything selected, whatever the estimate; when stderr is a terminal it shows progress (including a coverage rebuild) and says when it is waiting for another refresh. On a large first run, start with `slimetrail index`.

Environment precedence is highest first; empty strings are ignored, and Cursor overrides also trim whitespace. `~` below describes the captured `HOME`; native environment path values are used as paths, not shell-expanded strings.

| Store | Root precedence |
| --- | --- |
| Claude projects | `SESSION_TRACKER_PROJECTS_DIR` → `CLAUDE_CONFIG_DIR/projects` → `~/.claude/projects` |
| Codex sessions | `CODEX_SESSIONS_DIR` → `CODEX_HOME/sessions` → `~/.codex/sessions`; plain `.jsonl` and compressed `.jsonl.zst` rollouts |
| Codex native archive | `CODEX_HOME/archived_sessions` → `~/.codex/archived_sessions` |
| Codex title index | Sibling `session_index.jsonl` of `CODEX_SESSIONS_DIR` when set; otherwise `CODEX_HOME/session_index.jsonl` → `~/.codex/session_index.jsonl` |
| Cursor transcripts | `CURSOR_DATA_DIR/projects` → `~/.cursor/projects` |
| Cursor chats | `CURSOR_CONFIG_DIR/chats` → `XDG_CONFIG_HOME/cursor/chats` → `~/.cursor/chats` |
| Devin SQLite | `DEVIN_SESSIONS_DIR/cli/sessions.db` → `XDG_DATA_HOME/devin/cli/sessions.db` → `~/.local/share/devin/cli/sessions.db` |
| Droid | `(FACTORY_HOME_OVERRIDE or HOME)/.factory/sessions` |
| Pi | `PI_SESSIONS_DIR` → `PI_CODING_AGENT_SESSION_DIR` → `PI_CODING_AGENT_DIR/sessions` → `~/.pi/agent/sessions` |
| OpenCode SQLite | `OPENCODE_DB` → `XDG_DATA_HOME/opencode/opencode.db` → `~/.local/share/opencode/opencode.db` |
| Legacy OpenCode detection | `(XDG_DATA_HOME or ~/.local/share)/opencode/storage` |
| Optional ringleader archive | `(XDG_DATA_HOME or ~/.local/share)/ringleader/archive`; recognized Claude, Codex, Cursor, Droid and Pi transcripts, each read as its own agent's archived session |

All effective roots, including `codex_session_index`, archive and legacy paths, form an index namespace. Changing overrides cannot reuse another namespace's cached rows. Old namespaces remain until the index is removed. Droid companion settings participate in invalidation, and a Codex session is invalidated by a change to its own title in the title index, not by other threads' entries. A grown Claude, Codex, Droid or Pi transcript parses only its new tail; Devin reparses only sessions whose change key moved; OpenCode still invalidates on any database change, which makes a large, active OpenCode database expensive to keep fresh (see [migration checks](docs/integrations.md#migration-checks)). Even an unchanged OpenCode or Devin database is privately snapshotted and enumerated to prove inventory completeness.

Search coverage is set in `config.toml`. Its location is `SLIMETRAIL_CONFIG` if set, otherwise `$XDG_CONFIG_HOME/slimetrail/config.toml`, falling back to `~/.config/slimetrail/config.toml`. The file accepts one key, `coverage = "compact"` (the default) or `"full"`. Any other key, an invalid value or invalid TOML is a fatal exit 2 that names the file. `compact` skips reasoning and searches only the last 2 KB of each tool result; everything stays readable. `full` searches everything except Claude `toolUseResult` text that duplicates the same record's tool result. `slimetrail index --coverage full` (or `compact`) writes the config (replacing an invalid file, and dropping any comments in it; a symlinked config is written through to its target) and rebuilds only the search documents, without reparsing transcripts. Queries keep using the index's coverage until `slimetrail index` reconciles it. When the config does not set `coverage`, compact searches carry an informational `search_coverage` diagnostic; when the configured coverage (compact if unset) differs from the index's, searches carry an informational `coverage_mismatch`. Neither changes the exit status. See [coverage details](docs/compatibility.md#search-and-read).

## Integrate

The external interface is the CLI's [version-1 JSON protocol](docs/json-protocol.md), including exit status and incomplete diagnostics. Python consumers should invoke the binary with an argument array and parse JSON. There is no Python import API or PyO3 binding. Rust modules expose functions for the CLI and internal integration, but are not a separately stable external API promise.

Exit **0** means complete (including no matches), **1** means partial source coverage, and **2** means usage or fatal runtime failure. JSON stdout is machine-readable; diagnostics also go to stderr. A result with `incomplete: true` can contain useful data, or no data if the requested session could not be established.

Read [architecture and privacy](docs/architecture.md), [supported formats and finite compatibility differences](docs/compatibility.md), [consumer boundaries and migration references](docs/integrations.md), and [performance limitations and measurements](docs/performance.md). Token usage/cost, native-store mutations, consumer migrations and a daemon are outside v1.
