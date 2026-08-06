#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Which tokens make an order unbatchable, and how much volume they carry.

`unknown_decimals` is the admission counter for "no token metadata", which in practice means the
solver's market state has no `Token` record for one side of the trade — the token is not in the
indexed universe at all, rather than having odd decimals. This ranks the offenders by orders and
by USD, and separates them from tokens that merely lack a price.

Usage: unbatchable_tokens.py <dir-with-apex-and-comparisons-jsonl>
"""

from __future__ import annotations

import json
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


def main() -> int:
    directory = Path(sys.argv[1])
    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    # Count each token by the excluding status it appeared under, and by the USD it carried.
    blamed: dict[str, Counter[str]] = defaultdict(Counter)
    usd: dict[str, dict[str, float]] = defaultdict(lambda: defaultdict(float))
    seen_ids: set[tuple[str, str]] = set()
    ok_tokens: Counter[str] = Counter()

    for path in sorted(directory.glob("apex-*.jsonl")):
        for record in read_jsonl(path):
            if record.get("bracket") != "top":  # one bracket is enough; both see the same orders
                continue
            for order in record.get("orders") or []:
                status = order.get("status", "?")
                key = (order["id"], status)
                if key in seen_ids:
                    continue
                seen_ids.add(key)
                comparison = comparisons.get(order["id"])
                if comparison is None:
                    continue
                value = (comparison.get("top") or {}).get("settled_value_usd") or 0.0
                tokens = [comparison.get("token_in"), comparison.get("token_out")]
                if status in ("unknown_decimals", "token_unpriced"):
                    for token in tokens:
                        if token:
                            blamed[status][token] += 1
                            usd[status][token] += value
                else:
                    for token in tokens:
                        if token:
                            ok_tokens[token] += 1

    for status in ("unknown_decimals", "token_unpriced"):
        counts = blamed[status]
        if not counts:
            continue
        total_orders = sum(counts.values())
        print(f"\n=== {status}: {len(counts)} distinct tokens over {total_orders} token-slots ===")
        # A token that also appears in admitted orders is not itself the blocker — its
        # counterparty token was. Split them so the true offenders are visible.
        never_ok = [(t, c) for t, c in counts.items() if t not in ok_tokens]
        print(f"    tokens NEVER seen in an admitted order: {len(never_ok)}")
        print(f"    {'token':<44} {'orders':>7} {'usd':>12}")
        for token, count in counts.most_common(15):
            mark = "" if token in ok_tokens else "  <- never admitted"
            print(f"    {token:<44} {count:>7} {usd[status][token]:>12.2f}{mark}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
