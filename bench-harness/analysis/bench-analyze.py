#!/usr/bin/env python3
"""Break a benchmark run down by order size and route shape.

The report the benchmark writes says how often a config won. This says *where*: which order
sizes it won in, what shape of route it returned there, and which orders it lost worst. That is
the input to deciding what to change next, and it is deliberately not a summary -- every table
here points back at order ids that can be re-run under the profiler.

Usage:
    bench-harness/analysis/bench-analyze.py bench-results/<run> --config <name> --against <name> [--worst N]
"""

import argparse
import pathlib
import statistics
import sys

from bench_common import load_jsonl

# Order-size bins in USD. The dataset is 1k+ with a median near 4k and a p99 near 1M, so the
# bins are decade-ish and the top one is left open: the largest orders are the ones a split is
# built for and lumping them with the merely-large hides the case that matters.
BINS = [
    ("<5k", 0, 5_000),
    ("5k-25k", 5_000, 25_000),
    ("25k-100k", 25_000, 100_000),
    ("100k-500k", 100_000, 500_000),
    ("500k+", 500_000, float("inf")),
]


def bin_of(usd):
    for name, lo, hi in BINS:
        if lo <= usd < hi:
            return name
    return BINS[-1][0]


def count_fan_out_and_swaps(route):
    """How many parallel paths the route splits into, and how many swaps it took."""
    edges = route.get("edges") or []
    if not edges:
        return (0, 0)
    # A leg's `split` is the fraction of its input token that this swap took. A fresh parallel
    # path shows up as a swap consuming the order's input token, so counting those counts the
    # fan-out.
    first_token = edges[0]["token_in"]
    fan_out = sum(1 for edge in edges if edge["token_in"] == first_token)
    return (fan_out, len(edges))


def bps(ours, theirs):
    """Our advantage over theirs, in basis points of theirs."""
    if theirs <= 0:
        return None
    return (ours - theirs) / theirs * 10_000


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("run_dir", type=pathlib.Path)
    parser.add_argument("--config", required=True, help="the config under test")
    parser.add_argument("--against", required=True, help="the config to compare it to")
    parser.add_argument("--worst", type=int, default=15, help="how many worst losses to list")
    args = parser.parse_args()

    rows = list(load_jsonl(args.run_dir / "routes.jsonl"))
    if not rows:
        sys.exit(f"no routes in {args.run_dir}/routes.jsonl")

    per_bin = {name: [] for name, _, _ in BINS}
    losses = []
    only_us, only_them, neither = [], [], []

    for row in rows:
        routes = row["routes"]
        ours = routes.get(args.config)
        theirs = routes.get(args.against)
        if ours is None or theirs is None:
            sys.exit(f"run has configs {sorted(routes)}, not {args.config!r} and {args.against!r}")

        order = row["order"]
        usd = order["amount_usd"]
        we_solved, they_solved = ours["solved"], theirs["solved"]
        if not we_solved and not they_solved:
            neither.append(order["id"])
            continue
        if we_solved and not they_solved:
            only_us.append((usd, order["id"]))
            continue
        if they_solved and not we_solved:
            only_them.append((usd, order["id"], ours.get("failure")))
            continue

        advantage = bps(int(ours["amount_out_net_gas"]), int(theirs["amount_out_net_gas"]))
        if advantage is None:
            continue
        per_bin[bin_of(usd)].append((advantage, ours, theirs))
        if advantage < 0:
            losses.append((
                advantage,
                usd,
                order["id"],
                count_fan_out_and_swaps(ours),
                count_fan_out_and_swaps(theirs),
            ))

    print(f"# {args.config} vs {args.against}   ({args.run_dir.name})\n")
    print("## By order size\n")
    print("| bin | orders | mean bps | median bps | win | loss | tie | our fan-out | their fan-out |")
    print("|---|---|---|---|---|---|---|---|---|")
    for name, _, _ in BINS:
        entries = per_bin[name]
        if not entries:
            print(f"| {name} | 0 | — | — | — | — | — | — | — |")
            continue
        advantages = [a for a, _, _ in entries]
        wins = sum(1 for a in advantages if a > 0)
        drops = sum(1 for a in advantages if a < 0)
        ties = len(advantages) - wins - drops
        our_fan = statistics.mean(count_fan_out_and_swaps(o)[0] for _, o, _ in entries)
        their_fan = statistics.mean(count_fan_out_and_swaps(t)[0] for _, _, t in entries)
        print(
            f"| {name} | {len(advantages)} | {statistics.mean(advantages):+.1f} "
            f"| {statistics.median(advantages):+.1f} | {wins} | {drops} | {ties} "
            f"| {our_fan:.2f} | {their_fan:.2f} |"
        )

    every = [a for entries in per_bin.values() for a, _, _ in entries]
    if every:
        wins = sum(1 for a in every if a > 0)
        drops = sum(1 for a in every if a < 0)
        print(
            f"\n**overall** {len(every)} compared, mean {statistics.mean(every):+.1f} bps, "
            f"median {statistics.median(every):+.1f} bps, {wins} better, {drops} worse, "
            f"{len(every) - wins - drops} tie"
        )
        # The gate is stated on orders where the two configs returned different output. An order
        # both configs answered identically is not a contest that was drawn, it is one that did not
        # happen, and including them pins the median at zero before any algorithm runs.
        contested = [a for a in every if a != 0]
        if contested:
            print(
                f"\n**contested only** ({len(contested)} of {len(every)}) "
                f"mean {statistics.mean(contested):+.1f} bps, "
                f"median {statistics.median(contested):+.1f} bps, "
                f"win rate {wins / len(contested) * 100:.1f}%"
            )
        else:
            print("\n**contested only** none — the two configs agreed on every order")
    print(
        f"\ncoverage: {len(only_us)} solved only by {args.config}, "
        f"{len(only_them)} only by {args.against}, {len(neither)} by neither"
    )
    for usd, order_id, failure in sorted(only_them, reverse=True)[:10]:
        print(f"  missed ${usd:,.0f}  {order_id}  ({failure})")

    print(f"\n## Worst {args.worst} losses\n")
    print("| bps | usd | order | our fan-out/swaps | their fan-out/swaps |")
    print("|---|---|---|---|---|")
    for advantage, usd, order_id, ours_counted, theirs_counted in sorted(losses)[: args.worst]:
        our_fan_out, our_swaps = ours_counted
        their_fan_out, their_swaps = theirs_counted
        print(
            f"| {advantage:+.1f} | ${usd:,.0f} | `{order_id}` "
            f"| {our_fan_out}/{our_swaps} | {their_fan_out}/{their_swaps} |"
        )


if __name__ == "__main__":
    main()
