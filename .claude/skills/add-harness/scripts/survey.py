#!/usr/bin/env python3
"""Summarize an agent session store's layout and record schema without printing transcript text."""

import argparse
import json
import os
import re
import shutil
import sqlite3
import sys
import tempfile
from collections import Counter, defaultdict
from pathlib import Path

DISCRIMINATORS = (
    "type",
    "kind",
    "event",
    "recordType",
    "record_type",
    "role",
    "source",
)
ENUM_RE = re.compile(r"^[A-Za-z_][\w.:/-]{0,39}$")
ENUM_MAX_DISTINCT = 12
FREE_TEXT_KEY_RE = re.compile(
    r"(^|\.)(message|content|text|prompt|output|result|reasoning\w*|summary|title|description|command|query|input|stdout|stderr|diff|patch|data)(\[\])?$",
    re.I,
)
SECRET_KEY_RE = re.compile(
    r"token|secret|password|passwd|api_?key|auth|credential|cookie", re.I
)
PLACEHOLDERS = [
    (
        re.compile(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", re.I
        ),
        "UUID",
    ),
    (re.compile(r"\d{4}-\d{2}-\d{2}T[\d:.-]+Z?"), "DATETIME"),
    (re.compile(r"\d{4}-\d{2}-\d{2}"), "DATE"),
    (re.compile(r"[0-9a-f]{16,}", re.I), "HEX"),
    (re.compile(r"\d{3,}"), "N"),
]


def is_sqlite(path):
    try:
        with path.open("rb") as handle:
            return handle.read(16) == b"SQLite format 3\x00"
    except OSError:
        return False


def files_under(paths, max_files):
    found = []
    for root in paths:
        if root.is_file():
            found.append(root)
            continue
        for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
            dirnames.sort()
            found.extend(Path(dirpath) / name for name in sorted(filenames))
    found.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return found[:max_files] if max_files else found


def path_pattern(path, root):
    try:
        rel = str(path.relative_to(root))
    except ValueError:
        rel = path.name
    for regex, name in PLACEHOLDERS:
        rel = regex.sub(name, rel)
    return rel


