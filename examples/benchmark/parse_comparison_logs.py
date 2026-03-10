#!/usr/bin/env python3
"""
Parse solver debug logs to compare bellman_ford vs bellman_ford_v2 per trade.

Reads the log file and extracts per-pool solution results grouped by order_id.
Compares BF v1 vs BF v2 pairwise.

Usage:
    python3 parse_comparison_logs.py <solver.log> [output.json]
"""
import json
import re
import sys
from collections import defaultdict


def parse_log(log_path):
    """Extract per-pool solutions from debug log lines."""
    # Pattern: pool solution received order_id=X pool=Y amount_out_net_gas=Z status=W
    pattern = re.compile(
        r"pool solution received"
        r".*?order_id=(\S+)"
        r".*?pool=(\S+)"
        r".*?amount_out_net_gas=(\S+)"
        r".*?status=(\S+)"
    )

    # Pattern: selected best solution order_id=X pool=Y
    best_pattern = re.compile(
        r"selected best solution"
        r".*?order_id=(\S+)"
        r".*?pool=(\S+)"
    )

    orders = defaultdict(dict)  # order_id -> {pool_name: amount_out_net_gas}
    best_selections = {}  # order_id -> pool_name

    with open(log_path) as f:
        for line in f:
            m = pattern.search(line)
            if m:
                order_id, pool, amount, status = m.groups()
                if status == "Success":
                    orders[order_id][pool] = int(amount)

            m2 = best_pattern.search(line)
            if m2:
                order_id, pool = m2.groups()
                best_selections[order_id] = pool

    return orders, best_selections


def compare(orders, best_selections):
    """Compare BF v1 vs v2 across all orders."""
    v1_pool = "bellman_ford_pool"
    v2_pool = "bellman_ford_v2_pool"

    v2_wins = 0
    v1_wins = 0
    ties = 0
    only_v1 = 0
    only_v2 = 0
    both_missing = 0
    total = len(orders)

    v2_improvements = []
    v1_improvements = []

    # Track how often each pool was the overall winner
    best_counter = defaultdict(int)
    for pool in best_selections.values():
        best_counter[pool] += 1

    for order_id, pools in orders.items():
        has_v1 = v1_pool in pools
        has_v2 = v2_pool in pools

        if has_v1 and has_v2:
            v1_amt = pools[v1_pool]
            v2_amt = pools[v2_pool]

            if v2_amt > v1_amt:
                v2_wins += 1
                if v1_amt > 0:
                    pct = (v2_amt - v1_amt) / v1_amt * 100
                    v2_improvements.append(pct)
            elif v1_amt > v2_amt:
                v1_wins += 1
                if v2_amt > 0:
                    pct = (v1_amt - v2_amt) / v2_amt * 100
                    v1_improvements.append(pct)
            else:
                ties += 1
        elif has_v1 and not has_v2:
            only_v1 += 1
        elif has_v2 and not has_v1:
            only_v2 += 1
        else:
            both_missing += 1

    contested = v2_wins + v1_wins + ties

    print("=" * 60)
    print("  Bellman-Ford V1 vs V2 Comparison")
    print("=" * 60)
    print(f"\n  Total orders in log:    {total}")
    print(f"  Both pools responded:   {contested}")
    print(f"  Only V1 responded:      {only_v1}")
    print(f"  Only V2 responded:      {only_v2}")
    print(f"  Neither responded:      {both_missing}")

    if contested > 0:
        print(f"\n  --- Contested trades ({contested}) ---")
        print(f"  V2 wins:   {v2_wins:>6}  ({v2_wins/contested*100:.1f}%)")
        print(f"  V1 wins:   {v1_wins:>6}  ({v1_wins/contested*100:.1f}%)")
        print(f"  Ties:      {ties:>6}  ({ties/contested*100:.1f}%)")

    if v2_improvements:
        v2_improvements.sort()
        avg = sum(v2_improvements) / len(v2_improvements)
        med = v2_improvements[len(v2_improvements) // 2]
        print(f"\n  V2 improvement (when V2 wins):")
        print(f"    Mean:   {avg:.4f}%")
        print(f"    Median: {med:.4f}%")
        print(f"    Max:    {max(v2_improvements):.4f}%")
        print(f"    Min:    {min(v2_improvements):.6f}%")

    if v1_improvements:
        v1_improvements.sort()
        avg = sum(v1_improvements) / len(v1_improvements)
        med = v1_improvements[len(v1_improvements) // 2]
        print(f"\n  V1 improvement (when V1 wins):")
        print(f"    Mean:   {avg:.4f}%")
        print(f"    Median: {med:.4f}%")
        print(f"    Max:    {max(v1_improvements):.4f}%")

    if best_counter:
        print(f"\n  --- Overall best pool selection ---")
        for pool, count in sorted(
            best_counter.items(), key=lambda x: -x[1]
        ):
            pct = count / sum(best_counter.values()) * 100
            print(f"    {pool:<30} {count:>5}  ({pct:.1f}%)")

    print("\n" + "=" * 60)

    return {
        "total_orders": total,
        "contested": contested,
        "v2_wins": v2_wins,
        "v1_wins": v1_wins,
        "ties": ties,
        "only_v1": only_v1,
        "only_v2": only_v2,
        "v2_win_pct": round(v2_wins / contested * 100, 2) if contested else 0,
        "v1_win_pct": round(v1_wins / contested * 100, 2) if contested else 0,
        "v2_avg_improvement": (
            round(sum(v2_improvements) / len(v2_improvements), 4)
            if v2_improvements else 0
        ),
        "best_pool_counts": dict(best_counter),
    }


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: parse_comparison_logs.py <solver.log> [output.json]")
        sys.exit(1)

    log_path = sys.argv[1]
    output_path = sys.argv[2] if len(sys.argv) > 2 else None

    orders, best_selections = parse_log(log_path)
    results = compare(orders, best_selections)

    if output_path:
        with open(output_path, "w") as f:
            json.dump(results, f, indent=2)
        print(f"\nResults saved to {output_path}")
