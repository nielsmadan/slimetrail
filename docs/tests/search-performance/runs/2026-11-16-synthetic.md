# 2026-11-16 synthetic baseline

Purpose: establish the first repeatable release-mode baseline for cold refresh, reuse, fallback matching and one changed source. Followed the [procedure](../README.md), using `timeout 900 python3 scripts/benchmark.py` after `timeout 900 cargo build --release --locked`. All commands completed successfully. [Aggregate samples and cleanup evidence](2026-11-16-synthetic.jsonl) are retained.

## Setup

- Apple M1 Max, macOS/Darwin arm64; Rust 1.99.0 (`b940084d7`, 2026-09-28); Python 3.11.6. Power/thermal state and unrelated background load were not controlled.
- Crate 0.1.0, optimized release build, Cargo.lock used. Repository had no commit at measurement time; this was the Tasks 1–6 implementation including the selector/help fix, before final whole-run review.
- Binary SHA-256: `41d039981cfde1cbace23119fae9f633b4293c48fa11d661cab550b650f22f25`.
- 50 sessions per agent × 6 agents, 200 messages per session: 300 sessions, 60,000 entries, 60,200 search documents. This workload has messages and native titles, not a representative mix of tools/metadata.
- Source inventory: 351 files, 22,080,376 bytes; optional absent archives/sidecars are reflected by metadata missing counts and did not cause incomplete results. Indexed original entry text: 14,567,000 bytes. Index: 115,634,176 bytes. Source OpenCode DB: 4,530,176 bytes.
- 6,000 non-ASCII documents (9.97%); zero NUL documents. Deterministic marker distribution: 10% `needle`, 10% `café`, remaining messages ASCII filler.
- One process at a time, one index warmup, one discarded warmup per query, five measured warm repetitions. Cold and changed-source cases each have one sample. Filesystem caches were not flushed. Queries disable snippets and discard stdout; output serialization remains part of wall time.

## Results

| Operation | Samples | Minimum / median / maximum seconds | Refresh evidence |
| --- | ---: | --- | --- |
| Cold index | 1 | 4.805149 / 4.805149 / 4.805149 | 300 discovered, 300 reparsed, 0 failed |
| Unchanged index | 5 | 0.132007 / 0.145052 / 0.158620 | 300 reused, 0 reparsed in each |
| OpenCode-only inventory | 5 | 0.031155 / 0.033544 / 0.035208 | 50 reused, 0 reparsed in each |
| Claude-only inventory | 5 | 0.030906 / 0.037267 / 0.044677 | 50 reused, 0 reparsed in each |
| ASCII literal `needle` | 5 | 0.184952 / 0.185244 / 0.216695 | Refresh included |
| Short literal `ed` | 5 | 0.272313 / 0.290311 / 0.299804 | Full candidate scan; refresh included |
| Non-ASCII literal `café` | 5 | 0.294560 / 0.305756 / 0.333858 | Full candidate scan; refresh included |
| Regex `needle\|café` | 5 | 0.291240 / 0.294905 / 0.297471 | Full candidate scan; refresh included |
| One appended Claude message | 1 | 0.161569 / 0.161569 / 0.161569 | 299 reused, 1 reparsed, 0 failed |

All measured subprocess exits were 0 with no diagnostic counts. Index counters reported zero removals and failures. Warmups are excluded from the table.

## Interpretation and cleanup

The measured incremental contract held: no unchanged parses, exactly one parse after changing one transcript. Fallback queries added roughly 0.105–0.121 seconds compared with the ASCII query on this corpus, including differences in match counts and refresh timing. OpenCode's 0.033544-second inventory is an end-to-end refresh including snapshot/copy, not an isolated copy benchmark. Cold/changed single samples do not establish a latency distribution. No subsecond promise follows for real stores or Linux.

Before generation/indexing, the benchmark had about 279 GB free. The driver removed its entire owned workspace and reported `index_exists: false` and `workspace_exists: false`. Only these aggregate records remain; the synthetic sources/index were deleted.
