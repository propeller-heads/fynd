#!/usr/bin/env python3
"""Aggregate repeated Fynd scale benchmark JSON files."""

from __future__ import annotations

import argparse
import json
import re
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any

FILENAME = re.compile(
    r"cpu(?P<cpus>\d+)-workers(?P<workers>\d+)-concurrency(?P<concurrency>\d+)-rep\d+\.json"
)


def _median(rows: list[dict[str, Any]], path: tuple[str, ...]) -> float:
    values = []
    for row in rows:
        value: Any = row
        for key in path:
            value = value[key]
        if not isinstance(value, (int, float)):
            raise TypeError(f"metric {'.'.join(path)} is not numeric")
        values.append(float(value))
    return statistics.median(values)


def summarize(directory: Path) -> list[dict[str, Any]]:
    """Group benchmark repetitions and return median metrics for each case."""
    groups: dict[tuple[int, int, int], list[dict[str, Any]]] = defaultdict(list)
    for path in sorted(directory.glob("*.json")):
        match = FILENAME.fullmatch(path.name)
        if not match:
            continue
        payload = json.loads(path.read_text(encoding="utf-8"))
        point = payload["points"][0]
        key = (
            int(match["cpus"]),
            int(match["workers"]),
            int(match["concurrency"]),
        )
        groups[key].append(point)

    result = []
    for (cpus, workers, concurrency), rows in sorted(groups.items()):
        result.append(
            {
                "cpus": cpus,
                "workers": workers,
                "concurrency": concurrency,
                "repetitions": len(rows),
                "request_rps_median": _median(rows, ("throughput_rps",)),
                "solved_rps_median": _median(rows, ("solved_orders_rps",)),
                "route_coverage_median": _median(rows, ("route_coverage",)),
                "round_trip_ms": {
                    name: _median(rows, ("round_trip", name))
                    for name in ("median", "p95", "p99")
                },
                "solve_time_ms": {
                    name: _median(rows, ("solve_time", name))
                    for name in ("median", "p95", "p99")
                },
                "failed_requests_median": _median(rows, ("failed_requests",)),
            }
        )
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    encoded = json.dumps(summarize(args.directory), indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
