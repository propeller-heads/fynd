"""Pre-check for the APEX live single-block stage (PLAN.md v2, build-order step 0.5).

Counts, per day, blocks whose decoded trades could form a batch APEX can act on:
  - blocks with >=2 decoded trades
  - blocks with >=2 trades sharing at least one token (connected: token_in or token_out overlap)
  - same, restricted to trades fynd could solve at top-of-block (top.verdict present, not unsolvable)
Decision rule (grill round 1, finding 20): if connected blocks/day is too low, the live
single-block stage ships instrumentation-only and value claims come from offline window sweeps.
"""

import json
import sys
from collections import defaultdict
from pathlib import Path

BASE_BLOCKS_PER_DAY = 43_200  # 2 s blocks

data_dir = Path(sys.argv[1])

grand = defaultdict(int)
print(
    f"{'day':<12} {'trades':>7} {'blocks':>6} {'>=2tr':>6} {'conn':>6} "
    f"{'conn_solv':>9} {'conn/day%':>9} {'max_blk':>7}"
)
for day_file in sorted(data_dir.glob("comparisons-*.jsonl")):
    day = day_file.stem.removeprefix("comparisons-")
    blocks: dict[int, list[dict]] = defaultdict(list)
    n_trades = 0
    with day_file.open() as fh:
        for line in fh:
            rec = json.loads(line)
            n_trades += 1
            blocks[rec["block"]].append(rec)

    multi = 0
    connected = 0
    connected_solvable = 0
    max_block_trades = 0
    for trades in blocks.values():
        max_block_trades = max(max_block_trades, len(trades))
        if len(trades) < 2:
            continue
        multi += 1

        def has_token_overlap(trade_set: list[dict]) -> bool:
            token_to_count: dict[str, int] = defaultdict(int)
            for trade in trade_set:
                for token in {trade["token_in"], trade["token_out"]}:
                    token_to_count[token] += 1
            return any(count >= 2 for count in token_to_count.values())

        if has_token_overlap(trades):
            connected += 1
        solvable = [
            t
            for t in trades
            if t.get("top") and t["top"].get("verdict") not in (None, "Unsolvable")
            and not t["top"].get("unsolvable_reason")
        ]
        if len(solvable) >= 2 and has_token_overlap(solvable):
            connected_solvable += 1

    print(
        f"{day:<12} {n_trades:>7} {len(blocks):>6} {multi:>6} {connected:>6} "
        f"{connected_solvable:>9} {100 * connected / BASE_BLOCKS_PER_DAY:>8.2f}% {max_block_trades:>7}"
    )
    grand["trades"] += n_trades
    grand["blocks"] += len(blocks)
    grand["multi"] += multi
    grand["connected"] += connected
    grand["connected_solvable"] += connected_solvable

days = len(list(data_dir.glob("comparisons-*.jsonl")))
print(
    f"\nTOTAL {days}d: trades={grand['trades']} blocks_with_trades={grand['blocks']} "
    f"multi={grand['multi']} connected={grand['connected']} "
    f"connected_solvable={grand['connected_solvable']}"
)
print(
    f"per day: multi={grand['multi'] / days:.0f} connected={grand['connected'] / days:.0f} "
    f"connected_solvable={grand['connected_solvable'] / days:.0f} "
    f"(= {100 * grand['connected'] / days / BASE_BLOCKS_PER_DAY:.2f}% of {BASE_BLOCKS_PER_DAY} blocks/day)"
)
print(
    f"eligible-block share (>=1 trade): "
    f"{100 * grand['blocks'] / days / BASE_BLOCKS_PER_DAY:.1f}%/day "
    f"(v2.3 assumed ~35%)"
)
