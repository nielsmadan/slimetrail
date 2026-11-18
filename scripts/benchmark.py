#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import platform
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
AGENTS = ("claude", "codex", "cursor", "droid", "opencode", "pi")
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


def synthetic(base, sessions, entries):
    home = base / "home"
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(home), "TZ": "UTC"}
    stores = roots(env)
    timestamp = "2026-11-16T00:00:00Z"
    changed = None
    for agent in AGENTS:
        if agent == "opencode":
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
                for entry in range(entries):
                    text = payload(entry)
                    message = {"role": "user", "content": text}
                    record = {
                        "type": "message",
                        "timestamp": timestamp,
                        "message": message,
                    }
                    if agent == "claude":
                        record["type"] = "user"
                    elif agent == "codex":
                        record = {
                            "type": "response_item",
                            "timestamp": timestamp,
                            "payload": {"type": "message", **message},
                        }
                    elif agent == "cursor":
                        record = {
                            "role": "user",
                            "timestamp": timestamp,
                            "message": {"content": [{"type": "text", "text": text}]},
                        }
                    stream.write(json.dumps(record) + "\n")
            if agent == "claude" and number == 0:
                changed = path
    database = stores["opencode"][0]
    database.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(database) as connection:
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
            for entry in range(entries):
                item = f"{identity}-{entry}"
                connection.execute(
                    "INSERT INTO message VALUES(?,?,?,?)",
                    (item, identity, 1794787200000, '{"role":"user"}'),
                )
                connection.execute(
                    "INSERT INTO part VALUES(?,?,?,?)",
                    (
                        item,
                        item,
                        1794787200000,
                        json.dumps({"type": "text", "text": payload(entry)}),
                    ),
                )
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
        "--timeout", type=int, default=180, help="Per subprocess deadline in seconds"
    )
    parser.add_argument("--total-timeout", type=int, default=840)
    args = parser.parse_args()
    if (
        min(args.sessions, args.entries, args.repeat, args.timeout, args.total_timeout)
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
                "corpus": corpus,
                "free_bytes_before": free,
                "required_free_bytes": RESERVE + size * 12,
            }
        )
        if free < RESERVE + size * 12:
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
