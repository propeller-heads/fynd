#!/usr/bin/env python3
"""Per order, the pools the other config used that ours did not.

This answers the one question a loss poses: did we lose because we never looked at the pool, or
because we looked at it and allocated badly? The first is a candidate-set problem, the second an
allocator problem, and they call for opposite work. `report.md` cannot tell them apart.

Reads the benchmark's `routes.jsonl`.

Usage:
    bench-harness/analysis/bench-setdiff.py bench-results/<run> --config WF_d3 --against PFW_d3
"""

import argparse
import collections
import pathlib

from bench_common import load_jsonl


def pools_of(route):
    return {edge["component_id"] for edge in (route.get("edges") or [])}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("run_dir", type=pathlib.Path)
    parser.add_argument("--config", required=True)
    parser.add_argument("--against", required=True)
    parser.add_argument("--top", type=int, default=20)
    args = parser.parse_args()

    losses = 0
    missing_pool_losses = 0
    same_pool_losses = 0
    # How often each pool shows up in a route that beat ours while we never used it. A pool at the
    # top of this is one whole class of loss, not one order's bad luck.
    unseen_pools = collections.Counter()
    unseen_pool_bps = collections.defaultdict(float)
    examples = []

    for row in load_jsonl(args.run_dir / "routes.jsonl"):
        routes = row["routes"]
        ours, theirs = routes.get(args.config), routes.get(args.against)
        if not ours or not theirs or not ours["solved"] or not theirs["solved"]:
            continue
        our_net, their_net = int(ours["amount_out_net_gas"]), int(theirs["amount_out_net_gas"])
        if our_net >= their_net:
            continue

        losses += 1
        bps = (their_net - our_net) / their_net * 10_000 if their_net > 0 else 0.0
        our_pools, their_pools = pools_of(ours), pools_of(theirs)
        unseen = their_pools - our_pools
        if unseen:
            missing_pool_losses += 1
            for pool in unseen:
                unseen_pools[pool] += 1
                unseen_pool_bps[pool] += bps
        else:
            same_pool_losses += 1

        examples.append((bps, row["order"]["id"], row["order"]["amount_usd"], sorted(unseen)))

    if not losses:
        print(f"{args.config} never loses to {args.against} in {args.run_dir.name}.")
        return

    print(f"# {args.config} losing to {args.against}   ({args.run_dir.name})\n")
    print(f"{losses} losses.\n")
    print("| kind | orders | reading |")
    print("|---|---|---|")
    print(
        f"| used a pool we did not | {missing_pool_losses} "
        f"| candidate set: the pool was never on the table |"
    )
    print(
        f"| same pools, worse split | {same_pool_losses} "
        f"| allocator: we had every pool they had |"
    )

    print("\n## Pools most often in a winning route we never touched\n")
    print("| pool | losses | total bps |")
    print("|---|---|---|")
    for pool, count in unseen_pools.most_common(args.top):
        print(f"| `{pool}` | {count} | {unseen_pool_bps[pool]:,.0f} |")

    print(f"\n## Worst {args.top} losses\n")
    print("| bps | usd | order | pools they had that we did not |")
    print("|---|---|---|---|")
    for bps, order_id, usd, unseen in sorted(examples, reverse=True)[: args.top]:
        shown = ", ".join(p[:10] for p in unseen[:3]) or "— (same pools)"
        print(f"| {bps:,.1f} | ${usd:,.0f} | `{order_id}` | {shown} |")


if __name__ == "__main__":
    main()
