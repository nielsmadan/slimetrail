# Search performance procedure

Run when changing discovery, snapshotting, indexing or matching, and compare only like-for-like datasets and build settings. There is no pass/fail latency threshold. The required incremental behavior is unchanged sources reused and one changed synthetic transcript reparsed once; incomplete real-store coverage must be reported alongside timings.

## Prerequisites and execution

Use Python 3.11+ with stdlib SQLite, a Rust toolchain meeting the manifest's MSRV, and `timeout`. Run from the repository root. Build optimized code first:

```sh
timeout 900 just build
timeout 900 python3 scripts/benchmark.py
```

The default generator creates 50 sessions per agent and 200 message entries per session, across seven supported formats (Claude, Codex, Cursor, Devin, Droid, OpenCode, Pi), in an owned temporary directory below `.local/`. Every tenth message contains `needle`, the next contains `café`, and the rest contain ASCII filler. Besides messages, each session carries reasoning blocks, tool calls and tool results in the agent's own record shape; tool-result lengths follow a seeded, deterministic mix (60% under 1 KB, 30% 1–16 KB, 10% 16–256 KB). Cursor has companion JSON metadata; OpenCode uses a synthetic SQLite DB; Devin uses a synthetic `cli/sessions.db` (WAL) with `sessions`, `message_nodes` and `subagent_heads`. Native titles exist for Cursor/Devin/Droid/OpenCode/Pi. This measures format plumbing and text volume: message text is repetitive filler and the tool-result size mix is chosen, not measured from real stores, so the corpus is not a realistic distribution of message or tool-result lengths.

The driver records one cold index, one index warmup, five warm indexes, five OpenCode-only and Claude-only inventories, then a warmup and five measurements each for `needle`, `ed`, `café`, and regex `needle|café`. Queries use `--scope all --max-snippets 0 --format json --time-zone UTC`. It appends one message to one synthetic Claude transcript and records a final incremental refresh. `--sessions`, `--entries` and `--repeat` adjust workload/repetitions; preserve them for comparisons.

`just bench` runs this synthetic procedure and validates complete coverage, warm reuse, exactly one changed-source reparse and cleanup. Scheduled CI retains its aggregate output as an artifact without a timing threshold.

`--coverage compact|full` (default `compact`) writes `coverage = "…"` to a config file in the owned workspace and points `SLIMETRAIL_CONFIG` at it, so a real user config is never read or modified. `--per-agent` additionally runs, after the standard measurements, one cold `slimetrail index --<agent>` per selected agent into a fresh index and emits one record per agent: `source_files`, `source_bytes`, `sessions`, `entries`, `cold_ms`, `index_bytes`, exit status, diagnostic counts and refresh counters. `index_bytes`, here and in the standard run's `indexed_dimensions`, is the DB plus WAL size after `PRAGMA wal_checkpoint(TRUNCATE)`; that checkpoint opens only the owned benchmark index, never an agent store. `cold_ms` is OS-cache-warm: the per-agent pass runs after the standard measurements, which have already read the source files, so it understates cold disk I/O (notably for `--real`). Each per-agent index is removed right after its record is emitted. `scripts/check_benchmark.py` validates these records (complete, exit 0, positive sizes, coverage matching the run).

```sh
timeout 900 python3 scripts/benchmark.py --per-agent --coverage full
```

Real sources require explicit opt-in:

```sh
timeout 900 python3 scripts/benchmark.py --real
```

This captures the existing environment's root overrides and selects all seven agents by default. For a declared subset, use e.g. `--real --agents pi opencode`; a subset does not establish all-agent coverage. The default per-command deadline is 180 seconds, and total driver budget is 840 seconds. Larger runs may override `--timeout` and `--total-timeout`; place an outer `timeout` above the total budget to leave time for cleanup. The driver terminates a timed-out child before deleting its workspace. A real run never performs the changed-source mutation.

## Privacy, capacity and outputs

