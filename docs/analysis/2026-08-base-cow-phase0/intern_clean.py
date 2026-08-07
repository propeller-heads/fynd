#!/usr/bin/env -S uv run --script
# /// script
# requires-python = """>=3.11"""
# dependencies = []
# ///
"""Internalization restricted to batches untouched by an excluded token cluster.

Internalization is recorded per batch (pool flow and filled notional are solver-level sums), so
unlike the per-order statistics it cannot be filtered order by order after the fact. The closest
honest equivalent is to keep only batches in which no order touches the excluded tokens, and
aggregate those.

Usage: intern_clean.py <dir> --window N [--exclude-prefix 0xadf ...]
"""

from __future__ import annotations

import json
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

    def dirty(order_id: str) -> bool:
        comparison = comparisons.get(order_id)
        if comparison is None:
            return False
        for token in (comparison.get("token_in"), comparison.get("token_out")):
            if token and any(token.lower().startswith(p) for p in prefixes):
                return True
        return False

    kept_pool = kept_filled = 0.0
    all_pool = all_filled = 0.0
    kept = dropped = 0
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":
                continue
            if window is not None and (record.get("window_blocks") or 1) != window:
                continue
            pool = record.get("pool_cleared_wei") or 0.0
            filled = record.get("filled_notional_wei") or 0.0
            all_pool += pool
            all_filled += filled
            # A batch counts as clean only if none of its FILLED orders touch the cluster —
            # an excluded token that never filled cannot have moved the pool flow.
            touched = any(
                order.get("status") in ("filled", "partially_filled") and dirty(order["id"])
                for order in record.get("orders") or []
            )
            if touched:
                dropped += 1
                continue
            kept += 1
            kept_pool += pool
            kept_filled += filled

    def share(pool: float, filled: float) -> str:
        if filled <= 0:
            return "n/a (nothing filled)"
        return f"{max(0.0, min(1.0, 1.0 - pool / (2.0 * filled))):.3f}"

    print(f"window {window}:")
    print(f"  all batches      ({kept + dropped:>5}): internalization {share(all_pool, all_filled)}")
    print(f"  clean batches    ({kept:>5}): internalization {share(kept_pool, kept_filled)}")
    print(f"  dropped (cluster) {dropped:>5}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
