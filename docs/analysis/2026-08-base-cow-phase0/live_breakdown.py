#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Per-order APEX-vs-baseline outcomes, broken out by order size and fill type.

The headline median and the USD-weighted figure disagree whenever the large orders fare
differently from the small ones, which is exactly what a single row hides. Same joins and same
signed-gap convention as live_join.py.

Usage: live_breakdown.py <dir-with-apex-and-comparisons-jsonl> [--bracket top|bottom]
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


def excluded_prefixes() -> list[str]:
    """`--exclude-prefix 0xadf` (repeatable) drops orders touching a token-factory cluster.

    Base carries deployers that mint thousands of vanity-prefixed tokens and trade them against
    one another; their volume is manufactured, and Phase 0 already excluded one such pair by
    hand. Screening by address prefix is crude but matches how these clusters present.
    """
    out: list[str] = []
    for i, arg in enumerate(sys.argv):
        if arg == "--exclude-prefix" and i + 1 < len(sys.argv):
            out.append(sys.argv[i + 1].lower())
    return out


def touches_excluded(comparison: dict, prefixes: list[str]) -> bool:
    if not prefixes:
        return False
    for token in (comparison.get("token_in"), comparison.get("token_out")):
        if token and any(token.lower().startswith(p) for p in prefixes):
            return True
    return False

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


def bps(apex_out: float, baseline_out: float) -> float | None:
    return None if baseline_out <= 0 else 10_000.0 * (apex_out - baseline_out) / baseline_out


def row(label: str, gaps: list[tuple[float, float]]) -> str:
    """gaps: (bps, usd notional filled)."""
    if not gaps:
        return f"{label:<12} {'—':>6}"
    values = sorted(g for g, _ in gaps)
    n = len(values)
    wins = sum(1 for g in values if g > 0)
    notional = sum(u for _, u in gaps)
    net_usd = sum(g / 10_000.0 * u for g, u in gaps)
    weighted = 10_000.0 * net_usd / notional if notional else 0.0
    return (
        f"{label:<12} {n:>6} {statistics.median(values):>+9.1f} "
        f"{values[int(0.25 * (n - 1))]:>+9.1f} {values[int(0.75 * (n - 1))]:>+9.1f} "
        f"{100.0 * wins / n:>7.0f}% {notional:>12,.0f} {net_usd:>+10.2f} {weighted:>+9.1f}"
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
    by_bucket: dict[tuple[str, str], list[tuple[float, float]]] = defaultdict(list)
    by_fill: dict[tuple[str, str], list[tuple[float, float]]] = defaultdict(list)
    seen: set[str] = set()

    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != bracket_wanted or wrong_window(record, wanted_window):
                continue
            for order in record.get("orders") or []:
                status = order.get("status")
                if status not in ("filled", "partially_filled") or order["id"] in seen:
                    continue
                seen.add(order["id"])
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                state = comparison.get("top" if bracket_wanted == "top" else "back") or {}
                apex_out = order.get("bought_raw")
                if apex_out is None:
                    continue
                ratio = min(1.0, order.get("fill_ratio") or 1.0)
                usd = (state.get("settled_value_usd") or 0.0) * ratio
                for name, baseline in (
                    ("fynd", state.get("fynd_amount_out")),
                    ("executed", comparison.get("settled_amount_out")),
                ):
                    if baseline is None:
                        continue
                    gap = bps(float(apex_out), float(baseline) * ratio)
                    if gap is None:
                        continue
                    for label, low, high in BUCKETS:
                        if low <= usd < high:
                            by_bucket[(name, label)].append((gap, usd))
                            break
                    by_fill[(name, status)].append((gap, usd))

    header = (
        f"{'bucket':<12} {'n':>6} {'median':>9} {'p25':>9} {'p75':>9} "
        f"{'better':>8} {'notional$':>12} {'net$':>10} {'wtd bps':>9}"
    )
    for baseline in ("fynd", "executed"):
        print(f"\n=== vs {baseline} — {bracket_wanted} bracket, by order size ===")
        print(header)
        for label, _, _ in BUCKETS:
            print(row(label, by_bucket[(baseline, label)]))
        print(f"--- by fill type ---")
        for status in ("filled", "partially_filled"):
            print(row(status[:12], by_fill[(baseline, status)]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
