# 2026-11-16 final-build real Pi/OpenCode subset

Purpose: obtain completed current-build measurements after the [final all-agent cold timeout](2026-11-16-final-real-all.md). This measures **only Pi and OpenCode**; it does not establish completed coverage or latency for the other four real stores. Command: `timeout 900 python3 scripts/benchmark.py --real --agents pi opencode` (180-second subprocess deadline, 840-second driver budget, five warm repetitions).

The optimized locked build has SHA-256 `b7681b13566fb1ccfd5e84c381cd190804d4eab29bac3a3455cb4b33adee490f`, matching the final all-agent attempt. Apple M1 Max, Darwin arm64; Rust 1.99.0, Python 3.11.6. No concurrent build/test/benchmark workload; unrelated background load and OS caches were uncontrolled. [All aggregate samples and cleanup evidence](2026-11-16-final-real-subset.jsonl) are retained.

The corpus metadata inventory is unchanged from the earlier subset: Pi 154 files /26,262,196 bytes and OpenCode DB/sidecars 3 files /137,613,312 bytes; total 163,875,508 bytes. Metadata sizing found no unreadable paths and one absent OpenCode sidecar. The current normalizers produced 371 sessions (251 OpenCode, 120 Pi), 10,569 entries, 31,332,926 entry-text bytes and 10,875 search documents. Of those documents, 2,634 (24.22%) contained non-ASCII text and none contained NUL. The v2 index was 152,920,064 bytes. Entry/document counts differ from the historical binary, so old/new timing comparisons do not isolate one optimization.

One fresh-index cold sample was followed by an index warmup and five warm refreshes. Every query had a discarded warmup and five measured calls, using the procedure's generic queries, all-cwd scope and zero snippets. Search stdout and match results/counts were discarded; query selectivity was not retained. One process ran at a time. Real sources were not mutated for a changed-source test.

| Operation | Minimum / median / maximum seconds | Refresh/status evidence |
| --- | --- | --- |
| Cold index, one sample | 7.391771 / 7.391771 / 7.391771 | 371 discovered/reparsed, 0 failed |
| Warm index | 0.260072 / 0.276849 / 0.282232 | 371 reused, 0 reparsed each |
| OpenCode inventory | 0.200554 / 0.206309 / 0.224616 | 251 reused, 0 reparsed each |
| ASCII literal `needle` | 0.307562 / 0.308153 / 0.314869 | Exit 0, no diagnostics |
| Short literal `ed` | 0.306455 / 0.314035 / 0.322679 | Exit 0, no diagnostics |
| Non-ASCII literal `café` | 0.301673 / 0.311747 / 0.321507 | Exit 0, no diagnostics |
| Regex `needle\|café` | 0.315727 / 0.327106 / 0.329096 | Exit 0, no diagnostics |

Every subprocess exited 0 with no diagnostic counts; index envelopes reported complete coverage for the selected subset. Warm calls reused all sessions. OpenCode inventory timing includes startup, index opening, snapshot, enumeration and refresh; copy-only time was not isolated. Fallback medians were about 0.004–0.019 seconds above the ASCII median, with different match sets and uncontrolled variance. These end-to-end results provide no universal speedup, subsecond service guarantee, or full-corpus claim.

Capacity preflight passed with 279,408,738,304 bytes free against 4,113,989,744 required. The driver removed its private real-content workspace and reported `index_exists: false`, `workspace_exists: false`. No source IDs, text, snippets or paths appear in retained evidence; no original DB was opened through benchmark SQLite tooling, no source file was mutated and no default XDG index was used. Final independent cleanup verification is also recorded with the [final synthetic run](2026-11-16-final-synthetic.md).