def type_name(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "num"
    if isinstance(value, str):
        return "str"
    if isinstance(value, list):
        return "list"
    return "obj"


def walk_value(value, prefix, depth, out):
    out.append((prefix or "$", value))
    if depth <= 0:
        return
    if isinstance(value, dict):
        for key, child in value.items():
            walk_value(child, f"{prefix}.{key}" if prefix else key, depth - 1, out)
    elif isinstance(value, list):
        for child in value:
            walk_value(child, f"{prefix}[]", depth - 1, out)


def discriminator(record):
    if not isinstance(record, dict):
        return type_name(record)
    parts = [
        f"{key}={record[key]}"
        for key in DISCRIMINATORS
        if isinstance(record.get(key), str) and ENUM_RE.match(record[key])
    ][:1]
    for nested in ("payload", "message", "data", "item"):
        child = record.get(nested)
        if isinstance(child, dict):
            parts += [
                f"{nested}.{key}={child[key]}"
                for key in DISCRIMINATORS
                if isinstance(child.get(key), str) and ENUM_RE.match(child[key])
            ][:1]
    return " ".join(parts) or "(no discriminator)"


class Schema:
    def __init__(self, depth):
        self.depth = depth
        self.groups = Counter()
        self.paths = defaultdict(lambda: defaultdict(Counter))
        self.values = defaultdict(lambda: defaultdict(Counter))

    def add(self, record):
        group = discriminator(record)
        self.groups[group] += 1
        pairs = []
        walk_value(record, "", self.depth, pairs)
        for path, value in pairs:
            self.paths[group][path][type_name(value)] += 1
            if (
                isinstance(value, str)
                and ENUM_RE.match(value)
                and not SECRET_KEY_RE.search(path)
                and not FREE_TEXT_KEY_RE.search(path)
            ):
                self.values[group][path][value] += 1

    def report(self, indent="  "):
        lines = []
        for group, count in self.groups.most_common():
            lines.append(f"{indent}[{count}] {group}")
            for path, types in sorted(self.paths[group].items()):
                seen = sum(types.values())
                kinds = "|".join(sorted(types))
                values = self.values[group].get(path)
                shown = ""
                if (
                    values
                    and len(values) <= ENUM_MAX_DISTINCT
                    and sum(values.values()) == types.get("str", 0)
                ):
                    shown = "  = " + ", ".join(
                        f"{v}({n})" for v, n in values.most_common()
                    )
                lines.append(f"{indent}    {path}: {kinds} x{seen}{shown}")
        return lines


def locate(value, marker, prefix=""):
    if isinstance(value, str):
        return [prefix or "$"] if marker in value else []
    if isinstance(value, dict):
        return [
            hit
            for key, child in value.items()
            for hit in locate(child, marker, f"{prefix}.{key}" if prefix else key)
        ]
    if isinstance(value, list):
        return [hit for child in value for hit in locate(child, marker, f"{prefix}[]")]
    return []


def redact(value, marker, key=""):
    if isinstance(value, str):
        if SECRET_KEY_RE.search(key):
            return f"(secret-like key, str len={len(value)})"
        if marker and marker in value:
            start = max(0, value.index(marker) - 40)
            return f"...{value[start : start + len(marker) + 80]}..."
        if ENUM_RE.match(value) and not FREE_TEXT_KEY_RE.search(key):
            return value
        return f"(str len={len(value)})"
    if isinstance(value, dict):
        return {name: redact(child, marker, name) for name, child in value.items()}
    if isinstance(value, list):
        return [redact(child, marker, key) for child in value]
    return value


def split_records(records, key):
    for number, record in records:
        items = record.get(key) if isinstance(record, dict) else None
        if not isinstance(items, list):
            yield str(number), record
            continue
        yield f"{number}#header", {k: v for k, v in record.items() if k != key}
        for index, item in enumerate(items):
            yield f"{number}#{index}", item


def json_records(path):
    if path.suffix == ".json":
        try:
            yield 1, json.loads(path.read_text(errors="replace"))
        except (json.JSONDecodeError, OSError):
            yield 1, None
        return
    with path.open(errors="replace") as handle:
        for number, line in enumerate(handle, 1):
            if line.strip():
                try:
                    yield number, json.loads(line)
                except json.JSONDecodeError:
                    yield number, None


def private_copy(path, workdir):
    target = Path(workdir) / path.name
    shutil.copyfile(path, target)
    wal = path.with_name(path.name + "-wal")
    if wal.exists():
        shutil.copyfile(wal, target.with_name(target.name + "-wal"))
    return target


def survey_sqlite(path, args, out):
    with tempfile.TemporaryDirectory(prefix="survey-") as workdir:
        os.chmod(workdir, 0o700)
        copy = private_copy(path, workdir)
        conn = sqlite3.connect(copy)
        try:
            tables = [
                row[0]
                for row in conn.execute(
                    "select name from sqlite_master where type='table' order by name"
                )
            ]
            out.append(f"  sqlite tables: {len(tables)}")
            for table in tables:
                columns = conn.execute(f'pragma table_info("{table}")').fetchall()
                count = conn.execute(f'select count(*) from "{table}"').fetchone()[0]
                out.append(f"  table {table} rows={count}")
                for column in columns:
                    out.append(
                        f"    {column[1]} {column[2]}{' PK' if column[5] else ''}"
                    )
                pk = [c[1] for c in sorted(columns, key=lambda c: c[5]) if c[5]]
                key_sql = ", ".join(f'"{c}"' for c in pk) if pk else "rowid"
                for column in columns:
                    name = column[1]
                    rows = conn.execute(
                        f'select {key_sql}, "{name}" from "{table}" where typeof("{name}") in (\'text\',\'blob\') limit {args.sample_rows}'
                    ).fetchall()
                    schema = Schema(args.depth)
                    for *key_values, cell in rows:
                        rowid = "/".join(
                            v.hex()[:16] if isinstance(v, bytes) else str(v)
                            for v in key_values
                        )
                        text = (
                            cell.decode(errors="replace")
                            if isinstance(cell, bytes)
                            else cell
                        )
                        try:
                            parsed = json.loads(text)
                        except (json.JSONDecodeError, TypeError):
                            parsed = None
                        if (
                            isinstance(parsed, dict)
                            and args.split
                            and isinstance(parsed.get(args.split), list)
                        ):
                            for _, item in split_records([(rowid, parsed)], args.split):
                                schema.add(item)
                        elif isinstance(parsed, (dict, list)):
                            schema.add(parsed)
                        if args.marker and text and args.marker in text:
                            out.append(f"    MARKER {table}.{name} key={rowid}")
                    if schema.groups:
                        out.append(f"    json in {name}:")
                        out.extend(schema.report("      "))
        finally:
            conn.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=Path)
    parser.add_argument(
        "--max-files", type=int, default=200, help="newest N files (0 = all)"
    )
    parser.add_argument("--depth", type=int, default=4, help="key-path depth")
    parser.add_argument(
        "--split",
        help="treat each element of this top-level array key as a record (one-document-per-session stores)",
    )
    parser.add_argument(
        "--marker", help="report files, lines and key paths containing this probe token"
    )
    parser.add_argument(
        "--record",
        help="FILE:LINE, print that record with free text redacted to lengths",
    )
    parser.add_argument(
        "--sample-rows", type=int, default=500, help="rows per sqlite column to inspect"
    )
    args = parser.parse_args()

    if args.record:
        file, line = args.record.rsplit(":", 1)
        records = json_records(Path(file))
        if args.split:
            records = split_records(records, args.split)
        for number, record in records:
            if str(number) == line:
                print(
                    json.dumps(
                        redact(record, args.marker), indent=2, ensure_ascii=False
                    )
                )
                return 0
        print(f"no record at {args.record}", file=sys.stderr)
        return 1

    if not args.paths:
        parser.error("give at least one path, or --record FILE:LINE")
    roots = [p.expanduser() for p in args.paths]
    files = files_under(roots, args.max_files)
    out = [
        f"files surveyed: {len(files)} (newest first, cap {args.max_files or 'none'})"
    ]
    patterns = Counter(
        path_pattern(f, next((r for r in roots if r in f.parents), f.parent))
        for f in files
    )
    out.append("layout patterns:")
    out.extend(f"  {n:5d}  {p}" for p, n in patterns.most_common(40))

    schema = Schema(args.depth)
    bad = Counter()
    for path in files:
        if is_sqlite(path):
            out.append(f"sqlite {path}")
            survey_sqlite(path, args, out)
            continue
        if path.suffix not in (".jsonl", ".json", ".ndjson"):
            continue
        records = json_records(path)
        if args.split:
            records = split_records(records, args.split)
        for number, record in records:
            if record is None:
                bad[str(path)] += 1
                continue
            schema.add(record)
            if args.marker:
                hits = locate(record, args.marker)
                if hits:
                    out.append(
                        f"MARKER {path}:{number}  {discriminator(record)}  at {', '.join(sorted(set(hits)))}"
                    )
    out.append(
        "json record schema (count, discriminator, key paths with types; enum-like values shown):"
    )
    out.extend(schema.report())
    if bad:
        out.append("unparseable lines:")
        out.extend(f"  {n}  {p}" for p, n in bad.most_common(10))
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
