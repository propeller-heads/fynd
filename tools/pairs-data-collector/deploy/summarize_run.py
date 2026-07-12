#!/usr/bin/env python3
"""Summarize collector WAL capacity and coverage without third-party packages."""

from __future__ import annotations

import argparse
import json
import statistics
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


def _percentile(values: list[float], percentile: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = round((len(ordered) - 1) * percentile)
    return ordered[index]


def _distribution(values: list[float]) -> dict[str, float | None]:
    if not values:
        return {"min": None, "p50": None, "p95": None, "p99": None, "max": None}
    return {
        "min": min(values),
        "p50": statistics.median(values),
        "p95": _percentile(values, 0.95),
        "p99": _percentile(values, 0.99),
        "max": max(values),
    }


def _read_records(
    wal_dir: Path,
) -> tuple[list[dict[str, Any]], Counter, dict[int, int]]:
    blocks = []
    statuses: Counter[str] = Counter()
    attempted_by_block: dict[int, int] = defaultdict(int)
    for wal in sorted(wal_dir.glob("*.ndjson")):
        with wal.open(encoding="utf-8") as handle:
            for line in handle:
                try:
                    envelope = json.loads(line)
                except json.JSONDecodeError:
                    continue
                record_type = envelope.get("record_type")
                record = envelope.get("record", {})
                if record_type == "block_run":
                    blocks.append(record)
                elif record_type == "quote_point":
                    status = record.get("status", "unknown")
                    statuses[status] += 1
                    if status not in {"reverse_skipped_parent_failed", "missed_state"}:
                        attempted_by_block[record["block_number"]] += 1
    return blocks, statuses, attempted_by_block


def summarize(output_dir: Path) -> dict[str, Any]:
    """Return capacity and coverage metrics for all WALs under an output directory."""
    blocks, point_statuses, attempted_by_block = _read_records(output_dir / "wal")
    if not blocks:
        raise ValueError(f"no block_run records found under {output_dir / 'wal'}")
    blocks.sort(key=lambda row: row["block_number"])
    ready = [row for row in blocks if row.get("fynd_ready_at_ms")]
    durations = [
        row["collection_finished_at_ms"] - row["collection_started_at_ms"]
        for row in ready
    ]
    head_lags = [
        row["collection_finished_at_ms"] - row["head_received_at_ms"] for row in ready
    ]
    observation_rates = [
        row["scheduled_rows"] * 1_000 / duration
        for row, duration in zip(ready, durations, strict=True)
        if duration > 0
    ]
    attempted_rates = [
        attempted_by_block[row["block_number"]] * 1_000 / duration
        for row, duration in zip(ready, durations, strict=True)
        if duration > 0
    ]
    scheduled = sum(row["scheduled_rows"] for row in blocks)
    successful = sum(row["successful_rows"] for row in blocks)
    return {
        "blocks": len(blocks),
        "ready_blocks": len(ready),
        "missed_state_blocks": len(blocks) - len(ready),
        "first_block": blocks[0]["block_number"],
        "last_block": blocks[-1]["block_number"],
        "consecutive": all(
            row["block_number"] == blocks[0]["block_number"] + index
            for index, row in enumerate(blocks)
        ),
        "scheduled_rows": scheduled,
        "successful_rows": successful,
        "success_rate": successful / scheduled,
        "point_statuses": dict(point_statuses),
        "collection_ms": _distribution(durations),
        "head_to_collection_finish_ms": _distribution(head_lags),
        "scheduled_observation_rows_per_second": _distribution(observation_rates),
        "attempted_solver_quotes_per_second": _distribution(attempted_rates),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output_dir", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    summary = summarize(args.output_dir)
    encoded = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
