# Consumers and migration boundaries

Slimetrail owns read-only discovery, normalized entries and indexed search over supported agent stores. External consumers use the [version-1 CLI JSON protocol](json-protocol.md). Consumer migrations are deferred beyond v1 and should proceed one consumer at a time; this checkout does not change their integrations.

## Consumer map

This map records the integration landscape identified during the 2026-11-16 design. Paths are relative to the named repositories. Recheck each consumer before migrating it.

| Consumer | Reference location | Responsibility |
| --- | --- | --- |
| ringleader scan / registry | ringleader: `ringleader/sources/` | Session metadata: cwd, title, freshness and git information |
| `rl analyze` | ringleader: `ringleader/analysis/claude.py`, `codex.py` | Detailed requests, tool calls, rewinds and compactions for Claude and Codex |
| `rl usage` | ringleader: `ringleader/usage/` | Tokens and cost for five agents, excluding Cursor at design time |
| check-agent-logs | loadout: `skills/check-agent-logs/scripts/search_agent_logs.py` | Classified entries, search, read and timeline |
| time-reconstruct | loadout: `skills/time-reconstruct/scripts/reconstruct.py` | Session spans, obtained by importing the check-agent-logs script |
| circus | ringleader's `rl --json` interface | Indirect session consumption through ringleader |

Ringleader retains its registry, classification, token/cost accounting and all native-store mutations: rename, hide, archive, delete, companion-file moves, workspace relocation/rebinding, launch and fork. Slimetrail does not implement those capability protocols. GitHub binary releases follow the [release workflow](development-and-releases.md). Consumer distribution work, including Homebrew changes, remains separate.

## Reference implementations

These were the design and compatibility sources; their locations identify upstream code to consult, not runtime dependencies:

- Ringleader's `ringleader/sources/base.py`: the read half of `Source` (`discover`, `parse`, `RawRef`, `NormalizedSession`). Its six readers are `claude_code.py`, `codex.py`, `cursor.py`, `droid.py`, `opencode.py` and `pi.py`; associated helpers include `slug.py`, `opencode_db.py` and `transcript.py`.
- Loadout's `skills/check-agent-logs/scripts/search_agent_logs.py`: `Entry`, `SessionMeta`, per-agent entry readers, `read_jsonl_session`, `read_opencode_session`, search/rendering and activity spans. Its `test_search_agent_logs.py` and ringleader's `ringleader/devtools/` provide synthetic-fixture precedents.
- Ringleader's `ringleader/analysis/store.py`, `build.py`, `ingest.py` and `docs/tech/analysis.md`: versioned, disposable index files, fingerprint invalidation, locking and safe old-version pruning.
- Ringleader's `docs/reference/agent-session-management.md` and `docs/reference/coding-agent-session-search.md`: the original format and search research.

Current slimetrail behavior is defined by [architecture](architecture.md), [compatibility](compatibility.md) and the JSON protocol. In particular, the original SQLite readers are not a safety template: slimetrail opens only private database snapshots through SQLite, as described in [private snapshots](architecture.md#private-sqlite-snapshots).

## Migration checks

Invoke the binary with an argument array and consume the JSON envelope and exit status together. Exit 1 means partial source coverage and may still contain useful results; it must not become a complete empty result in a wrapper. Pin the consumer's expected protocol version independently of the private index schema.

Consumers such as check-agent-logs, time-reconstruct and ringleader should treat an `index_required` diagnostic as "ask the user to run `slimetrail index`": the named agents have pending work over the query's build limit, so their sessions are missing or stale. `refresh_in_progress` means another slimetrail process is refreshing; the result is usable but incomplete, and retrying later gives a fuller answer. `coverage_rebuild_pending` means a coverage rebuild was interrupted; the result is usable but incomplete, retrying does not help, and the user should run `slimetrail index` to finish it. Pass `--format json` (or set `CI`): a text-format run whose stdin and stderr are terminals may stop at a build prompt. A Codex title index (`session_index.jsonl`) that cannot be read or holds malformed records adds one agent-level `source_unavailable` or `malformed_metadata` diagnostic on every refresh until it is fixed; sessions keep their last indexed titles meanwhile. Read completeness from `incomplete` and the exit code, never from an empty `diagnostics` array, because the informational `search_coverage` and `coverage_excluded` codes appear on complete results.

Use the [finite compatibility differences](compatibility.md#finite-differences-from-the-reference-script) when comparing old and new readers. Preserve synthetic expected-output fixtures while both implementations coexist; never add real transcripts to the repository. The generators at `tests/fixtures/jsonl/generate.py` and `tests/fixtures/query/generate.py` take the reference `search_agent_logs.py` path as their first argument. Regeneration requires reviewing intentional differences, rather than treating the reference output as authoritative for every field.

Before replacing a consumer's reader, establish that its required metadata or analysis facts are represented by the public protocol. Normalized search entries do not by themselves promise parity with ringleader's detailed analysis or usage readers.
