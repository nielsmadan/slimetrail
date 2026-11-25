import contextlib
import random
import sqlite3
import tempfile
import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
