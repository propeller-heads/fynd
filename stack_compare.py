#!/usr/bin/env python3
"""Compare two production-style worker-pool stacks from a quality-benchmark output JSON.

A worker pool returns the best net over its member algorithms per order (what
WorkerPoolRouter does, ranking by amount_out_net_gas). This computes:

    Stack A = max(stack_algo_a, shared...)   per trade
    Stack B = max(stack_algo_b, shared...)   per trade

and compares Stack A vs Stack B on the set where both stacks solve.

Usage:
    stack_compare.py RESULT.json "split_max,bellman_ford" "split,bellman_ford"
"""
import json
import sys


def stack_net(trade, idx_by_name, members):
    """Best net over members for this trade, or None if none solved it."""
    best = None
    for m in members:
        v = trade["nets"][idx_by_name[m]]
        if v is None:
            continue
        iv = int(v)
        if best is None or iv > best:
            best = iv
    return best


def main(path, stack_a_spec, stack_b_spec):
    with open(path) as f:
        data = json.load(f)
    algos = data["algorithms"]
    idx = {a: i for i, a in enumerate(algos)}
    a_members = [s.strip() for s in stack_a_spec.split(",")]
    b_members = [s.strip() for s in stack_b_spec.split(",")]
    for m in a_members + b_members:
        if m not in idx:
            sys.exit(f"algorithm '{m}' not in result ({algos})")

    trades = data["trades"]
    a_cov = sum(1 for t in trades if stack_net(t, idx, a_members) is not None)
    b_cov = sum(1 for t in trades if stack_net(t, idx, b_members) is not None)

    wins = losses = ties = 0
    win_gain = 0
    loss_drop = 0
    a_total = 0
    b_total = 0
    common = 0
    win_bps = []
    for t in trades:
        a = stack_net(t, idx, a_members)
        b = stack_net(t, idx, b_members)
        if a is None or b is None:
            continue
        common += 1
        a_total += a
        b_total += b
        if a > b:
            wins += 1
            win_gain += a - b
            if b > 0:
                win_bps.append((a - b) / b * 10_000.0)
        elif a < b:
            losses += 1
            loss_drop += b - a
        else:
            ties += 1

    win_bps.sort()
    mean_wb = sum(win_bps) / len(win_bps) if win_bps else 0.0
    med_wb = win_bps[len(win_bps) // 2] if win_bps else 0.0
    mx_wb = win_bps[-1] if win_bps else 0.0

    print(f"Stack A = best of {a_members}")
    print(f"Stack B = best of {b_members}")
    print(f"coverage: A={a_cov}  B={b_cov}   common(both solved)={common}\n")
    print(f"Stack A vs Stack B over common set:")
    print(f"  A wins : {wins:5d}  ({wins / common * 100:.2f}%)")
    print(f"  B wins : {losses:5d}  ({losses / common * 100:.2f}%)")
    print(f"  ties   : {ties:5d}  ({ties / common * 100:.2f}%)")
    print(f"  net delta (A-B) : {a_total - b_total:+.4e}")
    print(f"  win_gain={win_gain:+.3e}  loss_drop={loss_drop:+.3e}")
    if wins:
        print(f"  among {len(win_bps)} A-wins: mean={mean_wb:+.1f}bps "
              f"median={med_wb:+.1f}bps max={mx_wb:+.1f}bps")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], sys.argv[3])
