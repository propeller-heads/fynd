#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Are the unbatchable orders dust, or real trades on tokens we cannot reach?

Compares the USD size distribution of orders excluded at admission against those admitted, and
splits the admitted side by whether Fynd itself could route them.

Usage: unroutable_size.py <dir-with-apex-and-comparisons-jsonl>
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path


def read_jsonl(path: Path):
    with path.open() as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def describe(values: list[float]) -> str:
    if not values:
        return "n=0"
    ordered = sorted(values)
    total = sum(ordered)
    return (
        f"n={len(ordered):>6}  total=${total:>12,.0f}  "
        f"median=${statistics.median(ordered):>8,.2f}  mean=${total / len(ordered):>9,.2f}  "
        f"p90=${ordered[int(0.9 * (len(ordered) - 1))]:>9,.2f}"
    )


def main() -> int:
    directory = Path(sys.argv[1])
    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    by_status: dict[str, list[float]] = defaultdict(list)
    seen: set[str] = set()
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":
                continue
            for order in record.get("orders") or []:
                if order["id"] in seen:
                    continue
                seen.add(order["id"])
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                top = comparison.get("top") or {}
                usd = top.get("settled_value_usd")
                if usd is None:
                    continue
                status = order.get("status", "?")
                bucket = "EXCLUDED at admission" if status in (
                    "unknown_decimals",
                    "token_unpriced",
                    "zero_amount_or_limit",
                ) else "admitted to the batch"
                by_status[bucket].append(float(usd))
                # Cross-check against Fynd's own verdict for the same order.
                if bucket.startswith("EXCLUDED"):
                    verdict = top.get("verdict") or "?"
                    by_status[f"    excluded & fynd={verdict}"].append(float(usd))

    print(f"{'bucket':<34} distribution")
    for bucket in sorted(by_status, key=lambda b: -len(by_status[b])):
        print(f"{bucket:<34} {describe(by_status[bucket])}")

    # How many of the excluded orders would Fynd itself have failed on?
    fynd_verdicts = Counter()
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":
                continue
            for order in record.get("orders") or []:
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                if order.get("status") in ("unknown_decimals", "token_unpriced"):
                    top = comparison.get("top") or {}
                    reason = top.get("unsolvable_reason") or top.get("verdict") or "?"
                    fynd_verdicts[reason] += 1
    print("\nFynd's own verdict on the orders APEX excluded:")
    for reason, count in fynd_verdicts.most_common(8):
        print(f"    {reason:<28} {count:>7}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
