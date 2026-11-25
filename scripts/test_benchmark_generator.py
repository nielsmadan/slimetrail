import contextlib
import random
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import benchmark


class ToolOutputTests(unittest.TestCase):
    def test_seeded_mix_and_determinism(self):
        def sizes(seed):
            rng = random.Random(seed)
            return [len(benchmark.tool_output(rng)) for _ in range(1000)]

        first = sizes("mix")
        self.assertEqual(first, sizes("mix"))
        small = sum(1 for n in first if n < 1024)
        medium = sum(1 for n in first if 1024 <= n <= 16 * 1024)
        large = sum(1 for n in first if n > 16 * 1024)
        self.assertEqual(small + medium + large, 1000)
        self.assertTrue(520 <= small <= 680, small)
        self.assertTrue(250 <= medium <= 350, medium)
        self.assertTrue(60 <= large <= 140, large)
        self.assertLessEqual(max(first), 256 * 1024)


class EventTests(unittest.TestCase):
    def test_events_are_deterministic_and_have_every_kind(self):
        first = list(benchmark.events("claude", 0, 20))
        self.assertEqual(first, list(benchmark.events("claude", 0, 20)))
        self.assertEqual(
            {e["kind"] for e in first}, {"user", "reasoning", "call", "result"}
        )
        calls = {e["id"] for e in first if e["kind"] == "call"}
        self.assertTrue(all(e["id"] in calls for e in first if e["kind"] == "result"))

    def test_each_jsonl_writer_emits_reasoning_call_and_result(self):
        by_kind = {}
        for event in benchmark.events("x", 0, 10):
            by_kind.setdefault(event["kind"], event)
        for agent in ("claude", "codex", "cursor", "droid", "pi"):
            with self.subTest(agent=agent):
                shapes = {
                    kind: benchmark.jsonl_record(agent, event)
                    for kind, event in by_kind.items()
                }
                self.assertEqual(len({repr(s) for s in shapes.values()}), 4)
                text = repr(shapes)
                if agent == "codex":
                    for marker in (
                        "reasoning",
                        "function_call",
                        "function_call_output",
                    ):
                        self.assertIn(marker, text)
                elif agent == "pi":
                    for marker in ("thinking", "toolCall", "toolResult"):
                        self.assertIn(marker, text)
                else:
                    for marker in ("thinking", "tool_use", "tool_result"):
                        self.assertIn(marker, text)


class DevinStoreTests(unittest.TestCase):
    def test_schema_tables_and_blocks(self):
        with tempfile.TemporaryDirectory() as directory:
            database = Path(directory) / "cli" / "sessions.db"
            benchmark.write_devin(database, 2, 10)
            with contextlib.closing(sqlite3.connect(database)) as connection:
                tables = {
                    row[0]
                    for row in connection.execute(
                        "SELECT name FROM sqlite_master WHERE type='table'"
                    )
                }
                self.assertTrue(
                    {"sessions", "message_nodes", "subagent_heads"} <= tables
                )
                self.assertEqual(
                    connection.execute("SELECT count(*) FROM sessions").fetchone()[0], 2
                )
                text = " ".join(
                    row[0]
                    for row in connection.execute(
                        "SELECT chat_message FROM message_nodes"
                    )
                )
            for marker in ("thinking", "tool_calls", "tool_call_id"):
                self.assertIn(marker, text)


class AggregateIndexTests(unittest.TestCase):
    def test_entry_bytes_count_split_tool_result_tails(self):
        with tempfile.TemporaryDirectory() as directory:
            index = Path(directory)
            with contextlib.closing(
                sqlite3.connect(index / "index-v3.sqlite3")
            ) as connection:
                connection.executescript(
                    "CREATE TABLE sessions(id TEXT);"
                    "CREATE TABLE entries(text_tail TEXT, text TEXT);"
                    "CREATE TABLE search_documents(has_non_ascii INTEGER, has_nul INTEGER);"
                    "INSERT INTO entries VALUES('tail', 'head'), ('', 'plain é');"
                )
            self.assertEqual(benchmark.aggregate_index(index)["entry_bytes"], 16)

    def test_index_bytes_include_pages_still_in_the_wal(self):
        with tempfile.TemporaryDirectory() as directory:
            index = Path(directory)
            path = index / "index-v3.sqlite3"
            with contextlib.closing(sqlite3.connect(path)) as writer:
                writer.executescript(
                    "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;"
                    "CREATE TABLE sessions(id TEXT);"
                    "CREATE TABLE entries(text_tail TEXT, text TEXT);"
                    "CREATE TABLE search_documents(has_non_ascii INTEGER, has_nul INTEGER);"
                )
                writer.execute("INSERT INTO entries VALUES('', ?)", ("x" * 300_000,))
                writer.commit()
                dimensions = benchmark.aggregate_index(index)
            self.assertGreater(dimensions["index_bytes"], 300_000)
            self.assertEqual(dimensions["index_bytes"], path.stat().st_size)


class PerAgentTests(unittest.TestCase):
    def run_agent(self, invoke):
        emitted = []
        with tempfile.TemporaryDirectory() as directory:
            fresh = Path(directory) / "index-pi"
            fresh.mkdir()
            with (
                mock.patch.object(benchmark, "invoke", invoke),
                mock.patch.object(benchmark, "emit", emitted.append),
                mock.patch.object(
                    benchmark, "agent_dimensions", return_value={"sessions": 2}
                ),
            ):
                try:
                    benchmark.measure_agent(
                        "slimetrail",
                        fresh,
                        {},
                        "pi",
                        {"files": 3, "bytes": 40},
                        "compact",
                        10,
                        0,
                    )
                finally:
                    removed = not fresh.exists()
        return emitted, removed

    def test_record_is_emitted_after_its_index_is_removed(self):
        result = {"seconds": 1.5, "exit": 0, "diagnostic_counts": {}}
        emitted, removed = self.run_agent(lambda *_: result)
        self.assertTrue(removed)
        self.assertEqual(
            emitted,
            [
                {
                    "per_agent": "pi",
                    "coverage": "compact",
                    "source_files": 3,
                    "source_bytes": 40,
                    "cold_ms": 1500,
                    "exit": 0,
                    "incomplete": None,
                    "diagnostic_counts": {},
                    "counters": {},
                    "sessions": 2,
                }
            ],
        )

    def test_fatal_exit_is_recorded_then_raised(self):
        result = {"seconds": 1, "exit": 2, "diagnostic_counts": {"fatal": 1}}
        with self.assertRaisesRegex(RuntimeError, "cli_fatal"):
            self.run_agent(lambda *_: result)

    def test_failure_is_recorded_then_reraised(self):
        def timeout(*_):
            raise TimeoutError

        emitted = []
        with mock.patch.object(benchmark, "emit", emitted.append):
            with self.assertRaises(TimeoutError):
                benchmark.attempt({"per_agent": "pi"}, timeout)
        self.assertEqual(
            [{k: v for k, v in e.items() if k != "elapsed_seconds"} for e in emitted],
            [{"per_agent": "pi", "failure": "TimeoutError"}],
        )


if __name__ == "__main__":
    unittest.main()
