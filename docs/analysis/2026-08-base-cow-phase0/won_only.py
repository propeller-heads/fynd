#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""USD-weighted gain restricted to the orders APEX actually won.

The all-orders weighted average dilutes the wins across volume that never had a chance; this
answers the narrower question "when APEX does win, how much is that worth per dollar of the
winning flow".

Usage: won_only.py <dir-with-apex-and-comparisons-jsonl>
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path



def window_filter() -> int | None:
    """`--window N` restricts to batches of that window length. Records written before
    multi-window support carry no tag and count as the 1-block case."""
    if "--window" in sys.argv:
        return int(sys.argv[sys.argv.index("--window") + 1])
    return None


def wrong_window(record: dict, wanted: int | None) -> bool:
    return wanted is not None and (record.get("window_blocks") or 1) != wanted

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


def main() -> int:
    directory = Path(sys.argv[1])
    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    wanted_window = window_filter()
    rows: dict[str, list[tuple[float, float]]] = defaultdict(list)
    seen: set[str] = set()
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top" or wrong_window(record, wanted_window):
                continue
            for order in record.get("orders") or []:
                if order.get("status") not in ("filled", "partially_filled"):
                    continue
                if order["id"] in seen:
                    continue
                seen.add(order["id"])
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                top = comparison.get("top") or {}
                apex = order.get("bought_raw")
                if apex is None:
                    continue
                ratio = min(1.0, order.get("fill_ratio") or 1.0)
                usd = (top.get("settled_value_usd") or 0.0) * ratio
                for name, baseline in (
                    ("fynd", top.get("fynd_amount_out")),
                    ("executed", comparison.get("settled_amount_out")),
                ):
                    if baseline is None:
                        continue
                    base = float(baseline) * ratio
                    if base <= 0:
                        continue
                    rows[name].append((10_000.0 * (float(apex) - base) / base, usd))

    print(f"{'baseline':>9} {'wins':>5} {'win notional':>14} {'gain $':>9} {'per $ when won':>15}")
    for name, values in rows.items():
        wins = [(gap, usd) for gap, usd in values if gap > 0]
        notional = sum(usd for _, usd in wins)
        gain = sum(gap / 10_000.0 * usd for gap, usd in wins)
        weighted = 10_000.0 * gain / notional if notional else 0.0
        median = statistics.median([g for g, _ in wins]) if wins else 0.0
        print(
            f"{name:>9} {len(wins):>5} {notional:>14,.0f} {gain:>9.2f} {weighted:>+14.1f}  "
            f"(median win {median:+.1f} bps)"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
