# slimetrail: build brief

slimetrail is a standalone, read-only Rust library and CLI. It knows where six coding agents keep their supported session formats, reads them into one normalized shape, and searches them through an incremental index. External consumers use the versioned JSON CLI; the Rust library is an internal integration seam, not a stable Python import API.

Written 2026-11-16 in a ringleader session, when this checkout contained only an empty git repo. Updated during the approved v1 build to record the settled Rust/CLI scope; the source history and references below are retained.

## Why

Six local tools now read agent session files, and each one has its own copy of the format knowledge:

| Consumer | Where | What it reads |
| --- | --- | --- |
| ringleader scan / registry | `~/wrksp/ringleader/dev1/ringleader/sources/` | One record per session: cwd, title, mtime, git info |
| `rl analyze` | `ringleader/analysis/` (`claude.py`, `codex.py`) | Detailed facts (requests, tool calls, rewinds, compactions), Claude and Codex only |
| `rl usage` | `ringleader/usage/` | Tokens and cost, 5 agents (not Cursor) |
| check-agent-logs skill | `~/ac/loadout/skills/check-agent-logs/scripts/search_agent_logs.py` | Every entry, classified message/tool/meta, plus a timeline |
| time-reconstruct skill | `~/ac/loadout/skills/time-reconstruct/scripts/reconstruct.py` | Loads check-agent-logs' script through `importlib` to get session spans |
| circus desktop app | consumes `rl --json` | Indirectly, through ringleader |

check-agent-logs scans raw files on every query. An all-agent `--scope all` search took minutes on 2026-11-15. A September decision said "an index is pure liability (3.5s scan)"; that held when the store was smaller and only one or two agents were involved, and it no longer does.

No existing library covers this, as of 2026-11-15:

- `@tangle-network/harness-sessions` (npm, TypeScript) reads 6 agents but has no Cursor support and no search or index.
- ATXF (`leostera/atxf`) is a draft transcript exchange format, which is a conversion concern.
- `coding_agent_session_search` and `agent-sessions` embed their readers inside apps, with an embeddings/BM25 search stack we don't want.

Format references: `~/wrksp/ringleader/dev1/docs/reference/agent-session-management.md` and `coding-agent-session-search.md`.

## Scope for v1

