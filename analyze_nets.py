#!/usr/bin/env python3
"""Exact per-algorithm net comparison from a quality-benchmark output JSON.

The benchmark's printed `total_net` is truncated to 4 significant digits, which hides
real differences between close algorithms. This reads the full per-trade `nets` and
reports exact totals, win/loss/tie counts, and net deltas against a baseline.
"""
import json
import sys
from collections import defaultdict


def main(path: str, baseline: str) -> None:
    with open(path) as f:
        data = json.load(f)
    algos = data["algorithms"]
    trades = data["trades"]
    bi = algos.index(baseline)

    # Common set: every algo solved.
    common = [t for t in trades if all(n is not None for n in t["nets"])]
    print(f"algorithms: {algos}")
    print(f"trades={len(trades)} common(all-solved)={len(common)} baseline={baseline}\n")

    coverage = {a: sum(1 for t in trades if t["nets"][i] is not None)
                for i, a in enumerate(algos)}

    for i, a in enumerate(algos):
        total = sum(int(t["nets"][i]) for t in common)
        wins = losses = ties = 0
        win_gain = 0
        loss_drop = 0
        for t in common:
            v = int(t["nets"][i]); b = int(t["nets"][bi])
            if v > b:
                wins += 1; win_gain += v - b
            elif v < b:
                losses += 1; loss_drop += b - v
            else:
                ties += 1
        delta = total - sum(int(t["nets"][bi]) for t in common)
        # bps among wins only (magnitude of the improvement where it wins).
        win_bps = []
        for t in common:
            v = int(t["nets"][i]); b = int(t["nets"][bi])
            if v > b and b > 0:
                win_bps.append((v - b) / b * 10_000.0)
        win_bps.sort()
        mean_wb = sum(win_bps) / len(win_bps) if win_bps else 0.0
        med_wb = win_bps[len(win_bps) // 2] if win_bps else 0.0
        mx_wb = win_bps[-1] if win_bps else 0.0
        print(f"{a:14s} cov={coverage[a]:5d}  W/L/T={wins:5d}/{losses:5d}/{ties:5d}  "
              f"net_delta_vs_base={delta:+.3e}  win_gain={win_gain:+.3e} loss_drop={loss_drop:+.3e}")
        if wins:
            print(f"{'':14s}   among {len(win_bps)} wins vs base: "
                  f"mean={mean_wb:+.1f}bps median={med_wb:+.1f}bps max={mx_wb:+.1f}bps")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "split")
