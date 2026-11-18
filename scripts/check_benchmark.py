import json
import sys
from pathlib import Path


def validate(records):
    if not records or records[0].get("mode") != "synthetic":
        raise ValueError("scheduled benchmarks must use synthetic stores")
    measurements = {r["measurement"]: r for r in records if "measurement" in r}
    required = {
        "cold_index",
        "warm_index",
        "ascii_literal",
        "short_literal",
        "non_ascii_literal",
        "regex",
        "one_changed_source",
    }
    if not required <= measurements.keys():
        raise ValueError("benchmark measurements are missing")
    for record in measurements.values():
        if record.get("failure") or not record.get("runs"):
            raise ValueError("benchmark measurement failed")
        for run in record["runs"]:
            if (
                run["exit"] != 0
                or run.get("incomplete")
                or run.get("invalid_envelope")
                or run["diagnostic_counts"]
            ):
                raise ValueError(
                    "synthetic benchmark returned incomplete or invalid results"
                )
    cold = measurements["cold_index"]["runs"][0]["counters"]
    count = cold["discovered"]
    if count <= 0 or cold["reparsed"] != count:
        raise ValueError("cold index did not parse every discovered source")
    for run in measurements["warm_index"]["runs"]:
        counters = run["counters"]
        if counters["reused"] != count or counters["reparsed"] != 0:
            raise ValueError("warm index did not reuse every source")
    changed = measurements["one_changed_source"]["runs"][0]["counters"]
    if changed["reparsed"] != 1 or changed["reused"] != count - 1:
        raise ValueError("one changed source did not reparse exactly once")
    cleanup = records[-1]
    if (
        cleanup.get("cleanup") != "removed_owned_workspace"
        or cleanup.get("index_exists") is not False
        or cleanup.get("workspace_exists") is not False
    ):
        raise ValueError("benchmark workspace was not cleaned up")


if __name__ == "__main__":
    validate([json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()])
    print("Synthetic benchmark coverage, incremental refresh and cleanup verified.")
