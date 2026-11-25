# 2026-11-24 synthetic per-agent measurements, compact and full coverage

Purpose: measure the extended synthetic generator (reasoning, tool calls and results, Devin) per agent under both coverage presets, as the like-for-like synthetic baseline for index v3 and a cross-check on the real per-agent rates.

Command: `timeout 900 python3 scripts/benchmark.py --per-agent --coverage COVERAGE` for `compact` and `full` (default workload: 50 sessions × 200 messages per agent, now with reasoning, tool calls and tool results whose lengths follow the generator's seeded 60/30/10 mix, plus a synthetic Devin store). Same build and machine as the [real per-agent record](2026-11-24-per-agent-real.md). [Aggregate samples](2026-11-24-per-agent-synthetic.jsonl) are retained; both runs passed `scripts/check_benchmark.py`.

| Measurement (seven agents, 350 sessions) | Compact | Full |
| --- | ---: | ---: |
| Cold index, seconds | 4.766 | 9.014 |
| Index bytes (standard run; main database file only, WAL not counted) | 286,658,560 | 393,924,608 |
| Search documents | 83,250 | 90,250 |
| Median warm index, seconds | 0.141 | 0.141 |
| Median ASCII / short / non-ASCII / regex, seconds | 0.138 / 0.349 / 0.351 / 0.352 | 0.140 / 0.371 / 0.381 / 0.383 |
| One changed transcript, seconds | 0.141 | 0.145 |

Per agent, a compact cold build indexed about 49,000–54,000 source bytes per millisecond for the JSONL agents (Devin 34,000, OpenCode 19,000) with an index of 1.7–1.8 × source (OpenCode 2.4 ×); full indexed about 19,000–23,000 bytes/ms with 2.4–2.7 × source. Synthetic text is repetitive and its tool results are larger than typical, so these ratios are not a projection for real stores.

Every subprocess in both runs exited 0; the only diagnostic code recorded was the informational `search_coverage` on compact searches. Each run removed its private workspace and reported `index_exists: false`, `workspace_exists: false` in its final record; only aggregate evidence is retained.
