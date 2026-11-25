#!/usr/bin/env python3
import argparse
import contextlib
import hashlib
import json
import os
import platform
import random
import shutil
import signal
import sqlite3
import statistics
import subprocess
import tempfile
import time
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AGENTS = ("claude", "codex", "cursor", "devin", "droid", "opencode", "pi")
RESERVE = 2 * 1024**3
DIAGNOSTICS = {
    "source_unavailable",
    "unsupported_legacy_store",
    "unsupported_transcript",
    "metadata_unavailable",
    "malformed_metadata",
    "malformed_records",
    "source_changed",
    "partial_source",
    "incomplete_inventory",
    "duplicate_identity",
    "timeline_missing_timestamps",
    "search_coverage",
    "coverage_excluded",
    "index_required",
    "refresh_in_progress",
    "coverage_rebuild_pending",
    "fatal",
}


def emit(value):
    print(json.dumps(value, sort_keys=True), flush=True)


def roots(env):
    home = Path(env["HOME"])

    def value(key, default):
        return Path(env.get(key) or default)

    def cursor(key, default):
        return Path(env.get(key, "").strip() or default)

    codex = value("CODEX_HOME", home / ".codex")
    data = value("XDG_DATA_HOME", home / ".local/share")
    sessions = value("CODEX_SESSIONS_DIR", codex / "sessions")
    return {
        "claude": [
            value(
                "SESSION_TRACKER_PROJECTS_DIR",
                value("CLAUDE_CONFIG_DIR", home / ".claude") / "projects",
            ),
            data / "ringleader/archive",
        ],
        "codex": [
            sessions,
            codex / "archived_sessions",
            sessions.parent / "session_index.jsonl"
            if env.get("CODEX_SESSIONS_DIR")
            else codex / "session_index.jsonl",
        ],
        "cursor": [
            cursor("CURSOR_DATA_DIR", home / ".cursor") / "projects",
            cursor(
                "CURSOR_CONFIG_DIR",
                cursor("XDG_CONFIG_HOME", home / ".config") / "cursor"
                if env.get("XDG_CONFIG_HOME", "").strip()
                else home / ".cursor",
            )
            / "chats",
        ],
        "devin": [value("DEVIN_SESSIONS_DIR", data / "devin") / "cli/sessions.db"],
        "droid": [value("FACTORY_HOME_OVERRIDE", home) / ".factory/sessions"],
        "pi": [
            value(
                "PI_SESSIONS_DIR",
                value(
                    "PI_CODING_AGENT_SESSION_DIR",
                    value("PI_CODING_AGENT_DIR", home / ".pi/agent") / "sessions",
                ),
            )
        ],
        "opencode": [value("OPENCODE_DB", data / "opencode/opencode.db")],
    }


def dimensions(paths):
    counts = Counter(files=0, bytes=0, unreadable=0, missing=0)
    seen = set()

    def count(path):
        try:
            if path.is_symlink() or path in seen:
                return
            seen.add(path)
            stat = path.stat()
            if path.is_file():
                counts.update(files=1, bytes=stat.st_size)
        except FileNotFoundError:
            counts.update(missing=1)
        except OSError:
            counts.update(unreadable=1)

    for path in paths:
        if path.is_dir() and not path.is_symlink():
            for directory, _, files in os.walk(
                path, followlinks=False, onerror=lambda _: counts.update(unreadable=1)
            ):
                for name in files:
                    count(Path(directory) / name)
        else:
            count(path)
            if path.suffix == ".db":
                for suffix in ("-wal", "-shm", "-journal"):
                    count(Path(str(path) + suffix))
    return dict(counts)


TIMESTAMP = "2026-11-16T00:00:00Z"


