import copy
import unittest

from check_benchmark import validate


def records():
    def measurement(name, reparsed=0, reused=6):
        return {
            "measurement": name,
            "runs": [
                {
                    "exit": 0,
                    "diagnostic_counts": {},
                    "counters": {
                        "discovered": 6,
                        "reparsed": reparsed,
                        "reused": reused,
                    },
                }
            ],
        }

    return [
        {"mode": "synthetic"},
        measurement("cold_index", 6, 0),
        measurement("warm_index"),
        measurement("ascii_literal"),
        measurement("short_literal"),
        measurement("non_ascii_literal"),
        measurement("regex"),
        measurement("one_changed_source", 1, 5),
        {
            "cleanup": "removed_owned_workspace",
            "index_exists": False,
            "workspace_exists": False,
        },
    ]


def per_agent_records(coverage="compact"):
    base = records()
    base[0].update(agents=["claude", "devin"], coverage=coverage, per_agent=True)
    entries = [
        {
            "per_agent": agent,
            "coverage": coverage,
            "source_files": 3,
            "source_bytes": 1000,
            "sessions": 3,
            "entries": 30,
            "cold_ms": 12,
            "index_bytes": 4096,
            "exit": 0,
            "incomplete": False,
            "diagnostic_counts": {},
            "counters": {"discovered": 3, "reparsed": 3, "reused": 0, "failed": 0},
        }
        for agent in ("claude", "devin")
    ]
    return base[:-1] + entries + base[-1:]


class BenchmarkCheckTests(unittest.TestCase):
    def test_complete_incremental_run(self):
        validate(records())

    def test_partial_coverage_and_refresh_regressions_fail(self):
        base = records()
        mutations = [
            (0, lambda r: r.update(mode="real")),
            (2, lambda r: r["runs"][0].update(exit=1)),
            (2, lambda r: r["runs"][0]["counters"].update(reparsed=1, reused=5)),
            (7, lambda r: r["runs"][0]["counters"].update(reparsed=6, reused=0)),
            (8, lambda r: r.update(workspace_exists=True)),
        ]
        for index, mutate in mutations:
            with self.subTest(index=index):
                changed = copy.deepcopy(base)
                mutate(changed[index])
                with self.assertRaises(ValueError):
                    validate(changed)

    def test_informational_coverage_notes_are_not_failures(self):
        for code in ("search_coverage", "coverage_mismatch"):
            with self.subTest(code=code):
                changed = records()
                changed[3]["runs"][0]["diagnostic_counts"] = {code: 1}
                validate(changed)

    def test_per_agent_records(self):
        for coverage in ("compact", "full"):
            validate(per_agent_records(coverage))

    def test_per_agent_regressions_fail(self):
        mutations = [
            lambda r: r.update(exit=1),
            lambda r: r.update(incomplete=True),
            lambda r: r.update(index_bytes=0),
            lambda r: r.update(coverage="full"),
            lambda r: r.update(diagnostic_counts={"other": 1}),
            lambda r: r["counters"].update(reparsed=1),
            lambda r: r.pop("sessions"),
        ]
        for number, mutate in enumerate(mutations):
            with self.subTest(number=number):
                changed = per_agent_records()
                mutate(changed[-2])
                with self.assertRaises(ValueError):
                    validate(changed)

    def test_default_run_header_shape_passes(self):
        changed = records()
        changed[0].update(
            agents=["claude", "devin"], coverage="compact", per_agent=False
        )
        validate(changed)

    def test_per_agent_requested_without_records_fails(self):
        changed = records()
        changed[0].update(agents=["claude"], coverage="full", per_agent=True)
        with self.assertRaises(ValueError):
            validate(changed)

    def test_per_agent_more_regressions_fail(self):
        mutations = [
            lambda r: r.update(invalid_envelope=True),
            lambda r: r["counters"].update(failed=1),
        ]
        for number, mutate in enumerate(mutations):
            with self.subTest(number=number):
                changed = per_agent_records()
                mutate(changed[-2])
                with self.assertRaises(ValueError):
                    validate(changed)

    def test_failure_duplicate_and_extra_records_fail(self):
        failure = per_agent_records()
        failure[-2] = {"per_agent": "devin", "failure": "RuntimeError"}
        duplicate = per_agent_records()
        duplicate.insert(-1, copy.deepcopy(duplicate[-2]))
        extra = per_agent_records()
        extra.insert(-1, {**copy.deepcopy(extra[-2]), "per_agent": "pi"})
        for name, changed in (
            ("failure", failure),
            ("duplicate", duplicate),
            ("extra", extra),
        ):
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    validate(changed)

    def test_missing_per_agent_record_fails(self):
        changed = per_agent_records()
        del changed[-2]
        with self.assertRaises(ValueError):
            validate(changed)

    def test_incomplete_run_fails(self):
        with self.assertRaises(ValueError):
            validate(records()[:4])


if __name__ == "__main__":
    unittest.main()
