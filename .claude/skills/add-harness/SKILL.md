---
name: add-harness
description: Add support for a new coding-agent harness (e.g. Gemini CLI, Amp, Goose, Kiro, Crush) to slimetrail - research its docs and source for the session store, survey existing local logs without exposing transcript text, have the user install it and run sessions, run marker-tagged probe sessions to see how subagents, forks, compaction, resume, titles and injected content appear in the logs, then implement the reader, tests, fixtures and docs. Use when the user says "add harness", "add support for X", "support another agent", "slimetrail should read X sessions", or "add a new agent store". Do NOT use for fixing a bug in an existing reader or for adding a harness to ringleader or check-agent-logs.
---

# Add a harness to slimetrail

The goal is a reader whose behavior rests on observed records, not on guesses from docs. Work happens in four stages: **research → observe → probe → implement**. Do not start implementing until the notes file answers every probe row (or records it as unsupported with a citation).

Run everything from the slimetrail repo root, with:

```bash
SURVEY=.claude/skills/add-harness/scripts/survey.py
NOTES=.local/harness-research/AGENT     # AGENT = the new agent id; .local/ is gitignored
mkdir -m 700 -p "$NOTES"
```

## Privacy rules (apply throughout)

- Agent stores are read-only. Never modify, move or delete native files, even probe sessions; tell the user what probe sessions exist and let them decide.
- Never open a native SQLite database through SQLite, even read-only — it can create sidecars. `survey.py` copies the DB and WAL into a private temp directory first; do the same for any ad-hoc query.
- Never print raw transcript text into the conversation. Use `survey.py`, which shows structure, types, counts and enum-like values only. `--record` redacts free text to lengths.
- Notes, survey output and any copies stay under `.local/`. Fixtures are synthetic: mirror the observed structure with invented text. Never commit or paste a real record.

## Instructions

### Step 1: Pin down the harness

Ask the user (in one message) for anything not already given:
- Harness name and CLI binary.
- The agent id: one lowercase word used for `--AGENT`, `AGENT_HARNESS` and the `AGENT:ID` ref prefix. Suggest one.
- Whether it is installed and has been used on this machine.

Record the answers and `CLI --version` in `$NOTES/notes.md`.

### Step 2: Research docs and source

