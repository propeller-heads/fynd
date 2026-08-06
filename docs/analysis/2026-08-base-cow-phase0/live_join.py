#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Join the live monitor's APEX batch results against its Fynd comparisons.

Both streams come from one `hindsight monitor --apex-dir … --comparisons-dir …` run, so an
order's batch clearing and its Fynd quote were computed at the SAME block state — the
comparison the offline sweeps could not make. The join key is the order id the monitor
stamps on both sides: `{settled_tx}:{tx_index}`.

Usage: live_join.py <dir-with-apex-and-comparisons-jsonl> [--out summary.json]
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path

FILLED = ("filled", "partially_filled")


def read_jsonl(path: Path):
    with path.open() as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue  # a run killed mid-write leaves at most one partial line


def load_comparisons(directory: Path) -> dict[str, dict]:
    """Per-order Fynd baseline, keyed by order id, from every comparisons file present."""
    by_id: dict[str, dict] = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is None or index is None:
                continue
            by_id[f"{tx}:{index}"] = record
    return by_id


def bps(apex_out: float, fynd_out: float) -> float | None:
    if fynd_out <= 0:
        return None
    return 10_000.0 * (apex_out - fynd_out) / fynd_out


def summarize(directory: Path) -> dict:
    comparisons = load_comparisons(directory)

    blocks_dispatched: set[int] = set()
    status_counts: Counter[str] = Counter()
    solve_ms: list[int] = []
    queue_ms: list[int] = []
    counters: Counter[str] = Counter()
    # Per bracket: the signed APEX-vs-Fynd gap of every order the batch filled, plus the
    # batch-vs-singles gap where the singles control also filled.
    gaps: dict[str, list[float]] = defaultdict(list)
    gaps_usd: dict[str, float] = defaultdict(float)
    notional_usd: dict[str, float] = defaultdict(float)
    vs_singles: dict[str, list[float]] = defaultdict(list)
    unmatched_ids = 0
    no_fynd_quote = 0
    filled_orders: dict[str, int] = defaultdict(int)

    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            bracket = record.get("bracket", "?")
            blocks_dispatched.add(record.get("block", -1))
            solve_ms.append(record.get("solve_wall_ms", 0))
            queue_ms.append(record.get("queue_wait_ms", 0))
            for key, value in (record.get("counters") or {}).items():
                if isinstance(value, (int, float)):
                    counters[key] += value
            singles = {
                single["id"]: single.get("bought_raw")
                for single in record.get("singles") or []
            }
            for order in record.get("orders") or []:
                status = order.get("status", "?")
                status_counts[f"{bracket}:{status}"] += 1
                if status not in FILLED:
                    continue
                filled_orders[bracket] += 1
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    unmatched_ids += 1
                    continue
                # Fynd's quote at the SAME state as this bracket: top = N-1, bottom = N.
                state = comparison.get("top" if bracket == "top" else "back") or {}
                fynd_out = state.get("fynd_amount_out")
                apex_out = order.get("bought_raw")
                if fynd_out is None or apex_out is None:
                    no_fynd_quote += 1
                    continue
                fill_ratio = min(1.0, order.get("fill_ratio") or 1.0)
                # Pro-rata the baseline: the batch may have filled only part of the order.
                gap = bps(float(apex_out), float(fynd_out) * fill_ratio)
                if gap is None:
                    no_fynd_quote += 1
                    continue
                gaps[bracket].append(gap)
                usd = state.get("settled_value_usd") or 0.0
                notional_usd[bracket] += usd * fill_ratio
                gaps_usd[bracket] += gap / 10_000.0 * usd * fill_ratio
                single_out = singles.get(order["id"])
                if single_out is not None:
                    single_gap = bps(float(apex_out), float(single_out) * fill_ratio)
                    if single_gap is not None:
                        vs_singles[bracket].append(single_gap)

    def stats(values: list[float]) -> dict:
        if not values:
            return {"n": 0}
        ordered = sorted(values)
        return {
            "n": len(ordered),
            "median_bps": round(statistics.median(ordered), 2),
            "mean_bps": round(statistics.fmean(ordered), 2),
            "p05_bps": round(ordered[int(0.05 * (len(ordered) - 1))], 2),
            "p95_bps": round(ordered[int(0.95 * (len(ordered) - 1))], 2),
            "apex_wins_share": round(
                sum(1 for value in ordered if value > 0) / len(ordered), 4
            ),
        }

    return {
        "source_dir": str(directory),
        "comparisons_loaded": len(comparisons),
        "blocks_dispatched": len(blocks_dispatched),
        "brackets_recorded": {
            bracket: count
            for bracket, count in sorted(
                Counter(
                    key.split(":", 1)[0] for key in status_counts.elements()
                ).items()
            )
        },
        "orders_filled": dict(filled_orders),
        "order_status_counts": dict(sorted(status_counts.items())),
        "solve_ms": {
            "n": len(solve_ms),
            "median": statistics.median(solve_ms) if solve_ms else 0,
            "max": max(solve_ms, default=0),
        },
        "queue_ms_max": max(queue_ms, default=0),
        "apex_counters": dict(sorted(counters.items())),
        "join_losses": {"ids_without_comparison": unmatched_ids, "no_fynd_quote": no_fynd_quote},
        "apex_vs_fynd_same_state": {
            bracket: stats(values) | {
                "notional_usd": round(notional_usd[bracket], 2),
                "surplus_usd": round(gaps_usd[bracket], 2),
            }
            for bracket, values in sorted(gaps.items())
        },
        "batch_vs_singles": {
            bracket: stats(values) for bracket, values in sorted(vs_singles.items())
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--out", type=Path, default=None)
    args = parser.parse_args()

    if not args.directory.is_dir():
        print(f"not a directory: {args.directory}", file=sys.stderr)
        return 1

    summary = summarize(args.directory)
    rendered = json.dumps(summary, indent=2)
    if args.out:
        args.out.write_text(rendered + "\n")
        print(f"wrote {args.out}")
    print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
