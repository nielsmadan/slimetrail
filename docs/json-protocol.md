# JSON protocol, version 1

Pass `--format json` to any command. Stdout contains one JSON document plus a newline; diagnostics also appear on stderr as `code: [agent source] message`, omitting unavailable identity fields. Object key order is not part of the contract. Version 1 describes the CLI DTOs, independently of crate and SQLite schema versions. Internal Rust models are not the wire format.

```json
{"version":1,"incomplete":false,"diagnostics":[],"data":{"count":0,"sessions":[]}}
```

| Field | Meaning |
| --- | --- |
| `version` | Integer protocol version, currently `1` |
| `incomplete` | Coverage was lost or a fatal error occurred; inspect diagnostics and exit status |
| `diagnostics` | Array of `{agent: string|null, source: string|null, code: string, message: string}` |
| `data` | Command-specific value, or `null` for fatal failures before data and unavailable exact reads |

Exit **0** is complete success, including empty results. Exit **1** is partial source coverage, even if every selected store failed. Exit **2** is invalid arguments or fatal runtime/index failure. Runtime fatal JSON has `incomplete: true`, `data: null` and a `fatal` diagnostic. CLI argument parsing errors can instead use Clap's stderr usage output without a JSON envelope. A closed stdout pipe is handled quietly with exit 0; it does not establish that a downstream consumer received a full document.

A query that cannot start its refresh within 5 s because another slimetrail process is refreshing the same index answers from the last published index state instead of failing. Its envelope has `incomplete: true`, exit 1, and one `refresh_in_progress` diagnostic with `agent: null` and `source: null`; sessions indexed or changed by the other refresh may be missing or stale. Query commands do not report an index-busy failure for this. `slimetrail index` waits up to an hour for the other refresh, and that wait timing out is a fatal exit 2. Any command can also fail with an index-busy exit 2 if it cannot take the shared lifecycle lock within 5 s, which happens only while an older slimetrail binary (or an index-file cleanup) holds it exclusively.

Queries build inline only up to an estimated cost: 20 s for non-interactive callers. Each selected agent's pending work is estimated before anything is written, and agents are admitted smallest first while the total stays within the limit. Each agent left out gets one `index_required` diagnostic with that `agent` and `source: null`. Its message gives the pending sessions, their log bytes, the estimated time and index size for the index's coverage, and says to run `slimetrail index`. The envelope has `incomplete: true` and exit 1. A gated agent that was built before answers from its last published state; one that was never built contributes nothing. When no admitted agent has data, `data` is the command's empty shape: search `{"query", "agents", "sort", "count": 0, "results": []}`, sessions `{"count": 0, "sessions": []}`, timeline `{"sessions": [], "daily_active_minutes": {}}`, read `null`, and entries `{"entries": [], "session": null}`. Interactive terminal runs use a 120 s limit and gate the job as a whole: if any selected agent is over it, the command writes and prints nothing, prompts on stderr, then runs once more, either building everything inline or under the per-agent gate above. JSON output is never interactive. `slimetrail index` is never gated.

Two diagnostic codes are informational and leave `incomplete: false` and exit 0. Every search on a compact-coverage index adds `search_coverage`, which names the text that is not searched: reasoning, and the part of a tool result before its last 2 KB. A search whose explicit `--kind` set contains `meta` or `tool` gets `coverage_excluded` instead, which says which requested kind is limited. Both have `agent: null` and `source: null`. When the config's coverage differs from the index's, the message also says so and asks you to run `slimetrail index`. On a full-coverage index, that mismatch is the only case that adds a `search_coverage` note. While a coverage rebuild is unfinished, every query answers with `coverage_rebuild_pending` (`agent: null`, `source: null`), `incomplete: true` and exit 1, and its results may be missing matches because the search documents are being rebuilt. That state persists until `slimetrail index` completes the rebuild, so retrying the query does not clear it; `refresh_in_progress` is reported only while another process holds the writer lock. Completeness is signalled by `incomplete`, not by an empty `diagnostics` array.

For an exact missing session, a complete inventory produces a fatal not-found argument error. If the inventory is partial, `read` returns incomplete `data: null`; `entries` returns empty entries with `session: null`. Never interpret missing data from partial discovery as proof of absence. Diagnostic codes/messages are operational details; do not equate an empty result with complete coverage without checking `incomplete` and process status.

## Shared session fields

