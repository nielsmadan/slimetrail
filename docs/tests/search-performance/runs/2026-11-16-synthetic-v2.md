# 2026-11-16 corrected v2 synthetic measurements

Purpose: measure the whole-feature review fixes on synthetic data, preserving the [original baseline](2026-11-16-synthetic.md) and historical real-store records. Current binary SHA-256: `6a56415b888c986cff7fb0f9f531c2465f669ae8d3a1bbd5a92c37a473493c0a`. Optimized locked build, Darwin arm64, Apple M1 Max, Rust 1.99.0. SQLite bundled version 3.50.2. No real store was read. Background load and filesystem caches were uncontrolled; no latency target applies.

## Default six-agent workload

Executed `timeout 900 cargo build --release --locked`, then `timeout 900 python3 scripts/benchmark.py`. Same default generator and procedure: 300 sessions, 60,000 entries, 60,200 documents; 6,000 non-ASCII documents and zero NUL documents. Source bytes remain 22,080,376; index v2 uses 115,949,568 bytes. Five measured warm repetitions after a discarded warmup; cold and changed-source timings are single samples. [All samples and cleanup](2026-11-16-synthetic-v2.jsonl).

| Operation | Samples | Minimum / median / maximum seconds |
| --- | ---: | --- |
| cold_index | 1 | 4.248845 / 4.248845 / 4.248845 |
| warm_index | 5 | 0.137749 / 0.144479 / 0.147251 |
| warm_opencode_inventory | 5 | 0.031866 / 0.034586 / 0.039121 |
| warm_claude_inventory | 5 | 0.029832 / 0.032283 / 0.033838 |
| ascii_literal | 5 | 0.198910 / 0.203116 / 0.221832 |
| short_literal | 5 | 0.310021 / 0.353139 / 0.354912 |
| non_ascii_literal | 5 | 0.307269 / 0.340584 / 0.353412 |
| regex | 5 | 0.306718 / 0.326522 / 0.345856 |
| one_changed_source | 1 | 0.153964 / 0.153964 / 0.153964 |

All invocations exited 0 without diagnostics. Unchanged refresh reused all 300 sessions; one appended source reparsed one and reused 299. The cold observation decreased from 4.805 to 4.249 seconds, while ASCII and broad fallback query medians increased relative to the old run. These different-time end-to-end observations do not establish a universal speedup or isolate individual fixes. Query durations include refresh. Both source/index workspaces were removed by the driver.

## Bundled-engine candidate query

Executed `timeout 900 cargo test --release --locked --lib candidate_query_release_scaling -- --ignored --nocapture`. The test compares the former query against the corrected candidate query on the same current synthetic index, using rusqlite's bundled SQLite and VM-step counter. Two eligible sessions, all ASCII, no NUL; two hits for `needle`, zero for `missing`. It checks row counts and FTS/foreign-key integrity. [Complete output](2026-11-16-candidates-v2.txt).

| Documents | No-hit old / corrected VM steps | Two-hit old / corrected VM steps |
| ---: | ---: | ---: |
| 2,000 | 32,037 / 52 | 32,220 / 319 |
| 4,000 | 64,037 / 52 | 64,220 / 319 |
| 8,000 | 128,037 / 52 | 128,220 / 319 |

Bundled `EXPLAIN QUERY PLAN` verifies candidate rowids drive integer-primary-key lookups; `documents_nul` and `documents_non_ascii` supply covering-index fallback subsets. The regular unit test `bundled_candidate_plan_uses_rowids_and_indexed_fallbacks` checks those properties. Reported microsecond samples in the output are single observations, not a latency distribution. This isolates query execution without refresh; Unicode fallback volume and many hits can still create broad candidate work.

## Long-session child-key index

A bounded synthetic CLI probe used the same final release binary for both variants. For each case, it created an owned fresh index from an empty Claude session, replaced that transcript with the stated count of short `needle synthetic entry N` messages, then timed one refresh. Only the control index's `documents_session` definition was replaced with its former four-column prefix; the corrected variant retains `entry_index`. Source roots were synthetic. Each CLI call had a 60-second deadline, outer probe 120 seconds. [Samples and cleanup](2026-11-16-long-session-v2.jsonl).

| Entries | Former child-key prefix, seconds | Complete child key, seconds |
| ---: | ---: | ---: |
| 2,000 | 0.232218 | 0.067771 |
| 4,000 | 0.798506 | 0.134319 |
| 8,000 | 3.498129 | 0.255068 |

All cases reparsed one source, exited 0 with no diagnostics, and passed entry count, SQLite integrity, foreign-key and FTS checks. The corrected curve approximately doubles as entries double, unlike the former growing child scan. These short synthetic entries do not establish realistic long-output timing or explain the historical full-real-store timeout conclusively. The probe workspace was removed. Automated regression `long_session_replacement_preserves_entries_foreign_keys_and_fts` additionally verifies full replacement from 4,000 to 2,000 entries.

## Evidence boundaries

Historical all-agent real attempts still timed out on their recorded older binary; neither completion nor final-build real latency is established by these synthetic results. Historical Pi/OpenCode numbers remain valid observations of the older binary, not measurements of this v2 build. Linux runtime and current dependency-advisory auditing were not performed. The additional probes are bounded evidence for the reviewed query and child-key changes, not a new broad performance programme.