The index always lives under an owned 0700 `.local/benchmark-*` workspace, never the default XDG location. Metadata preflight reports only per-agent file counts, byte counts and missing/unreadable counts, then requires free space of at least `--capacity-factor` (default 12) × discovered source bytes plus a 2 GiB reserve. This is a conservative heuristic, not a maximum expansion guarantee. Runtime checks the free-space reserve every half second. Originals are read only by the app's source/snapshot code; benchmark SQLite connections touch only generated synthetic databases or the owned index for aggregate counts.

The JSON-lines output includes build hash, platform, dimensions, individual durations, medians, exit statuses, refresh counters, whitelisted diagnostic-code counts and cleanup proof. Search stdout is discarded; index stdout is parsed only to retain numeric counters. Stderr messages, paths, session IDs, titles, entry text and snippets are not emitted or stored. Unknown diagnostic codes are counted as `other`. Do not replace these safeguards with raw CLI logging on real stores.

Cleanup runs in `finally`, including child failures, deadlines and SIGTERM. SIGKILL or machine loss cannot run Python cleanup; after verifying that no benchmark process is active, remove only the abandoned workspace owned by that run. A successful cleanup record must report both `index_exists: false` and `workspace_exists: false`. Retain only aggregate JSON-lines evidence in run records.

## Interpretation

Durations use `time.monotonic` around a complete CLI invocation. Warm query medians include discovery/refresh and all-agent OpenCode snapshots; they are not cache-only query latency. OpenCode-only inventory timing includes the entire CLI refresh and is not isolated copy overhead. Cold index means absent index, with OS caches uncontrolled. There is one concurrent CLI process, one cold sample and one synthetic mutation sample; warm queries have a discarded warmup plus the configured repeat count. Background machine load is uncontrolled. No p95 estimate is inferred from five samples.

Record hardware/toolchain, binary hash, actual arguments, workload dimensions, status and diagnostics, and cleanup evidence. An exit-1 measurement is useful timing for incomplete coverage, not a successful full-corpus run. A deadline is a timed-out attempt, not a slow completed measurement. Code snapshots can be identified by the binary hash when the repository has no commit yet.

## Recorded runs

The 2026-11-24 runs are the current baselines, recorded before later v3 changes (separately stored tool-result tails, `secure_delete=FAST` with incremental vacuum, chunked append verification) that have not been re-measured; the 2026-11-16 v2 runs are historical.

- [2026-11-24 real per-agent cold builds](runs/2026-11-24-per-agent-real.md): index v3, each store indexed separately under compact and full coverage; source of the first-build estimate defaults.
- [2026-11-24 synthetic per-agent measurements](runs/2026-11-24-per-agent-synthetic.md): the extended generator (reasoning, tool calls/results, Devin) under both coverages.
- [2026-11-16 final-build synthetic measurements](runs/2026-11-16-final-synthetic.md): the last v2 default-workload baseline (six agents, messages only), after the final fallback-query fix; five warm samples and verified cleanup.
- [2026-11-16 final-build real all-agent attempt](runs/2026-11-16-final-real-all.md): the v2 final build's cold build timed out at 900 seconds; no completed full-corpus warm/query evidence for v2.
- [2026-11-16 final-build real Pi/OpenCode subset](runs/2026-11-16-final-real-subset.md): the v2 final build's complete subset measurements, separate from all-agent coverage.
- [2026-11-16 earlier corrected v2 synthetic measurements](runs/2026-11-16-synthetic-v2.md): intermediate build, bundled-engine candidate plans/VM steps, and bounded long-session child-key comparison.

- [2026-11-16 synthetic baseline](runs/2026-11-16-synthetic.md): the comparison baseline for the default 300-session/60,000-entry corpus.
- [2026-11-16 real all-agent, 180-second deadline](runs/2026-11-16-real-180.md): cold timed out; index removed.
- [2026-11-16 real all-agent, 900-second deadline](runs/2026-11-16-real-900.md): cold timed out; full-corpus warm/query performance remains unestablished; index removed.
- [2026-11-16 real Pi/OpenCode subset](runs/2026-11-16-real-subset.md): completed cold/warm/fallback queries for 371 sessions; separate subset evidence, with index removed.
