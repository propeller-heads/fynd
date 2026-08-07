#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""How much more data each headline number needs before it is trustworthy.

Two very different answers hide behind one sample. A win RATE is a proportion — its precision
follows sqrt(n) and is already decent. A dollar-weighted GAIN is a ratio dominated by a few large
contributions; its precision follows the *effective* sample size

    n_eff = (Σ x)² / Σ x²        (Kish)

which counts a total made of two big trades as roughly two observations, not two hundred.

Usage: power.py <dir> --window N [--exclude-prefix 0xadf]
"""

from __future__ import annotations

import json
import math
import sys
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


def main() -> int:
    directory = Path(sys.argv[1])
    window = int(sys.argv[sys.argv.index("--window") + 1]) if "--window" in sys.argv else None
    prefixes = [
        sys.argv[i + 1].lower()
        for i, arg in enumerate(sys.argv)
        if arg == "--exclude-prefix" and i + 1 < len(sys.argv)
    ]

    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    gaps: list[float] = []
    contributions: list[float] = []
    seen: set[str] = set()
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":
                continue
            if window is not None and (record.get("window_blocks") or 1) != window:
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
                if any(
                    token and any(token.lower().startswith(p) for p in prefixes)
                    for token in (comparison.get("token_in"), comparison.get("token_out"))
                ):
                    continue
                top = comparison.get("top") or {}
                apex, base_raw = order.get("bought_raw"), top.get("fynd_amount_out")
                if apex is None or base_raw is None:
                    continue
                ratio = min(1.0, order.get("fill_ratio") or 1.0)
                base = float(base_raw) * ratio
                if base <= 0:
                    continue
                gap = 10_000.0 * (float(apex) - base) / base
                gaps.append(gap)
                usd = (top.get("settled_value_usd") or 0.0) * ratio
                if gap > 0:
                    contributions.append(gap / 10_000.0 * usd)

    n = len(gaps)
    wins = sum(1 for g in gaps if g > 0)
    if n == 0:
        print("no data")
        return 1
    rate = wins / n
    half = 1.96 * math.sqrt(rate * (1 - rate) / n)
    print(f"window {window}   compared n = {n}")
    print(f"  win rate            : {100 * rate:.1f}%  ±{100 * half:.1f}pp (95% CI)")
    print(f"    -> already usable" if half < 0.05 else "    -> still wide")

    total = sum(contributions)
    sq = sum(c * c for c in contributions)
    n_eff = (total * total / sq) if sq > 0 else 0.0
    print(f"  gain $              : {total:.2f} from {len(contributions)} winning orders")
    print(f"  effective sample    : {n_eff:.1f}  (Kish; concentration-adjusted)")
    for target in (10, 30):
        if n_eff > 0:
            factor = target / n_eff
            print(
                f"    for n_eff={target:>2}: need ~{factor:.0f}x more data "
                f"(~{factor:.0f} more nights at this rate)"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
