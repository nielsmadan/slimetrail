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

    def test_incomplete_run_fails(self):
        with self.assertRaises(ValueError):
            validate(records()[:4])


if __name__ == "__main__":
    unittest.main()
