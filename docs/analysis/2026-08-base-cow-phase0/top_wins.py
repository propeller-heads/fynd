#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Which orders drive the 'gain when APEX wins' figure.

A per-dollar gain conditioned on winning is easy to inflate: one large order with an implausible
gap, or a cluster of micro-orders where a few wei is thousands of basis points, moves it a lot.
This lists the biggest dollar contributors so the figure can be judged rather than trusted.

Usage: top_wins.py <dir> --window N [--baseline fynd|executed]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path



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


def main() -> int:
    directory = Path(sys.argv[1])
    window = int(sys.argv[sys.argv.index("--window") + 1]) if "--window" in sys.argv else None
    baseline_name = (
        sys.argv[sys.argv.index("--baseline") + 1] if "--baseline" in sys.argv else "fynd"
    )

    comparisons = {}
    for path in sorted(directory.glob("comparisons-*.jsonl")):
        for record in read_jsonl(path):
            tx, index = record.get("settled_tx"), record.get("tx_index")
            if tx is not None and index is not None:
                comparisons[f"{tx}:{index}"] = record

    skip_prefixes = excluded_prefixes()
    wins: list[tuple[float, float, float, str, str, str]] = []
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
                if comparison is None or touches_excluded(comparison, skip_prefixes):
                    continue
                top = comparison.get("top") or {}
                apex = order.get("bought_raw")
                base_raw = (
                    top.get("fynd_amount_out")
                    if baseline_name == "fynd"
                    else comparison.get("settled_amount_out")
                )
                if apex is None or base_raw is None:
                    continue
                ratio = min(1.0, order.get("fill_ratio") or 1.0)
                base = float(base_raw) * ratio
                if base <= 0:
                    continue
                gap = 10_000.0 * (float(apex) - base) / base
                if gap <= 0:
                    continue
                usd = (top.get("settled_value_usd") or 0.0) * ratio
                wins.append(
                    (
                        gap / 10_000.0 * usd,  # dollar contribution
                        gap,
                        usd,
                        comparison.get("token_in", "")[:10],
                        comparison.get("token_out", "")[:10],
                        f"{ratio:.2f}",
                    )
                )

    wins.sort(reverse=True)
    total = sum(w[0] for w in wins)
    print(f"winning orders: {len(wins)}   total gain ${total:.2f}")
    print(f"{'gain $':>9} {'share':>6} {'bps':>10} {'order $':>10} {'fill':>5}  pair")
    running = 0.0
    for gain, gap, usd, token_in, token_out, ratio in wins[:15]:
        running += gain
        print(
            f"{gain:>9.2f} {100 * gain / total:>5.1f}% {gap:>10.1f} {usd:>10.2f} {ratio:>5}  "
            f"{token_in}…/{token_out}…"
        )
    top10 = sum(w[0] for w in wins[:10])
    print(f"\ntop 10 orders = {100 * top10 / total:.0f}% of all the gain")
    tiny = [w for w in wins if w[2] < 10]
    print(
        f"wins on orders under $10: {len(tiny)} of {len(wins)} "
        f"({100 * sum(w[0] for w in tiny) / total:.1f}% of gain)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
