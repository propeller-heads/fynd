#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""What running BOTH engines and serving the better quote is worth.

In deployment APEX never has to lose: if its clearing is worse than the Fynd route, the Fynd
route is served. So the realized gain per order is

    gain = max(0, apex_out - fynd_out)

and the average over all orders decomposes into how often APEX wins times how much it wins by.
Losses are reported separately — not because they cost anything under best-of, but because they
say how often the batch is simply redundant.

Usage: best_of.py <dir-with-apex-and-comparisons-jsonl> [--bracket top|bottom]
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

BUCKETS = [
    ("< $10", 0.0, 10.0),
    ("$10–100", 10.0, 100.0),
    ("$100–1k", 100.0, 1_000.0),
    ("$1k–10k", 1_000.0, 10_000.0),
    ("> $10k", 10_000.0, float("inf")),
]



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


def report(label: str, rows: list[tuple[float, float]]) -> str:
    """rows: (signed bps vs fynd, usd notional filled)."""
    if not rows:
        return f"{label:<12} {'—':>6}"
    n = len(rows)
    wins = [(g, u) for g, u in rows if g > 0]
    notional = sum(u for _, u in rows)
    # Realized gain under best-of: positive gaps only.
    gain_usd = sum(g / 10_000.0 * u for g, u in wins)
    win_bps = sorted(g for g, _ in wins)
    avg_gain_per_order = (len(wins) / n) * statistics.fmean(win_bps) if win_bps else 0.0
    return (
        f"{label:<12} {n:>6} {100.0 * len(wins) / n:>7.0f}% "
        f"{(statistics.median(win_bps) if win_bps else 0.0):>+9.1f} "
        f"{(statistics.fmean(win_bps) if win_bps else 0.0):>+9.1f} "
        f"{avg_gain_per_order:>+10.2f} {gain_usd:>10.2f} {notional:>12,.0f} "
        f"{(10_000.0 * gain_usd / notional if notional else 0.0):>+9.2f}"
    )


def main() -> int:
    directory = Path(sys.argv[1])
    bracket_wanted = "top"
    if "--bracket" in sys.argv:
        bracket_wanted = sys.argv[sys.argv.index("--bracket") + 1]

    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    wanted_window = window_filter()
    rows_all: list[tuple[float, float]] = []
    by_bucket: dict[str, list[tuple[float, float]]] = defaultdict(list)
    seen: set[str] = set()

    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != bracket_wanted or wrong_window(record, wanted_window):
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
                state = comparison.get("top" if bracket_wanted == "top" else "back") or {}
                apex_out, fynd_out = order.get("bought_raw"), state.get("fynd_amount_out")
                if apex_out is None or fynd_out is None:
                    continue
                ratio = min(1.0, order.get("fill_ratio") or 1.0)
                baseline = float(fynd_out) * ratio
                if baseline <= 0:
                    continue
                gap = 10_000.0 * (float(apex_out) - baseline) / baseline
                usd = (state.get("settled_value_usd") or 0.0) * ratio
                rows_all.append((gap, usd))
                for label, low, high in BUCKETS:
                    if low <= usd < high:
                        by_bucket[label].append((gap, usd))
                        break

    header = (
        f"{'slice':<12} {'n':>6} {'APEX won':>8} {'med win':>9} {'mean win':>9} "
        f"{'avg/order':>10} {'gain $':>10} {'notional$':>12} {'gain bps':>9}"
    )
    print(f"=== best-of(APEX, Fynd) — {bracket_wanted} bracket ===")
    print("(avg/order = win-rate x mean gain when winning, in bps; gain bps = gain$ / notional)")
    print(header)
    print(report("ALL", rows_all))
    for label, _, _ in BUCKETS:
        print(report(label, by_bucket[label]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