Search defaults to human-readable text blocks. That display format escapes terminal controls and bidirectional formatting controls; JSON preserves the original field values. Consumers should explicitly request `--format json` and use this versioned protocol rather than parse text output.

Session objects contain the following fields. Search hits, read pages and timeline sessions flatten these fields into their own object, without a nested `session` member.

| Field | Type / meaning |
| --- | --- |
| `agent` | `claude`, `codex`, `cursor`, `devin`, `droid`, `opencode`, `pi` |
| `id`, `ref` | Strings; `ref` is exact `agent:id` |
| `source` | Source path/provenance string; database sources may include a session locator |
| `title`, `cwd`, `branch` | String or null |
| `first_ts`, `last_ts` | RFC3339 UTC conversation timestamp or null |
| `parent_ref` | Exact parent `agent:id` or null; unknown parent identity stays null |
| `archived` | Boolean |

Source freshness (`Session.source_updated_at`, often called source mtime) is internal and distinct from `last_ts`; it drives date filters but is not currently a JSON session field. Raw timestamps are retained for entries/rendered read text; session bounds are normalized instants.

## Command data

| Command | `data` shape |
| --- | --- |
| `sessions` | `{count, sessions: [Session]}`; count is returned sessions |
| `index` | `{discovered, reused, reparsed, removed, failed, elapsed_ms}`; integer refresh counters and internal elapsed milliseconds |
| `search` | `{query, agents: [Agent], sort: "score"|"recent", count, results: [SearchHit]}` |
| `read` | Session fields plus `{mode, body, offset, next_offset, total_chars, returned_chars, truncated}` |
| `entries` | `{entries: [Entry], session: Session|null}` |
| `timeline` | `{sessions: [TimelineSession], daily_active_minutes: {"YYYY-MM-DD": number}}` |

`SearchHit` adds `score`, `matches`, `by_kind` (a map of occurrence counts, including `title`), and `snippets` (strings labelled `[title]` or `[role/kind]`). `count` is the number of returned session hits after `max_results`, not an uncapped total. Scores sum occurrence weights; see [matching rules](compatibility.md). Snippets can contain sensitive text.

`Read` mode is `lite`, `full` or `log`. `body` is the rendered page, `offset` echoes the requested start, and `next_offset` is an integer only when `truncated` is true, otherwise null. Counts and offsets are Unicode scalar values, matching Python string indexing for ordinary Unicode text, not UTF-8 bytes or grapheme clusters. Concatenate pages by following `next_offset` while retaining the same mode. Pages refer to the newly refreshed transcript; a rewrite between calls can change offsets. There are no revision-pinned continuation tokens. Beyond-end offsets return an empty untruncated page with the full `total_chars`.

`Entry` fields are `index` (dense integer), `source_id` (string), `timestamp` (original timestamp string when present, otherwise RFC3339 fallback, or null), `timestamp_ms` (UTC epoch milliseconds or null), `role` (string), `kind` (`message`|`tool`|`meta`), `subtype` and `tool_name` (string or null), `automated` (boolean), and `text` (string). A raw timestamp can exist while its normalized `timestamp_ms` is null. Reasoning/compaction are represented through metadata/subtype, not additional top-level kinds. Stable source IDs do not promise identity through rewrites.

`TimelineSession` adds `spans`, `active_minutes`, `start`, `end`. Each span is `{start: RFC3339, end: RFC3339, minutes: number, prompts: integer}`. Session start/end describe its emitted spans. Daily keys use the selected IANA time zone, and values union overlapping spans across sessions; they are not a sum that double counts concurrency. Durations are floating-point minutes, with no rounding guarantee.

## Streaming entries and consumer handling

`entries` writes `data.entries` first, then session metadata, then the envelope's `incomplete` and `diagnostics`. Consumers must read through the closing envelope and check the exit code before accepting completeness. An error after entries have been emitted finalizes partial data with `session: null`, `incomplete: true`, a fatal diagnostic and exit 2. It cannot retract bytes already written. A fatal error before emission uses the normal `data: null` envelope. Abrupt process termination or output failure can still leave truncated JSON.

Python integrations should use `subprocess.run([...], capture_output=True, text=True)` without `check=True`, inspect the exit code, and parse stdout only when a complete JSON document is present. Avoid shell interpolation of queries. For large `entries` streams, use a streaming JSON consumer and wait for the final status fields. Source paths, titles, identifiers and entry text are private user data; do not log the full envelope by default.
