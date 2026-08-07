#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Batches where two fills could genuinely have crossed: same token pair, opposite directions.

Only these can be a coincidence of wants at all — everything else that filled was routed. Of
those, the ones to distrust are where both sides came from the SAME sender: one party trading
with itself, which inflates any CoW measure.

Prints the candidate pairs, and a SQL `IN` list of their transaction hashes so senders can be
resolved for runs recorded before the monitor captured them.

Usage: cow_candidates.py <dir> --window N [--sql]
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
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
    as_sql = "--sql" in sys.argv

    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    # batch -> list of (tx, token_in, token_out, usd, sender_or_none)
    batches: dict[str, list[tuple[str, str, str, float, str | None]]] = defaultdict(list)
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":
                continue
            if window is not None and (record.get("window_blocks") or 1) != window:
                continue
            key = f"w{record.get('window_blocks') or 1}b{record.get('block')}"
            for order in record.get("orders") or []:
                if order.get("status") not in ("filled", "partially_filled"):
                    continue
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                batches[key].append(
                    (
                        comparison.get("settled_tx") or "",
                        (comparison.get("token_in") or "").lower(),
                        (comparison.get("token_out") or "").lower(),
                        float((comparison.get("top") or {}).get("settled_value_usd") or 0.0),
                        comparison.get("sender"),
                    )
                )

    candidates: list[tuple[str, tuple, tuple]] = []
    for key, fills in batches.items():
        for i, a in enumerate(fills):
            for b in fills[i + 1 :]:
                # Opposite directions on the same pair.
                if a[1] == b[2] and a[2] == b[1]:
                    candidates.append((key, a, b))

    if as_sql:
        hashes = sorted({c[1][0] for c in candidates} | {c[2][0] for c in candidates})
        print(",\n".join(f"    {h}" for h in hashes if h))
        print(f"-- {len(hashes)} hashes", file=sys.stderr)
        return 0

    same_sender = 0
    known_sender = 0
    print(f"{'batch':<16} {'pair usd':>10}  senders")
    for key, a, b in candidates:
        senders = (a[4], b[4])
        mark = ""
        if all(senders):
            known_sender += 1
            if senders[0].lower() == senders[1].lower():
                same_sender += 1
                mark = "  <-- SAME SENDER (self-match)"
        print(f"{key:<16} {a[3] + b[3]:>10.2f}  {senders[0]} / {senders[1]}{mark}")
    print(
        f"\ncrossing candidates (same pair, opposite direction): {len(candidates)}"
        f"\n  with senders recorded: {known_sender}"
        f"\n  self-matched (same sender both sides): {same_sender}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
