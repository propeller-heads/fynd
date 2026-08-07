#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Dump the filled orders of each batch, so their senders can be looked up and screened.

A batch that crossed two orders from the SAME sender has not found a coincidence of wants — one
party traded with itself. Runs written before the monitor recorded senders can still be screened
by resolving the transaction hashes externally.

Emits: one line per filled order as `batch_key<TAB>tx_hash<TAB>token_in<TAB>token_out<TAB>usd`.

Usage: selfmatch_extract.py <dir> --window N [--hashes-only]
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
    hashes_only = "--hashes-only" in sys.argv

    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    rows: list[tuple[str, str, str, str, float]] = []
    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":
                continue
            if window is not None and (record.get("window_blocks") or 1) != window:
                continue
            # One batch = one (window, closing block) pair.
            batch_key = f"w{record.get('window_blocks') or 1}b{record.get('block')}"
            for order in record.get("orders") or []:
                if order.get("status") not in ("filled", "partially_filled"):
                    continue
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                usd = (comparison.get("top") or {}).get("settled_value_usd") or 0.0
                rows.append(
                    (
                        batch_key,
                        comparison.get("settled_tx") or "",
                        comparison.get("token_in") or "",
                        comparison.get("token_out") or "",
                        float(usd),
                    )
                )

    if hashes_only:
        for tx in sorted({row[1] for row in rows if row[1]}):
            print(tx)
        return 0

    # Only batches with at least two fills can self-match.
    counts: dict[str, int] = {}
    for batch, *_ in rows:
        counts[batch] = counts.get(batch, 0) + 1
    multi = {batch for batch, count in counts.items() if count >= 2}
    for batch, tx, token_in, token_out, usd in rows:
        if batch in multi:
            print(f"{batch}\t{tx}\t{token_in}\t{token_out}\t{usd:.2f}")
    print(
        f"# batches with >=2 fills: {len(multi)}; orders in them: "
        f"{sum(1 for row in rows if row[0] in multi)}; distinct tx: "
        f"{len({row[1] for row in rows if row[0] in multi})}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
