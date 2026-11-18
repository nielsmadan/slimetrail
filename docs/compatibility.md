# Formats and behavior

Compatibility is checked with synthetic normalized-entry and query goldens generated from the supplied `check-agent-logs` reader, plus independent tests of indexing, roots, failures and query behavior. This establishes coverage for the fixture formats below, not every historical agent release. No real transcript is a fixture. Reference generators are in `tests/fixtures/jsonl/generate.py` and `tests/fixtures/query/generate.py`.

## Supported stores

| Agent | Supported data | Boundaries |
| --- | --- | --- |
| Claude Code | Project JSONL, messages/tools/metadata, child sessions and recognized Claude ringleader archive records | Mixed-archive foreign formats are not treated as Claude; no store mutations |
| Codex | Session/rollout JSONL, native archived sessions, header identity, title index, message/tool/reasoning records | Child ID is distinct from parent `session_id`; structured tool payload rendering differs from Python repr |
| Cursor | JSONL under `agent-transcripts`, companion `meta.json`, private chat `store.db` metadata | Legacy role-delimited `.txt` transcripts produce incomplete diagnostics; protobuf message blobs are not decoded; missing tool output is not reconstructed |
| Droid | Session JSONL and companion settings metadata | Automation/exec settings affect human prompt accounting and fingerprints |
| OpenCode | SQLite session/message/part schema, optional parent/archive fields and project cwd fallback | Legacy file-only storage is diagnosed unsupported; unsupported schema or unstable snapshot produces incomplete coverage |
| Pi | Session JSONL with canonical header ID, messages, tools and metadata | Filename guesses cannot replace a known header identity |

Required selected stores that are missing or unreadable are incomplete. The optional ringleader archive may be absent normally. Cursor metadata failures are explicit; cwd is never reconstructed from a lossy project slug. A subagent marker can mark entries automated without supplying a reliable `parent_ref`. Unknown records with meaningful searchable content can normalize to metadata; records without searchable content may be ignored. Stable malformed records preserve usable entries with diagnostics; repeated source churn excludes stale entries.

Store root and environment precedence is in the [README](../README.md#stores-and-index). Tests override Codex's home as well as its sessions directory so native archive lookup cannot escape to real stores. Stored cwd paths are expected to be absolute; a nonstandard stored `~` is not expanded. CLI `--cwd ~` is expanded from captured HOME.

## Search and read

Literal matching uses Unicode simple case folding by default; it is not accent normalization or full multi-character case folding. `--case-sensitive` disables folding. Regex mode uses Rust regex syntax, including supported inline flags; it has no look-around/backreferences. Empty literal queries are rejected. Zero-width regex matches are ignored. Matches are non-overlapping, counted separately per title/entry, and grouped by session.

Weights are native title 5, message 3, metadata 2 and tool 1 per occurrence. A native title is searched once even with an entry-kind filter. Derived titles are display-only. Score order breaks ties by last conversation timestamp descending, then agent/key; recent order breaks ties by score descending, then agent/key. Snippets follow document/entry order, with native titles first, rather than strongest-match order.

`--days` on search/sessions uses source freshness: file mtime for file-backed sessions and native update time for database-backed sessions. It does not mean "entry timestamps in the last N days." Cwd scope compares normalized lexical paths: current is equality, siblings adds equal parents, all also permits unknown cwd. `--app` is a case-insensitive cwd substring and takes precedence over scope. Filters run after a full refresh of selected agents, so narrowing the query does not skip source health checks.

Read rendering preserves original timestamps when available, emits timestamp plus `[role/kind]` headings, trims trailing whitespace from entry text, and separates blocks by blank lines. Lite includes messages, full includes messages/metadata, log includes all kinds. Character pagination preserves Unicode scalar boundaries; it is refreshed each call and does not pin a transcript revision. JSON exposes `truncated` and `next_offset`. Text pages keep the transcript slice on stdout and print `Transcript truncated; continue with --offset N` to stderr only when more text remains.

## Timeline

Timeline requires `--since` or positive `--days` (mutually exclusive). Entries are included in `[since, until)`; `--until` defaults to the captured current time. A bare until date includes that full local date by resolving the next civil midnight. Explicit offsets identify absolute instants; naive datetimes and date-only values use `--time-zone`, then `TZ`, then the IANA system zone. Ambiguous/nonexistent local datetimes are rejected with a request for an explicit offset. Local days can be 23 or 25 hours across DST; `--days N` remains exactly N × 24 hours ending at the resolved until.

Sessions must also have source freshness at least the resolved since. An old unmodified source can therefore be excluded even if its records overlap the requested window. Explicit historical until windows do not independently apply a cutoff relative to today. Entries without usable timestamps are skipped with one counted diagnostic per source, making the timeline incomplete.

An event gap at or below `--idle-minutes` stays in the same span; a larger gap starts another. The span ends at its last event, with no idle tail added; a single event has zero duration. Human prompts count only non-automated user message entries. Kind filters do not remove timeline events. Claude/Cursor child sessions with parent provenance or a `subagents` path are excluded from timeline to match reference behavior; search still includes them. Other harness child sessions remain included. A marker with no parent/path does not independently trigger that timeline exclusion.

Daily totals merge overlapping spans across all included sessions and split at civil-day boundaries in the selected zone. Summing session totals can exceed a day's union total because sessions overlap.

## Finite differences from the reference script

1. Default search is literal substring; reference regex behavior requires explicit `--regex` here. Rust regex syntax/simple folding can differ from Python, and zero-width matches are ignored.
2. The public output is a versioned CLI JSON envelope with explicit DTOs. Python consumers use subprocess JSON; importing the reference module is not the integration model.
3. All selected-source failures return incomplete coverage with exit 1, even when no usable sources remain. Fatal usage/index/runtime failures return 2. This replaces the script's differing all-agent and explicit-selector failure exits.
4. Non-string Codex tool arguments/results render as deterministic insertion-order JSON, not Python `repr`; existing string payloads remain strings.
5. Only recognized Claude records from the mixed ringleader archive are included there; foreign agent archive routing is deferred. Native Codex archives are supported.
6. Cursor legacy text and OpenCode legacy file storage are explicitly unsupported/incomplete. No guessed Cursor protobuf decoder or synthetic tool-output reconstruction is included.
7. Native `thinking`/`reasoning` content blocks become metadata with subtype `reasoning`. Content arrays retain adjacent message/reasoning/tool runs in source order, rather than flattening reasoning into messages and collecting tools separately. This changes emitted-item ordinals for mixed blocks; lite reads exclude reasoning and meta-only searches score it at weight 2.

These are specific compatibility boundaries, not an exemption for unlisted discrepancies. The crate declares MSRV 1.89. Local validation is macOS; the initial implementation also passed both Linux and macOS [CI checks](https://github.com/nielsmadan/slimetrail/actions/runs/44023048762). Current CI additionally checks the manifest MSRV and release targets. There is no Windows support claim.