def tool_output(rng):
    """Seeded mix: 60% under 1 KB, 30% 1-16 KB, 10% 16-256 KB."""
    roll = rng.random()
    if roll < 0.6:
        size = rng.randint(64, 1023)
    elif roll < 0.9:
        size = rng.randint(1024, 16 * 1024)
    else:
        size = rng.randint(16 * 1024, 256 * 1024)
    line = "tool output line with ordinary log content\n"
    return (line * (size // len(line) + 1))[:size]


def events(agent, number, entries):
    """Deterministic per-session event stream shared by every agent's writer."""
    rng = random.Random(f"{agent}:{number}")
    for entry in range(entries):
        yield {"kind": "user", "text": payload(entry)}
        phase = entry % 10
        if phase == 2:
            yield {
                "kind": "reasoning",
                "text": f"Considering ordinary approach {entry}",
            }
        elif phase == 3:
            yield {
                "kind": "call",
                "id": f"call-{entry}",
                "name": "Shell",
                "input": {"command": f"ls -la entry-{entry}"},
            }
        elif phase == 4:
            yield {
                "kind": "result",
                "id": f"call-{entry - 1}",
                "text": tool_output(rng),
            }


def blocks(event):
    kind = event["kind"]
    if kind == "user":
        return "user", [{"type": "text", "text": event["text"]}]
    if kind == "reasoning":
        return "assistant", [{"type": "thinking", "thinking": event["text"]}]
    if kind == "call":
        return "assistant", [
            {
                "type": "tool_use",
                "id": event["id"],
                "name": event["name"],
                "input": event["input"],
            }
        ]
    return "user", [
        {"type": "tool_result", "tool_use_id": event["id"], "content": event["text"]}
    ]


def jsonl_record(agent, event):
    kind = event["kind"]
    if agent == "codex":
        if kind == "user":
            payload_ = {"type": "message", "role": "user", "content": event["text"]}
        elif kind == "reasoning":
            payload_ = {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": event["text"]}],
            }
        elif kind == "call":
            payload_ = {
                "type": "function_call",
                "name": "exec",
                "call_id": event["id"],
                "arguments": json.dumps({"cmd": event["input"]["command"]}),
            }
        else:
            payload_ = {
                "type": "function_call_output",
                "call_id": event["id"],
                "output": event["text"],
            }
        return {"type": "response_item", "timestamp": TIMESTAMP, "payload": payload_}
    role, content = blocks(event)
    if agent == "claude":
        if kind == "user":
            message = {"role": "user", "content": event["text"]}
        else:
            message = {"role": role, "content": content}
        return {"type": role, "timestamp": TIMESTAMP, "message": message}
    if agent == "cursor":
        return {
            "role": role,
            "timestamp": TIMESTAMP,
            "message": {"content": content},
        }
    if agent == "pi" and kind == "result":
        message = {
            "role": "toolResult",
            "toolCallId": event["id"],
            "toolName": "read",
            "content": [{"type": "text", "text": event["text"]}],
        }
    elif agent == "pi" and kind == "call":
        message = {
            "role": "assistant",
            "content": [
                {
                    "type": "toolCall",
                    "id": event["id"],
                    "name": "read",
                    "arguments": event["input"],
                }
            ],
        }
    elif kind == "user":
        message = {"role": "user", "content": event["text"]}
    else:
        message = {"role": role, "content": content}
    return {"type": "message", "timestamp": TIMESTAMP, "message": message}


def write_opencode(database, sessions, entries):
    database.parent.mkdir(parents=True, exist_ok=True)
    with contextlib.closing(sqlite3.connect(database)) as connection:
        connection.executescript(
            "CREATE TABLE session(id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, time_created INTEGER, data TEXT);"
        )
        for number in range(sessions):
            identity = f"bench-{number:05}"
            connection.execute(
                "INSERT INTO session VALUES(?,?,?,?,?)",
                (
                    identity,
                    "/fixture/project",
                    "Benchmark",
                    1794787200000,
                    1794787200000,
                ),
            )
            count = 0
            pending = {}
            for event in events("opencode", number, entries):
                kind = event["kind"]
                if kind == "call":
                    pending[event["id"]] = event["input"]
                    continue
                role = "user" if kind == "user" else "assistant"
                if kind == "user":
                    part = {"type": "text", "text": event["text"]}
                elif kind == "reasoning":
                    part = {"type": "reasoning", "text": event["text"]}
                else:
                    part = {
                        "type": "tool",
                        "tool": "shell",
                        "state": {
                            "input": pending.pop(event["id"], {}),
                            "output": event["text"],
                        },
                    }
                item = f"{identity}-{count}"
                count += 1
                connection.execute(
                    "INSERT INTO message VALUES(?,?,?,?)",
                    (item, identity, 1794787200000, json.dumps({"role": role})),
                )
                connection.execute(
                    "INSERT INTO part VALUES(?,?,?,?)",
                    (item, item, 1794787200000, json.dumps(part)),
                )
        connection.commit()


def devin_message(role, source, content):
    return {
        "role": role,
        "content": content,
        "metadata": {
            "created_at": "2026-08-10T10:00:00Z",
            "is_user_input": True if source == "user" else None,
            "telemetry": {"source": source},
        },
    }


def write_devin(database, sessions, entries):
    database.parent.mkdir(parents=True, exist_ok=True)
    with contextlib.closing(sqlite3.connect(database)) as connection:
        connection.executescript(
            "PRAGMA journal_mode=WAL;"
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, working_directory TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER);"
            "CREATE TABLE message_nodes(row_id INTEGER PRIMARY KEY, session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, created_at INTEGER, metadata TEXT);"
            "CREATE TABLE subagent_heads(session_id TEXT, agent_id TEXT, chain_node_id INTEGER, updated_at INTEGER, PRIMARY KEY(session_id, agent_id)) WITHOUT ROWID;"
        )
        for number in range(sessions):
            identity = f"bench-{number:05}"
            chain = [
                (
                    devin_message("system", "sysprompt", "System prompt"),
                    {"is_system_prefix": True},
                )
            ]
            for event in events("devin", number, entries):
                kind = event["kind"]
                if kind == "user":
                    chat = devin_message("user", "user", event["text"])
                elif kind == "reasoning":
                    chat = devin_message("assistant", "assistant", "")
                    chat["thinking"] = {"thinking": event["text"], "signature": "sig"}
                elif kind == "call":
                    chat = devin_message("assistant", "assistant", "")
                    chat["tool_calls"] = [
                        {
                            "id": event["id"],
                            "name": "exec",
                            "arguments": event["input"],
                        }
                    ]
                else:
                    chat = devin_message("tool", "tool_result", event["text"])
                    chat["tool_call_id"] = event["id"]
                chain.append((chat, {}))
            connection.execute(
                "INSERT INTO sessions VALUES(?,?,?,?,?,?,0)",
                (
                    identity,
                    "/fixture/project",
                    "Benchmark",
                    1786356000,
                    1786356300,
                    len(chain) - 1,
                ),
            )
            connection.executemany(
                "INSERT INTO message_nodes(session_id, node_id, parent_node_id, chat_message, created_at, metadata) VALUES(?,?,?,?,?,?)",
                [
                    (
                        identity,
                        node,
                        node - 1 if node else None,
                        json.dumps(chat),
                        1786356000 + node,
                        json.dumps(metadata),
                    )
                    for node, (chat, metadata) in enumerate(chain)
                ],
            )
        connection.commit()


def synthetic(base, sessions, entries):
    home = base / "home"
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(home), "TZ": "UTC"}
    stores = roots(env)
    changed = None
    for agent in AGENTS:
        if agent in ("opencode", "devin"):
            continue
        for number in range(sessions):
            identity = f"bench-{number:05}"
            path = stores[agent][0] / f"project/{identity}.jsonl"
            if agent == "cursor":
                path = (
                    stores[agent][0]
                    / f"project/agent-transcripts/{identity}/{identity}.jsonl"
                )
                meta = stores[agent][1] / f"project/{identity}/meta.json"
                meta.parent.mkdir(parents=True, exist_ok=True)
                meta.write_text(
                    json.dumps(
                        {
                            "cwd": "/fixture/project",
                            "title": "Benchmark",
                            "createdAtMs": 1794787200000,
                            "updatedAtMs": 1794787200000,
                        }
                    )
                )
            path.parent.mkdir(parents=True, exist_ok=True)
            headers = {
                "claude": {
                    "type": "system",
                    "sessionId": identity,
                    "cwd": "/fixture/project",
                },
                "codex": {
                    "type": "session_meta",
                    "payload": {"id": identity, "cwd": "/fixture/project"},
                },
                "droid": {
                    "type": "session_start",
                    "id": identity,
                    "cwd": "/fixture/project",
                    "title": "Benchmark",
                },
                "pi": {
                    "type": "session",
                    "id": identity,
                    "cwd": "/fixture/project",
                    "name": "Benchmark",
                },
            }
            with path.open("w") as stream:
                if agent in headers:
                    stream.write(json.dumps(headers[agent]) + "\n")
                for event in events(agent, number, entries):
                    stream.write(json.dumps(jsonl_record(agent, event)) + "\n")
            if agent == "claude" and number == 0:
                changed = path
    write_opencode(stores["opencode"][0], sessions, entries)
    write_devin(stores["devin"][0], sessions, entries)
    return env, changed


