# 2026-11-16 real Pi/OpenCode subset

Purpose: obtain completed real-store refresh/query timings after the [all-agent 900-second timeout](2026-11-16-real-900.md). This is **only Pi and OpenCode**, not a substitute for all-agent coverage. Command: `timeout 900 python3 scripts/benchmark.py --real --agents pi opencode`, using a 180-second per-command deadline, 840-second total budget and five warm repetitions. All commands completed with exit 0 and no diagnostic counts. [Aggregate samples](2026-11-16-real-subset.jsonl) contain no transcript text, paths, session IDs or snippets.

## Setup

Hardware/toolchain and binary hash match the [synthetic baseline](2026-11-16-synthetic.md): Apple M1 Max, Darwin arm64, Rust 1.99.0, Python 3.11.6; release binary SHA-256 `41d039981cfde1cbace23119fae9f633b4293c48fa11d661cab550b650f22f25`. The repository had no commit. This was a new private index after both all-agent workspaces had been removed; unrelated machine load and OS filesystem caches were uncontrolled.

Metadata inventory: Pi 154 files / 26,262,196 bytes; OpenCode DB/sidecars 3 files / 137,613,312 bytes, one absent sidecar. Combined metadata bytes: 163,875,508. There were no unreadable metadata paths. Indexed result: 371 sessions (251 OpenCode, 120 Pi), 10,282 entries, 31,333,216 original entry-text bytes, 10,588 search documents and a 151,351,296-byte index. Of the documents, 2,601 contained non-ASCII text (24.57%); none contained NUL.

One process ran at a time. Cold used a fresh index and one sample. An index warmup preceded five warm refreshes. Every query had one discarded warmup and five measured calls with `--scope all --max-snippets 0`; stdout was discarded. The generic literal queries were `needle`, `ed`, `café`, with regex `needle|café`. Match results/counts were not retained, so these measurements establish latency and status, not comparative result selectivity. No real source was changed for an incremental test.

## Results

| Operation | Minimum / median / maximum seconds | Evidence |
| --- | --- | --- |
| Cold index (one sample) | 7.733923 / 7.733923 / 7.733923 | 371 discovered/reparsed, 0 failed |
| Warm index (five samples) | 0.270617 / 0.299617 / 0.313146 | 371 reused, 0 reparsed in every call |
| OpenCode inventory (five samples) | 0.218674 / 0.229996 / 0.238068 | 251 reused, 0 reparsed in every call |
| ASCII literal | 0.298391 / 0.309122 / 0.322500 | Exit 0, no diagnostics |
| Short literal | 0.325067 / 0.346071 / 0.372977 | Exit 0, no diagnostics |
| Non-ASCII literal | 0.327130 / 0.350335 / 0.367273 | Exit 0, no diagnostics |
| Regex | 0.316014 / 0.347130 / 0.368182 | Exit 0, no diagnostics |

Warm refresh reuses entries but still pays for inventory and snapshot work. The OpenCode-only median of 0.229996 seconds includes startup, index opening, private snapshot, enumeration and refresh; copy time was not separately instrumented. Fallback-query medians were roughly 0.037–0.041 seconds above the ASCII median on this subset, with different match sets and uncontrolled machine variance. These are end-to-end observations, not isolated algorithm timings or a performance guarantee. The other four real agents and the complete all-agent query path remain unmeasured after the cold timeouts.

## Capacity and cleanup

Preflight reported 279,675,580,416 free bytes against a conservative 4,113,989,744-byte requirement. The driver removed its owned real-content workspace in `finally` and reported `index_exists: false`, `workspace_exists: false`. A separate final inventory found **zero `.local/benchmark-*` workspaces**, covering both all-agent attempts and this subset. No original DB was opened by benchmark SQLite tooling, no source files were mutated, and only aggregate evidence remains.
