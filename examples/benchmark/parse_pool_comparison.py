#!/usr/bin/env python3
"""
Parse solver debug logs to compare per-pool solution quality.

Extracts "pool solution received" and "selected best solution" log lines,
then compares two pools head-to-head on contested trades (where both returned
a successful route).

Usage:
    python3 parse_pool_comparison.py <solver_log> <pool_a> <pool_b>

Example:
    python3 parse_pool_comparison.py solver.log bellman_ford_pool bellman_ford_v2_pool
"""
import re
import sys
from collections import Counter, defaultdict


def strip_ansi(s):
    return re.sub(r'\x1b\[[0-9;]*m', '', s)


def parse_log(log_path):
    """Extract per-pool solutions and best-pool selections from debug logs."""
    order_solutions = defaultdict(dict)  # order_id -> {pool: amount}
    order_best = {}  # order_id -> pool

    with open(log_path) as f:
        for raw_line in f:
            line = strip_ansi(raw_line)

            if "pool solution received" in line:
                m_oid = re.search(r'order_id=(\S+)', line)
                m_pool = re.search(r'pool=(\S+)', line)
                m_amt = re.search(r'amount_out_net_gas=(\S+)', line)
                m_st = re.search(r'status=(\S+)', line)
                if m_oid and m_pool and m_amt and m_st:
                    if m_st.group(1) == "Success":
                        try:
                            order_solutions[m_oid.group(1)][m_pool.group(1)] = \
                                int(m_amt.group(1))
                        except ValueError:
                            pass

            elif "selected best solution" in line:
                m_oid = re.search(r'order_id=(\S+)', line)
                m_pool = re.search(r'pool=(\S+)', line)
                if m_oid and m_pool:
                    order_best[m_oid.group(1)] = m_pool.group(1)

    return order_solutions, order_best


def compare(order_solutions, order_best, pool_a, pool_b):
    """Compare two pools head-to-head across all orders."""
    a_wins = 0
    b_wins = 0
    ties = 0
    contested = 0
    only_a = 0
    only_b = 0
    neither = 0
    winner_counter = Counter()
    a_improvements = []
    b_improvements = []

    for order_id, pools in order_solutions.items():
        has_a = pool_a in pools
        has_b = pool_b in pools

        best = order_best.get(order_id, "")
        if best:
            winner_counter[best] += 1

        if has_a and has_b:
            contested += 1
            amt_a = pools[pool_a]
            amt_b = pools[pool_b]
            if amt_a > amt_b:
                a_wins += 1
                if amt_b > 0:
                    a_improvements.append((amt_a - amt_b) / amt_b * 100)
            elif amt_b > amt_a:
                b_wins += 1
                if amt_a > 0:
                    b_improvements.append((amt_b - amt_a) / amt_a * 100)
            else:
                ties += 1
        elif has_a:
            only_a += 1
        elif has_b:
            only_b += 1
        else:
            neither += 1

    total = len(order_solutions)

    print("=" * 60)
    print(f"  {pool_a} vs {pool_b}")
    print("=" * 60)
    print(f"\n  Total orders with solutions: {total}")
    print(f"  Both succeeded (contested):  {contested}")
    print(f"  Only {pool_a}: {only_a}")
    print(f"  Only {pool_b}: {only_b}")
    print(f"  Neither: {neither}")

    if contested > 0:
        print(f"\n--- Head-to-head ({contested} trades) ---")
        print(f"  {pool_a} wins:  {a_wins:>6}  "
              f"({a_wins / contested * 100:.1f}%)")
        print(f"  {pool_b} wins:  {b_wins:>6}  "
              f"({b_wins / contested * 100:.1f}%)")
        print(f"  Ties: {' ' * max(0, len(pool_a) - 4)}{ties:>6}  "
              f"({ties / contested * 100:.1f}%)")

    for label, data in [(pool_a, a_improvements), (pool_b, b_improvements)]:
        if data:
            data.sort()
            print(f"\n--- Improvement when {label} wins ({len(data)} trades) ---")
            print(f"  Mean:   {sum(data) / len(data):.4f}%")
            print(f"  Median: {data[len(data) // 2]:.4f}%")
            print(f"  Max:    {max(data):.4f}%")

    if winner_counter:
        print(f"\n--- Overall best pool selection ---")
        for pool, count in winner_counter.most_common():
            pct = count / total * 100 if total > 0 else 0
            print(f"  {pool:<35} {count:>6}  ({pct:.1f}%)")

    print("\n" + "=" * 60)


if __name__ == "__main__":
    if len(sys.argv) != 4:
        print("Usage: parse_pool_comparison.py <solver_log> <pool_a> <pool_b>")
        print("Example: parse_pool_comparison.py solver.log "
              "bellman_ford_pool bellman_ford_v2_pool")
        sys.exit(1)

    log_path, pool_a, pool_b = sys.argv[1], sys.argv[2], sys.argv[3]
    order_solutions, order_best = parse_log(log_path)
    compare(order_solutions, order_best, pool_a, pool_b)
