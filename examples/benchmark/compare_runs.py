#!/usr/bin/env python3
"""
Compare two benchmark runs (old BF vs new BF).

Usage:
    python3 compare_runs.py <old_results.json> <new_results.json>
"""
import json
import sys
from collections import Counter


def load_results(path):
    with open(path) as f:
        data = json.load(f)
    return data["summary"], data["results"]


def compare(old_path, new_path):
    old_summary, old_results = load_results(old_path)
    new_summary, new_results = load_results(new_path)

    print("=" * 60)
    print("  Bellman-Ford A/B Comparison")
    print("=" * 60)

    print(f"\n{'Metric':<30} {'Old BF':>12} {'New BF':>12}")
    print("-" * 54)
    for key in ["total_trades", "successes", "no_routes", "failures",
                "total_time_s", "rate_rps", "solve_time_p50_ms",
                "solve_time_p95_ms", "solve_time_mean_ms",
                "solve_time_max_ms"]:
        old_val = old_summary.get(key, "N/A")
        new_val = new_summary.get(key, "N/A")
        print(f"  {key:<28} {str(old_val):>12} {str(new_val):>12}")

    # Build index -> result maps
    old_by_idx = {r["index"]: r for r in old_results}
    new_by_idx = {r["index"]: r for r in new_results}

    # Pairwise comparison on trades where both succeeded
    common_indices = set(old_by_idx.keys()) & set(new_by_idx.keys())

    new_wins = 0
    old_wins = 0
    ties = 0
    both_success = 0
    only_old_success = 0
    only_new_success = 0
    both_fail = 0
    improvements = []

    for idx in sorted(common_indices):
        old_r = old_by_idx[idx]
        new_r = new_by_idx[idx]

        old_ok = old_r.get("status") == "success"
        new_ok = new_r.get("status") == "success"

        if old_ok and new_ok:
            both_success += 1
            old_net = int(old_r.get("amount_out_net_gas", "0"))
            new_net = int(new_r.get("amount_out_net_gas", "0"))

            if new_net > old_net:
                new_wins += 1
                if old_net > 0:
                    pct = (new_net - old_net) / old_net * 100
                    improvements.append(pct)
            elif old_net > new_net:
                old_wins += 1
            else:
                ties += 1
        elif old_ok and not new_ok:
            only_old_success += 1
        elif not old_ok and new_ok:
            only_new_success += 1
        else:
            both_fail += 1

    total_contested = both_success
    print(f"\n{'Pairwise Comparison':<30}")
    print("-" * 54)
    print(f"  {'Both succeeded (contested)':<28} {total_contested:>12}")
    print(f"  {'New BF wins':<28} {new_wins:>12}"
          f" ({new_wins/total_contested*100:.1f}%)" if total_contested else "")
    print(f"  {'Old BF wins':<28} {old_wins:>12}"
          f" ({old_wins/total_contested*100:.1f}%)" if total_contested else "")
    print(f"  {'Ties':<28} {ties:>12}"
          f" ({ties/total_contested*100:.1f}%)" if total_contested else "")
    print(f"  {'Only old succeeded':<28} {only_old_success:>12}")
    print(f"  {'Only new succeeded':<28} {only_new_success:>12}")
    print(f"  {'Both failed':<28} {both_fail:>12}")

    if improvements:
        improvements.sort()
        avg_imp = sum(improvements) / len(improvements)
        med_imp = improvements[len(improvements) // 2]
        print(f"\n{'Improvement stats (when new wins)':}")
        print(f"  Mean:   {avg_imp:.2f}%")
        print(f"  Median: {med_imp:.2f}%")
        print(f"  Max:    {max(improvements):.2f}%")

    # Hop distribution comparison
    print(f"\n{'Hop Distribution':<30}")
    print("-" * 54)
    for label, results in [("Old BF", old_results), ("New BF", new_results)]:
        hops = Counter()
        for r in results:
            if r.get("status") == "success":
                hops[r.get("hops", "?")] += 1
        hop_str = ", ".join(f"{k}h:{v}" for k, v in sorted(hops.items()))
        print(f"  {label}: {hop_str}")

    print("\n" + "=" * 60)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: compare_runs.py <old_results.json> <new_results.json>")
        sys.exit(1)
    compare(sys.argv[1], sys.argv[2])
