# 2026-11-16 final-build real all-agent attempt

Purpose: measure the reviewed final build after the index-v2 scaling and fallback-query fixes. This is the single final all-agent attempt, distinct from the preserved [earlier 900-second attempt](2026-11-16-real-900.md). The release binary was refreshed with `timeout 900 cargo build --release --locked` (exit 0). Binary SHA-256: `b7681b13566fb1ccfd5e84c381cd190804d4eab29bac3a3455cb4b33adee490f`. Hardware: Apple M1 Max, Darwin arm64; Rust 1.99.0, Python 3.11.6. No other benchmark/build/test workload ran concurrently; OS caches and unrelated background machine activity were uncontrolled.

Executed exactly:

```sh
timeout 1860 python3 scripts/benchmark.py --real --timeout 900 --total-timeout 1800 --repeat 3
```

[Aggregate evidence](2026-11-16-final-real-all.jsonl) records 29,611 files and 16,229,133,726 metadata bytes across all six selected agents. These root-inventory counts include companions and unsupported files; they are not parsed-session counts. Metadata sizing found zero unreadable paths and one absent OpenCode sidecar. Preflight passed with 279,421,784,064 free bytes against the conservative 196,897,088,360-byte requirement. The 2 GiB runtime reserve remained enforced.

Cold indexing did **not** complete: it reached the 900-second command deadline after **900.004849 seconds**. The driver killed/reaped the child and exited 2 with `TimeoutError`. Aggregate-only progress showed a 3,832,496,128-byte index at about 288.5 seconds, 8,259,002,368 bytes at about 619.1 seconds, and 11,047,084,032 bytes at about 882.8 seconds. Free space at the ten-minute checkpoint was 271,138,619,392 bytes. These are partial storage-growth observations, not completed timings or a forecast of remaining work.

No completed refresh envelope was received. Final discovered/reused/reparsed/failure counters, source diagnostic counts, complete source coverage, and all-agent warm/fallback query performance therefore remain **unestablished for the final build**. No sandbox denial or capacity stop was observed. This is a time-limited attempt, not a completed 900-second cold latency measurement, and no further all-agent retry followed.

The driver reported `index_exists: false` and `workspace_exists: false`; a separate post-exit check found zero `.local/benchmark-*` workspaces before the subset began. The real-content index was removed. Only aggregate evidence was retained; no original DB was opened by benchmark SQLite tooling, no source files were mutated, and the default XDG index was not used. Final Pi/OpenCode and synthetic records provide separate bounded evidence and do not replace this full-corpus limitation.