def payload(entry):
    marker = "needle" if entry % 10 == 0 else "café" if entry % 10 == 1 else "ordinary"
    return f"{marker} record {entry} " + "synthetic benchmark content " * 8


def aggregate_index(index):
    databases = [
        path
        for path in index.glob("index-v*.sqlite3")
        if path.name.removeprefix("index-v").removesuffix(".sqlite3").isdigit()
    ]
    if not databases:
        return {}
    path = max(
        databases,
        key=lambda path: int(
            path.name.removeprefix("index-v").removesuffix(".sqlite3")
        ),
    )
    if not path.exists():
        return {}
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        return {
            "sessions": connection.execute("SELECT count(*) FROM sessions").fetchone()[
                0
            ],
            "entries": connection.execute("SELECT count(*) FROM entries").fetchone()[0],
            "entry_bytes": connection.execute(
                "SELECT coalesce(sum(length(cast(text AS BLOB))),0) FROM entries"
            ).fetchone()[0],
            "documents": connection.execute(
                "SELECT count(*) FROM search_documents"
            ).fetchone()[0],
            "non_ascii_documents": connection.execute(
                "SELECT count(*) FROM search_documents WHERE has_non_ascii=1"
            ).fetchone()[0],
            "nul_documents": connection.execute(
                "SELECT count(*) FROM search_documents WHERE has_nul=1"
            ).fetchone()[0],
            "index_bytes": path.stat().st_size,
        }


