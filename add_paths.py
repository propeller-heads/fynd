#!/usr/bin/env python3
"""Add ordered `paths` (one lane per split leg) to normalized route JSON for Route Lanes.

The offline route dump emits swaps grouped by leg (each leg is a contiguous chain from the order's
source token to its sink token). Route Lanes needs explicit ordered paths to render split routes
that revisit intermediate tokens (e.g. USDC->USDT in one leg, USDT->USDC in another). This derives
each leg and its share of the order input.
"""
import json
import sys
from pathlib import Path


def add_paths(route: dict) -> dict:
    source, sink = route["source"], route["sink"]
    swaps = route["swaps"]
    legs = []
    current = []
    for i, s in enumerate(swaps):
        if s["source"] == source:
            if current:
                legs.append(current)
            current = [i]
        else:
            current.append(i)
        if s["target"] == sink:
            legs.append(current)
            current = []
    if current:
        legs.append(current)

    total_in = sum(int(swaps[leg[0]]["amount_in"]) for leg in legs) or 1
    paths = []
    for leg in legs:
        share = int(swaps[leg[0]]["amount_in"]) / total_in
        paths.append({"share": share, "swaps": leg})
    # Normalize shares to sum exactly to 1.0.
    ssum = sum(p["share"] for p in paths) or 1.0
    for p in paths:
        p["share"] = p["share"] / ssum
    route["paths"] = paths
    return route


def main(in_dir: str, out_dir: str) -> None:
    Path(out_dir).mkdir(parents=True, exist_ok=True)
    for f in sorted(Path(in_dir).glob("*.json")):
        route = json.load(open(f))
        route = add_paths(route)
        json.dump(route, open(Path(out_dir) / f.name, "w"), indent=2)
        print(f"{f.name}: {len(route['paths'])} lanes")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
