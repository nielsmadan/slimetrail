# 2026-11-24 real per-agent cold builds, compact and full coverage

Purpose: measure what each real store costs to index from scratch under each coverage preset, so that the first-build estimate has defaults and so that sizes are quoted only from measurement. This is one user's corpus on one machine: a labelled stress case, not a typical distribution. Each agent and coverage ran as its own invocation:

```sh
timeout 1500 python3 scripts/benchmark.py --real --agents AGENT --per-agent --coverage COVERAGE --repeat 1 --timeout 1200 --total-timeout 1400
timeout 4800 python3 scripts/benchmark.py --real --agents claude --per-agent --coverage COVERAGE --repeat 1 --timeout 2200 --total-timeout 4600 --capacity-factor 4
```

Claude used `--capacity-factor 4` because 12 × its 14.3 GB of source exceeded the free space; synthetic runs had shown the index at 1.7–2.6 × source, and the runtime 2 GiB reserve check stayed active. The optimized build has SHA-256 `dfb54efe6c5178dce71333124e2b1b08100f32c4f2653d8f9f8d081b88953318` (index v3). Apple M1 Max, 64 GB, Darwin arm64; Rust 1.99.0, Python 3.15.2. No concurrent build, test or benchmark ran; unrelated background load was uncontrolled, and the per-agent cold run follows the standard run, so its source files were in the OS cache. [All aggregate samples and cleanup evidence](2026-11-24-per-agent-real.jsonl) are retained.

## Per-agent cold index (`slimetrail index --AGENT` into a fresh index)

Source bytes are the metadata inventory of each store (transcripts, companion files, databases and sidecars). Index bytes are the database plus WAL after a TRUNCATE checkpoint.

| Agent | Sessions | Entries | Source bytes | Compact ms | Compact index bytes | Full ms | Full index bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Claude | 10,835 | 1,256,834 | 14,273,487,361 | 456,975 | 9,068,220,416 | 1,021,724 | 14,087,720,960 |
| Codex | 5,573 | 454,195 | 3,540,441,882 | 204,387 | 5,146,402,816 | 791,178 ¹ | 11,154,964,480 ¹ |
| Cursor | 55 | 1,875 | 125,343,072 | 526 | 10,772,480 | 514 | 10,772,480 |
| Devin | 76 | 7,266 | 226,418,304 | 7,139 | 36,007,936 | 8,003 | 54,562,816 |
| Droid | 770 | 68,791 | 839,886,445 | 24,384 | 931,041,280 | 116,086 | 2,015,457,280 |
| OpenCode | 251 | 5,421 | 167,997,440 | 2,628 | 60,723,200 | 2,931 | 66,506,752 |
| Pi | 121 | 5,150 | 26,362,933 | 863 | 33,280,000 | 1,965 | 54,329,344 |

¹ Codex full: the standard run's cold index completed in 791.178 seconds with the index size shown (taken from its indexed dimensions, before checkpoint). The later per-agent cold run then hit the remaining 1,400-second driver budget and was recorded as a `TimeoutError`; it is not a completed per-agent sample.

Summed over agents, compact cold builds took about 697 seconds and produced 15.3 GB of index for 19.2 GB of source; full builds took about 1,942 seconds and produced 27.4 GB. The earlier v2 all-agent cold build did not finish in 900 seconds ([record](2026-11-16-final-real-all.md)); these per-agent sums are separate invocations, not one all-agent run.

## Warm refresh and queries (standard run, one sample each after a warmup)

| Agent, coverage | Warm index s | ASCII `needle` s | Short `ed` s | Non-ASCII `café` s | Regex s |
| --- | ---: | ---: | ---: | ---: | ---: |
| Claude compact | 0.588 | 5.339 | 9.663 | 9.228 | 9.656 |
| Claude full | 0.615 | 7.468 | 10.160 | 9.588 | 9.494 |
| Codex compact | 0.387 | 3.414 | 7.003 | 6.854 | 6.848 |
| Codex full | 0.359 | 6.738 | 7.947 | 7.095 | 7.325 |
| Droid compact | 0.191 | 0.717 | 1.277 | 1.355 | 1.371 |
| Droid full | 0.176 | 0.972 | 1.414 | 1.330 | 1.335 |

Smaller stores answered every query in under 0.2 seconds in both coverages; see the JSON-lines evidence. Compact searches carry the informational `search_coverage` diagnostic and exit 0; no run reported any other diagnostic. Search latency on the two largest stores is dominated by candidate verification and fallback scans over a million-entry corpus; it is recorded, not addressed here.

Every invocation reported complete coverage. Each run removed its private workspace and reported `index_exists: false`, `workspace_exists: false`. No source IDs, text, snippets or paths appear in the retained evidence; no original database was opened through benchmark SQLite tooling, no source was mutated and the default XDG index was not used.
