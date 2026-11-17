# 2026-11-16 final-build synthetic measurements

Purpose: record the final reviewed release after the last fallback-query fix, preserving the [earlier v2 record](2026-11-16-synthetic-v2.md). Executed `timeout 900 python3 scripts/benchmark.py` after the final locked release build and completed real-store attempts, with no concurrent benchmark/build/test workload. Binary SHA-256: `b7681b13566fb1ccfd5e84c381cd190804d4eab29bac3a3455cb4b33adee490f`, checked again after all measurements. Apple M1 Max, Darwin arm64; Rust 1.99.0 (`b940084d7`, 2026-09-28); Python 3.11.6. OS caches and unrelated background load were uncontrolled. [All aggregate samples](2026-11-16-final-synthetic.jsonl) are retained.

The default six-agent generator produced 300 sessions, 60,000 entries and 60,200 search documents. Source inventory: 351 files /22,080,376 bytes; indexed entry text: 14,567,000 bytes; index v2: 115,949,568 bytes. Non-ASCII documents: 6,000 (9.97%); NUL documents: zero. Workload and queries match the documented [procedure](../README.md). One fresh-index cold sample and one appended-source sample; five warm measurements after discarded warmups, one CLI process at a time. Query stdout was discarded with snippets disabled; durations include refresh and serialization.

| Operation | Minimum / median / maximum seconds | Refresh evidence |
| --- | --- | --- |
| Cold index, one sample | 4.135773 / 4.135773 / 4.135773 | 300 discovered/reparsed |
| Warm index | 0.119637 / 0.122138 / 0.130949 | 300 reused, 0 reparsed each |
| OpenCode inventory | 0.029500 / 0.031326 / 0.033943 | 50 reused, 0 reparsed each |
| Claude inventory | 0.025363 / 0.028621 / 0.029572 | 50 reused, 0 reparsed each |
| ASCII literal `needle` | 0.177848 / 0.187932 / 0.206887 | Refresh included |
| Short literal `ed` | 0.278408 / 0.279647 / 0.303736 | Refresh included |
| Non-ASCII literal `café` | 0.291167 / 0.300223 / 0.304303 | Refresh included |
| Regex `needle\|café` | 0.293504 / 0.306808 / 0.336302 | Refresh included |
| One changed source, one sample | 0.160416 / 0.160416 / 0.160416 | 299 reused, 1 reparsed |

Every subprocess and the driver exited 0, without diagnostics; all index envelopes were complete with zero failures/removals. Unchanged entries were reused and exactly one changed transcript was reparsed. Fallback-query medians were about 0.092–0.119 seconds above the ASCII median, with different match sets. OpenCode inventory timing includes snapshot and enumeration but does not isolate copy time. These different-time measurements do not isolate individual fixes or establish a universal speedup, p95 latency, or subsecond guarantee.

Capacity preflight passed with 279,385,092,096 bytes free against 2,412,448,160 required. The driver reported `index_exists: false`, `workspace_exists: false`. A final independent check found **zero `.local/benchmark-*` workspaces**, covering the final full-real attempt, final real subset and final synthetic run as well as earlier removed workspaces. The known generated `scripts/__pycache__/benchmark.cpython-311.pyc` was removed, along with its empty directory. Only aggregate run evidence remains. Full-real cold/warm/query completeness remains unestablished after the [final 900-second timeout](2026-11-16-final-real-all.md); [Pi/OpenCode](2026-11-16-final-real-subset.md) has separate complete subset measurements. Linux runtime was not measured.