def index_database(index):
    databases = [
        path
        for path in index.glob("index-v*.sqlite3")
        if path.name.removeprefix("index-v").removesuffix(".sqlite3").isdigit()
    ]
    return max(
        databases,
        key=lambda path: int(
            path.name.removeprefix("index-v").removesuffix(".sqlite3")
        ),
        default=None,
    )


def agent_dimensions(index, agent):
    """Aggregate counts and DB+WAL bytes after a TRUNCATE checkpoint (owned index only)."""
    path = index_database(index)
    if path is None:
        return {}
    with contextlib.closing(sqlite3.connect(path)) as connection:
        sessions = connection.execute(
            "SELECT count(*) FROM sessions WHERE agent=?", (agent,)
        ).fetchone()[0]
        entries = connection.execute(
            "SELECT count(*) FROM entries WHERE agent=?", (agent,)
        ).fetchone()[0]
        busy = connection.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0]
        if busy:
            raise RuntimeError("checkpoint_busy")
    wal = Path(str(path) + "-wal")
    return {
        "sessions": sessions,
        "entries": entries,
        "index_bytes": path.stat().st_size
        + (wal.stat().st_size if wal.exists() else 0),
    }


def invoke(binary, index, env, args, timeout, deadline):
    started = time.monotonic()
    command = [
        str(binary),
        "--index-dir",
        str(index),
        "--format",
        "json",
        "--time-zone",
        "UTC",
        "--scope",
        "all",
        *args,
    ]
    capture = args[0] == "index"
    process = subprocess.Popen(
        command,
        env=env,
        stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        while True:
            remaining = min(
                timeout - (time.monotonic() - started), deadline - time.monotonic()
            )
            if remaining <= 0:
                raise TimeoutError
            if shutil.disk_usage(index.parent).free < RESERVE:
                raise RuntimeError("disk_reserve")
            try:
                stdout, stderr = process.communicate(timeout=min(0.5, remaining))
                break
            except subprocess.TimeoutExpired:
                continue
    finally:
        if process.poll() is None:
            process.kill()
            process.communicate()
    codes = Counter()
    for line in stderr.decode("utf-8", errors="replace").splitlines():
        code = line.split(":", 1)[0]
        codes[code if code in DIAGNOSTICS else "other"] += 1
    result = {
        "seconds": round(time.monotonic() - started, 6),
        "exit": process.returncode,
        "diagnostic_counts": dict(codes),
    }
    if capture:
        try:
            envelope = json.loads(stdout)
            result["incomplete"] = bool(envelope["incomplete"])
            result["counters"] = (
                {
                    key: envelope["data"][key]
                    for key in (
                        "discovered",
                        "reused",
                        "reparsed",
                        "removed",
                        "failed",
                        "elapsed_ms",
                    )
                    if isinstance(envelope.get("data", {}).get(key), int)
                }
                if envelope.get("data")
                else {}
            )
        except (ValueError, KeyError, TypeError):
            result["invalid_envelope"] = True
    return result


def main():
    parser = argparse.ArgumentParser(
        description="Aggregate CLI benchmark; synthetic by default; real stores require --real."
    )
    parser.add_argument("--real", action="store_true")
    parser.add_argument("--agents", nargs="+", choices=AGENTS, default=list(AGENTS))
    parser.add_argument(
        "--sessions", type=int, default=50, help="Synthetic sessions per agent"
    )
    parser.add_argument(
        "--entries", type=int, default=200, help="Synthetic entries per session"
    )
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument(
        "--coverage",
        choices=("compact", "full"),
        default="compact",
        help="Index coverage, written to a workspace-local config",
    )
    parser.add_argument(
        "--per-agent",
        action="store_true",
        help="Also run one cold index per agent into a fresh index",
    )
    parser.add_argument(
        "--timeout", type=int, default=180, help="Per subprocess deadline in seconds"
    )
    parser.add_argument("--total-timeout", type=int, default=840)
    parser.add_argument(
        "--capacity-factor",
        type=int,
        default=12,
        help="Required free space as a multiple of source bytes",
    )
    args = parser.parse_args()
    if (
        min(
            args.sessions,
            args.entries,
            args.repeat,
            args.timeout,
            args.total_timeout,
            args.capacity_factor,
        )
        <= 0
    ):
        parser.error("numeric options must be positive")
    binary = ROOT / "target/release/slimetrail"
    private = ROOT / ".local"
    private.mkdir(mode=0o700, exist_ok=True)
    if private.is_symlink() or private.stat().st_mode & 0o777 != 0o700:
        raise RuntimeError("private_directory_mode")
    deadline = time.monotonic() + args.total_timeout
    workspace = Path(tempfile.mkdtemp(prefix="benchmark-", dir=private))
    index = workspace / "index"
    try:
        env, changed = (
            (dict(os.environ), None)
            if args.real
            else synthetic(workspace, args.sessions, args.entries)
        )
        config = workspace / "config.toml"
        config.write_text(f'coverage = "{args.coverage}"\n')
        env["SLIMETRAIL_CONFIG"] = str(config)
        corpus = {
            agent: dimensions(paths)
            for agent, paths in roots(env).items()
            if agent in args.agents
        }
        size = sum(item["bytes"] for item in corpus.values())
        free = shutil.disk_usage(workspace).free
        emit(
            {
                "mode": "real" if args.real else "synthetic",
                "agents": args.agents,
                "platform": platform.system(),
                "machine": platform.machine(),
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "repetitions": args.repeat,
                "coverage": args.coverage,
                "per_agent": args.per_agent,
                "corpus": corpus,
                "free_bytes_before": free,
                "required_free_bytes": RESERVE + size * args.capacity_factor,
            }
        )
        if free < RESERVE + size * args.capacity_factor:
            raise RuntimeError("insufficient_disk_capacity")

        def measure(label, command, repetitions=1):
            selection = (
                []
                if any("--" + agent in command for agent in AGENTS)
                else ["--" + agent for agent in args.agents]
            )
            started = time.monotonic()
            try:
                runs = [
                    invoke(
                        binary, index, env, command + selection, args.timeout, deadline
                    )
                    for _ in range(repetitions)
                ]
            except (Exception, KeyboardInterrupt) as error:
                emit(
                    {
                        "measurement": label,
                        "failure": type(error).__name__,
                        "elapsed_seconds": round(time.monotonic() - started, 6),
                    }
                )
                raise
            emit(
                {
                    "measurement": label,
                    "runs": runs,
                    "median_seconds": round(
                        statistics.median(run["seconds"] for run in runs), 6
                    ),
                }
            )
            if any(run["exit"] not in (0, 1) for run in runs):
                raise RuntimeError("cli_fatal")

        measure("cold_index", ["index"])
        emit({"indexed_dimensions": aggregate_index(index)})
        measure("warmup_index", ["index"])
        measure("warm_index", ["index"], args.repeat)
        if "opencode" in args.agents:
            measure("warm_opencode_inventory", ["index", "--opencode"], args.repeat)
        if "claude" in args.agents:
            measure("warm_claude_inventory", ["index", "--claude"], args.repeat)
        for label, query in (
            ("ascii_literal", ["needle"]),
            ("short_literal", ["ed"]),
            ("non_ascii_literal", ["café"]),
            ("regex", ["needle|café", "--regex"]),
        ):
            command = ["search", *query, "--max-snippets", "0"]
            measure(label + "_warmup", command)
            measure(label, command, args.repeat)
        if changed and "claude" in args.agents:
            with changed.open("a") as stream:
                stream.write(
                    json.dumps(
                        {
                            "type": "user",
                            "timestamp": "2026-11-16T00:01:00Z",
                            "message": {"content": "needle changed synthetic source"},
                        }
                    )
                    + "\n"
                )
            measure("one_changed_source", ["index"])
        else:
            emit({"one_changed_source": "synthetic_only"})
        if args.per_agent:
            for agent in args.agents:
                fresh = workspace / f"index-{agent}"
                started = time.monotonic()
                try:
                    run = invoke(
                        binary,
                        fresh,
                        env,
                        ["index", "--" + agent],
                        args.timeout,
                        deadline,
                    )
                    record = {
                        "per_agent": agent,
                        "coverage": args.coverage,
                        "source_files": corpus[agent]["files"],
                        "source_bytes": corpus[agent]["bytes"],
                        "cold_ms": round(run["seconds"] * 1000),
                        "exit": run["exit"],
                        "incomplete": run.get("incomplete"),
                        "diagnostic_counts": run["diagnostic_counts"],
                        "counters": run.get("counters", {}),
                        **agent_dimensions(fresh, agent),
                    }
                    if run.get("invalid_envelope"):
                        record["invalid_envelope"] = True
                except (Exception, KeyboardInterrupt) as error:
                    emit(
                        {
                            "per_agent": agent,
                            "failure": type(error).__name__,
                            "elapsed_seconds": round(time.monotonic() - started, 6),
                        }
                    )
                    raise
                finally:
                    shutil.rmtree(fresh, ignore_errors=True)
                emit(record)
                if run["exit"] not in (0, 1):
                    raise RuntimeError("cli_fatal")
    finally:
        shutil.rmtree(workspace)
        emit(
            {
                "cleanup": "removed_owned_workspace",
                "index_exists": index.exists(),
                "workspace_exists": workspace.exists(),
            }
        )


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    try:
        main()
    except (Exception, KeyboardInterrupt) as error:
        emit(
            {
                "benchmark_error": type(error).__name__,
                "reason": str(error)
                if isinstance(error, RuntimeError)
                and str(error)
                in {
                    "disk_reserve",
                    "private_directory_mode",
                    "insufficient_disk_capacity",
                    "cli_fatal",
                }
                else "details_suppressed",
            }
        )
        raise SystemExit(2)