1. **Discovery and session metadata.** Lift the read half of ringleader's `Source` seam (`ringleader/sources/base.py`: `Source.discover()` / `parse()`, `RawRef`, `NormalizedSession`) and the six readers: `claude_code.py`, `codex.py`, `cursor.py`, `droid.py`, `opencode.py`, `pi.py`. That's 2,161 lines in total, interleaved with write capabilities.
   - Bring their helpers too: `slug.py` (Claude's slugify and `droid_slug`), `opencode_db.py` (`connect_read_only`), and the relevant parts of `transcript.py`.
2. **Entry stream.** One normalized entry per record: session, index, timestamp, role, kind (`message` | `tool` | `meta`), text, and tool name where there is one.
   - Lift the per-agent entry readers from `search_agent_logs.py` (`claude_entries`, `codex_entries`, `droid_entries`, `pi_entries`, `cursor_entries`, `opencode_entries`, `read_jsonl_session`, `read_opencode_session`). They already handle OpenCode's SQLite store, Cursor's transcript plus chat-store split, and Cursor's missing tool output.
   - The `Entry` / `SessionMeta` dataclasses there are the starting shape.
3. **Index.** A SQLite FTS5 index over entries, incremental by file fingerprint. Follow the rules `rl analyze`'s index already proved (`ringleader/analysis/store.py`, `build.py`, `ingest.py`; `docs/tech/analysis.md`):
   - One file per schema version (`index-v<N>.sqlite3`), under `$XDG_DATA_HOME/slimetrail/` (default `~/.local/share/slimetrail/`), overridable with `--index-dir`.
   - Re-read only transcripts whose fingerprint changed. Re-read a live (still-growing) transcript whole.
   - Write under a lock. Never unlink or rename a live index file. Prune older-version files under the lock, and never touch a newer one.
   - Make deleting the index cost only a cold rebuild.
4. **CLI.**
   - `slimetrail search <query>` supports agent selectors, `--cwd` / `--app`, `--days`, `--kind`, ranking (message 3, meta 2, tool 1, title 5), and `--sort recent`.
   - `slimetrail read <agent:id>` supports `--mode lite|full|log`, `--offset`, `--max-chars`, and a truncation notice that names the next Unicode scalar character offset. JSON exposes `truncated` and `next_offset`.
   - `slimetrail timeline` supports `--since`, `--until` and `--idle-minutes`, splitting sessions into active spans.
   - `--format json` everywhere, with a `version` field. Copy check-agent-logs' current flags and output wherever they are sensible, so the skill can become a thin wrapper.
5. **Library/API boundary.** The CLI is a thin layer over Rust functions that take roots and clock as arguments. Python and other external consumers call the CLI through version-1 JSON; there is no Python import API or PyO3 binding. `sessions`, `entries`, `search`, `read`, `timeline` and explicit `index` are CLI commands; source discovery stays internal.

## Out of scope for v1

Every operation that writes to an agent's store stays in ringleader: renaming, hiding from the picker, archiving, deleting, companion-file moves, workspace relocation and rebinding, and launching or forking sessions. Those are the capability protocols in `sources/base.py` beyond `discover` / `parse`.

Also out of scope:

- The ringleader registry and classification.
- Embeddings or semantic search.
- A daemon or file watcher; indexing on each query is fine.
- Migrating ringleader, check-agent-logs or time-reconstruct onto slimetrail. That comes after v1 has shipped, one consumer at a time.

## Hard constraints

- **Read-only on agent stores.** Never write, lock, rename or delete anything under `~/.claude`, `~/.codex`, `~/.factory`, `~/.cursor`, the Cursor data dir, `~/.pi`, or OpenCode's data dir. Never open an original agent DB through SQLite, even with `mode=ro`, because sidecars may be written. Copy DB/WAL through retained read-only OS handles into validated private snapshots, compare pre/post fingerprints, and open only the private copy for WAL recovery and query-only extraction. Repeated churn or rollback-journal recovery needs make coverage incomplete.
- **Respect each agent's environment overrides** (`CURSOR_CONFIG_DIR`, `CURSOR_DATA_DIR`, `FACTORY_HOME_OVERRIDE`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `PI_CODING_AGENT_DIR`, `PI_SESSIONS_DIR`, and OpenCode's XDG dirs; ringleader's `sources/` show where each one is read). Take roots as parameters so tests never touch real stores.
- **Tests run only against synthetic fixtures.** Real transcripts contain secrets and PII, so never copy one into the repo. ringleader's `ringleader/devtools/` and the existing `test_search_agent_logs.py` show how fixtures are built.
- **The index holds secrets too.** Create it with mode 0600 inside a 0700 directory, and never log entry text.
- **Fail loudly on a missing or unreadable store.** Report the search as incomplete, naming the agent, rather than returning an empty result (check-agent-logs already does this).

## Decisions settled

- **Name:** slimetrail. Free on PyPI, npm and crates.io.
- **Language:** Rust, edition 2024, MSRV 1.89; useful dependencies are accepted, including bundled SQLite, serde, clap, regex and jiff. No unsafe code in this crate.
- **Consumers:** version-1 JSON through CLI subprocesses; Rust internal APIs are not a separate stable external promise.
- **Search:** literal case-insensitive substring by default, optional `--case-sensitive` and explicit Rust `--regex`; no fuzzy search, daemon or watcher.
- **Failures:** complete results exit 0, partial selected-source coverage exits 1 (including no usable sources), usage/fatal failures exit 2.

## Build-session decisions resolved

1. **Token usage.** Keep token/cost reading in ringleader; it is outside v1.
2. **Package and distribution.** Build a local release binary and support `cargo install --path . --locked` with an explicit prefix for validation. Publishing, Homebrew tap changes, PyPI and consumer migrations are deferred. `just build-release` builds and `just install` installs into Cargo's installation directory, replacing the same version; `CARGO_INSTALL_ROOT` selects a temporary prefix for validation. No global install is part of the build-session work.
3. **Dependencies.** Accept useful dependencies and retain Cargo.lock; macOS checks run locally and Linux has a CI recipe. A configured workflow alone does not establish Linux execution. Windows is not promised.
4. **Entry schema details.** Keep `message`, `tool`, `meta`, with subtype/tool/automation fields; reasoning and compaction use metadata subtypes. Include native child/archive sessions with parent/provenance where known. Entry indices are dense and source IDs stable on unchanged reread/append, not through arbitrary rewrites. Timeline preserves Claude/Cursor child exclusions. Cursor legacy text/protobuf and OpenCode legacy file readers are outside supported v1 formats and diagnosed as incomplete where detected.

## Definition of done

- `slimetrail search`, `read` and `timeline` match check-agent-logs' results on synthetic fixtures for all six agents under the finite, documented compatibility exceptions in [docs/compatibility.md](docs/compatibility.md).
- Record cold, warm, one-changed-source and short/non-ASCII/regex timings and counters, with no fixed latency target. Real-store measurement is local/read-only when access permits, uses a private repo-local index, retains only aggregate metrics and removes that index afterward; changed-source measurement is synthetic only.
- The index rebuilds incrementally: touching one transcript re-reads only that transcript, and a schema bump creates a new index file.
- Strict checks pass: `timeout 900 just check` runs format checks, locked all-target/all-feature Clippy with warnings denied, and locked all-target tests.
- A README covers the CLI and versioned JSON integration and lists store roots, override precedence and known format gaps. Architecture/privacy, compatibility and repeatable aggregate performance measurements have canonical docs; the original brief remains retained.

## House rules

- **Git:** the user handles git. Commit only when asked, onto the current branch.
  - Commit messages are `feat|fix|chore: subject`, with no scope and no body.
  - Agents cannot push.
- **This checkout:** `~/wrksp/slimetrail/dev1` follows the `<project>/devN` layout used for ringleader and circus.
- **Size:** the job spans several files and several hours, so `longshot` (or `plan` for a first slice) is the expected way to build it.