1. Check prior art first: `grep -ril HARNESS ~/wrksp/ringleader/dev1/docs/reference/ ~/.claude/skills/check-agent-logs/scripts/search_agent_logs.py`. `agent-session-management.md` may already describe the store.
2. Load `research-tech` for the docs search. Look for: session storage location and every env var that overrides it, file format (JSONL, JSON, SQLite, other), headless/non-interactive mode, resume/continue, fork/branch, compaction, subagents/tasks, session naming, archive/delete.
3. Search GitHub for other tools that already parse this harness's sessions (session viewers, checkpointing tools, trajectory converters): `gh search code "HARNESS" transcripts`, `gh search issues "HARNESS session" transcript`. Their parsers, tests and bug trackers often document the schema, write timing and storage quirks more precisely than official docs. Treat their env-var conventions as theirs until confirmed.
4. If the harness is open source, read the source — it is authoritative over docs. Use `gh` (`gh repo view`, then `gh api --method GET repos/OWNER/REPO/contents/PATH -H "Accept: application/vnd.github.raw"`). Find the session writer, the record type definitions (TS types, Rust structs, Pydantic models), the store-path resolution including env overrides, and how subagent/fork/compaction write their records.
5. Write a "Docs/source" section in the notes: store root precedence (in the README's table form), format, record types claimed, feature support, with links or file paths for each claim.

Success: notes name a store root, its overrides, the format and which probe features exist.

### Step 3: Survey existing local logs

Look for the store at the documented root and the usual candidates: `~/.NAME`, `~/.config/NAME`, `~/.local/share/NAME`, `~/Library/Application Support/NAME`, and any env override that is set. Then:

```bash
timeout 300 python3 -I "$SURVEY" STORE_ROOT --max-files 200 > "$NOTES/survey.txt"
```

For a store holding one JSON document per session with an array of steps/messages, add `--split KEY` (e.g. `--split steps`) so each element is surveyed as a record. For SQLite, the script surveys a private copy; JSON in text columns gets the same skeleton.

If the store is unreadable from this session (EPERM, sandbox denial), don't stop and don't request a grant first: write `$NOTES/observe.sh` that runs `CLI --version`, `CLI --help`, lists the store tree (names, sizes, mtimes; skip credential and log contents) and runs the survey into `$NOTES/`, dry-run it against a synthetic store under the scratchpad (via the root's env override), and ask the user to run it in their own terminal. Survey output is redacted, so reading it back is safe.

Read `survey.txt`: layout patterns (where sessions and child sessions live), record discriminators and counts, key paths per record kind, enum values (roles, content block types). Compare against the docs/source claims and note every mismatch — the observed shape wins.

If the harness is not installed, or the store is empty or tiny (fewer than ~5 real sessions), go to Step 4. Otherwise continue to Step 5 and use Step 4 only if real usage is missing a feature.

### Step 4: Have the user install it and run real sessions

Ask the user to install the harness, log in, and do some ordinary work with it — a few sessions in a real project, ideally including: a multi-turn conversation with tool use, a subagent/task delegation, resuming a session, renaming one, and a compaction. Real usage reveals records synthetic probes miss (images, long tool output, errors, hooks, MCP calls). Wait for the user to confirm, then rerun Step 3.

### Step 5: Run probe sessions

Follow `references/probes.md`: generate markers, create probe directories, and run one probe per feature (basic, tool, reasoning, resume, subagent, fork, compact, title, cwd, abort, injected content), headless where possible and through tmux for interactive-only features. After each probe, locate its records with `--marker` and record the structure in the notes.

Before the first probe:
- **Sandbox:** if this session runs inside a sandbox, the harness probably cannot write its store under `$HOME`. Check with `nono why --self --path STORE_ROOT --op write`. If denied, write the probe commands to `$NOTES/probes.sh` and ask the user to run it **in their own terminal** (not a `!` command, which inherits this sandbox), then continue with the survey steps once they confirm.
- **Permissions:** run probes in the harness's read-only auto-approval mode, never a bypass-all mode (see `references/probes.md`).
- **Cost:** probes use the user's harness account. Tell the user how many sessions you will run (typically 10–12, each a few turns) and use the cheapest model that supports the feature.

Success: every row of the probe matrix has a finding or a cited "unsupported".

### Step 6: Write the mapping

Add a "Mapping" section to the notes that decides, for slimetrail's model:
- **Canonical store:** when the harness keeps more than one (e.g. a live database plus exported or end-of-session transcripts), which one slimetrail reads, and why — completeness while a session runs, fork/branch structure, availability across versions.
- **Identity:** native id source (header, row, filename); duplicates across live/archive.
- **Session metadata:** cwd, native title (and which record wins), git branch, freshness source.
- **Entries:** for each record kind → role and `Kind` (message/tool/meta); reasoning → meta with subtype `reasoning`; compaction summary → meta or automated message; ignored kinds (usage, model changes) and why.
- **Automation:** what marks subagent, injected-instruction, hook and compaction content as automated; `parent_ref` when the store establishes it.
- **Tree-shaped stores:** which branch is read (usually the live chain from the head pointer), and how history that left the chain — compaction, retries, reverts — is recovered or deliberately dropped.
- **Children and forks:** separate sessions or not; whether fork-copied history should be indexed (it will duplicate search hits — state the decision).
- **Fingerprint inputs:** companion files, title indexes, WAL/sidecars.
- **Unsupported:** formats or record kinds that will produce incomplete diagnostics instead of guesses.

Show the user this mapping and get agreement before coding. Decisions about forks, compaction and automation change search results and timeline totals.

### Step 7: Implement

Use the most recently added harness with the same storage style as the template, and find every exhaustive agent list by grepping, not from memory:

```bash
grep -rn "Agent::Pi\b" src tests          # every match/list over agents
grep -rniw "pi" README.md docs/*.md src/cli.rs   # selector help, store tables, prose lists
grep -rni "six\b\|all six" README.md docs src tests
```

Touchpoints, all of which a new agent needs:
- `src/model.rs`: `Agent` variant; `Roots` field(s) for every store path.
- `src/dto.rs`: `Agent::as_str` (which `Display` uses) and `FromStr`.
- `src/roots.rs`: root precedence exactly matching the harness's own env overrides.
- `src/sources/AGENT.rs` plus `registry` and `registry_in` in `src/sources/mod.rs`.
  - JSONL store close to an existing one: add `Agent` arms in `src/sources/jsonl.rs` (`discover` root, `header_record`, `update_metadata`, `normalize`, and `archive_kind` if ringleader can archive it), as Droid and Pi do, and delegate `Source::transcript` and `Source::drifted` to `jsonl::transcript` and `jsonl::drifted` so the live file is checked for stability and grown transcripts resume. Shared line scanning is in `src/sources/jsonl/scan.rs`; Codex's title index is in `src/sources/jsonl/titles.rs`.
  - JSONL/JSON with a different shape: a dedicated module (Cursor precedent), implementing `transcript` and `drifted` when it reads a live file.
  - SQLite: snapshot through `src/sources/sqlite.rs`, never the original, with `with_staging` (OpenCode precedent).
- Search-kind classification: set `Entry.searchable` to `Searchable::ToolResult` on entries that hold only tool output (compact coverage searches just their last 2 KB) and `Searchable::Duplicate` on text that repeats another entry; reasoning is `Kind::Meta` with subtype `reasoning`, which compact does not search. JSONL readers get this from the helpers in `jsonl.rs` (`tool_result_block`, `codex_tool_output`, `tool_result_role`) when the record shape matches; extend them or set the field in the reader (Devin precedent). An entry combining a call and its output stays `Text`.
- Reader contract: set `ParsedSession.recheck` when a diagnostic or value depends on anything outside the session's fingerprinted dependencies (a final line still being written, an unreadable companion file). Without it, an unchanged partial source is reused with its old diagnostics and never reread.
- `src/index/estimate.rs`: a `calibration` arm with default bytes per millisecond and index-to-source ratio for compact and full. The match is exhaustive, so the build fails without it. Measure them with the benchmark's `--per-agent` mode on the user's store, or borrow the closest existing agent's values and record that in the notes.
- `scripts/benchmark.py`: `AGENTS`, `roots()` (mirroring `src/roots.rs`) and a synthetic writer in `synthetic()` with reasoning, tool calls and tool results in the agent's record shape, plus a case in `scripts/test_benchmark_generator.py`.
- `src/api.rs`: `selected()` default set.
- `src/index/lock.rs`: a root that is a file (a database or index file) joins the file-root list in the index-overlap check. Grep for the template's root field name (e.g. `opencode_db`), not only its `Agent::` variant, to find this and the tests that enumerate root fields.
- `src/timeline.rs`: `excluded_child` decides whether the new agent's child sessions are dropped from timeline spans.
- `src/cli.rs`: selector flag, `--current` help text, `AGENT_HARNESS` error message, the `filter` list.
- `src/index/schema.rs`: decide whether parser changes require an index version bump (see architecture.md "Index storage and search"). Agents are stored by name, so a new `Agent` variant can go in alphabetical order; a new `Roots` field changes the namespace hash, which isolates old cached rows without a bump.
- Tests: every `for agent in [...]` list and root-default assertion the grep found; a synthetic `tests/fixtures/jsonl/AGENT.jsonl` (or database fixture) built from the observed skeleton; a golden; a test per probe feature (child session, fork, compaction, automation, title precedence, malformed record, missing store → incomplete).
- Goldens: the fixture generators derive goldens from the check-agent-logs reference reader, which has no reader for a new agent. Hand-write the new golden, review it entry by entry against the mapping, and keep the new agent out of `six.golden.json`'s generator unless the user also adds it to the reference reader. Record this in `docs/compatibility.md`.
- Docs: README intro list, selector text, store root table; `docs/compatibility.md` supported-stores row and boundaries, and its compact-coverage list of what counts as a tool result; `docs/json-protocol.md` agent values; `docs/performance.md` if the calibration came from a new measurement; any architecture or timeline rule the mapping introduced (e.g. child-session timeline exclusion).

### Step 8: Verify

1. `timeout 900 just check` — read the full summary, not a filtered tail.
2. Smoke-test against the real store with a private throwaway index:
   ```bash
   timeout 900 just build
   mkdir -m 700 -p .local/smoke-index
   ./target/release/slimetrail index --AGENT --index-dir .local/smoke-index --format json > "$NOTES/smoke-index.json"; echo "exit $?"
   ./target/release/slimetrail sessions --AGENT --scope all --index-dir .local/smoke-index --format json > "$NOTES/smoke-sessions.json"; echo "exit $?"
   ./target/release/slimetrail search SLIMEPROBE --AGENT --scope all --index-dir .local/smoke-index
   ```
   Build with `index` first: a query in JSON mode builds inline only what its estimate says fits in 20 s, so on a larger store it exits 1 with `index_required` instead of indexing. Every exit must be 0 (1 means incomplete — read the diagnostics). Every probe marker must be found in the expected session, `read AGENT:ID --mode log` of each probe session must show the features from the notes, subagent/injected content must be automated, and a timeline over the probe window must give plausible spans.
3. Confirm the other readers still refresh: run `index` with the new agent plus a fast subset (e.g. `--pi --opencode`) against the same throwaway index, on the release build. Do not cold-index `--all` on the real corpus: `docs/performance.md` records per-agent cold builds of one large corpus summing to about 12 minutes compact and 32 minutes full, and the older v2 all-agent build timing out at 900 seconds.
4. Prove the key tests bite: mutate the reader (disable compaction recovery, disable de-duplication, drop the automation flag), confirm with grep that each edit landed, and check that a test fails for each; restore the file.
5. Remove `.local/smoke-index`. Leave `$NOTES` for the user, and list the probe sessions left in the harness's store.

## Examples

### Example: a harness with almost no local history

User says: "add support for gemini cli to slimetrail"

Actions:
1. Agree on id `gemini`; record the CLI version.
2. Research the docs and the open-source repo: store root and its env override, record format, headless flag, compaction and saved-chat commands — each with a source link in the notes.
3. Survey finds 3 sessions, too few. Ask the user to run a handful of real sessions, then resurvey.
4. Probe headless for basic/tool/resume/subagent/cwd, and through tmux for compaction, saving/forking a chat and abort. Suppose the subagent probe's child marker shows up inline in the parent file: record "no separate child session; child content marked by its tool record".
5. Present the mapping (e.g. compaction summary → automated meta; saved chats → separate sessions) and get agreement.
6. Implement in a dedicated module if the shape differs from the JSONL readers, with a test per probe feature and docs updated.
7. `just check` passes; the smoke search finds every probe marker in the expected session.

Result: `slimetrail search X --gemini` works, and `docs/compatibility.md` documents its boundaries.

### Example: harness already in heavy use

User says: "slimetrail should read Amp sessions too, I've been using it for weeks"

Actions: skip Step 4; the survey covers real usage. Probes run only for features the survey didn't show (fork, compaction, abort).

## Troubleshooting

### The store directory returns EPERM or "Operation not permitted"
**Cause:** This session's sandbox has no grant for the harness's store, so even existence can't be checked.
**Solution:** Confirm with `nono why --self --path STORE_ROOT --op read`. Route the survey and the probes through scripts the user runs in their own terminal (Step 3), and mention the denial to the user; whether to add a grant is their call.

### Probe sessions don't appear in the store
**Cause:** The harness ran inside this session's sandbox and could not write its store, or an env override points elsewhere, or headless mode doesn't persist sessions.
**Solution:** Check the harness output for write errors and `nono why --self --path STORE_ROOT --op write`; have the user run `probes.sh` in their own terminal. Check `env | grep -i HARNESS_NAME` and the docs for a "don't save" default in headless mode (some need an explicit flag).

### `survey.py` reports "(no discriminator)" for most records
**Cause:** The store uses a discriminator key the script doesn't check.
**Solution:** Use `--record FILE:LINE` on a few lines to find the real discriminator, then add the key to `DISCRIMINATORS` in the script.

### A marker is found in several sessions
**Cause:** Resume, fork or a subagent copied history, or the parent records the child's prompt.
**Solution:** That is the finding. Record which copy is canonical and decide in the mapping whether copies are indexed.

### Smoke run exits 1
**Cause:** A selected store is incomplete — unparseable records, an unsupported legacy format, or churn while the harness was running.
**Solution:** Read the stderr diagnostics; stop running harness sessions and retry. Decide whether each diagnostic is a reader gap to fix or an intended "unsupported" boundary to document.
