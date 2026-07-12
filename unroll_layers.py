#!/usr/bin/env python3
"""Unroll repeated intermediate tokens per leg so the Classic Sankey (acyclic) can render.

Split routes can revisit tokens across legs (USDC->USDT in one leg, USDT->USDC in another), which is
a cycle in the merged token graph. The route-visualization workflow says to unroll repeated tokens
by hop layer. Here each leg's intermediate tokens get leg-scoped node ids (symbol preserved), while
the shared source and sink stay global — yielding a source -> per-leg chains -> sink DAG.

Input: normalized route JSON with `paths` (from add_paths.py). Output: classic-ready normalized JSON.
"""
import json
import sys
from pathlib import Path


def unroll(route: dict) -> dict:
    source, sink = route["source"], route["sink"]
    swaps = route["swaps"]
    sym = {t["id"]: t for t in route["tokens"]}
    paths = route.get("paths") or [{"share": 1.0, "swaps": list(range(len(swaps)))}]

    new_tokens = {source: dict(sym[source]), sink: dict(sym[sink])}
    new_swaps = []

    def node(orig_id: str, leg: int) -> str:
        if orig_id in (source, sink):
            return orig_id
        nid = f"{orig_id}__L{leg}"
        if nid not in new_tokens:
            base = sym.get(orig_id, {"symbol": orig_id[:8], "decimals": 18})
            new_tokens[nid] = {"id": nid, "symbol": base["symbol"], "decimals": base["decimals"]}
        return nid

    for leg, p in enumerate(paths):
        for si in p["swaps"]:
            s = swaps[si]
            new_swaps.append({
                "source": node(s["source"], leg),
                "target": node(s["target"], leg),
                "amount_in": s["amount_in"],
                "amount_out": s["amount_out"],
                "protocol": s["protocol"],
                "pool": s["pool"],
            })

    out = {
        "title": route.get("title", ""),
        "chain": route.get("chain", "ethereum"),
        "source": source,
        "sink": sink,
        "tokens": list(new_tokens.values()),
        "swaps": new_swaps,
    }
    return out


def main(in_dir: str, out_dir: str) -> None:
    Path(out_dir).mkdir(parents=True, exist_ok=True)
    for f in sorted(Path(in_dir).glob("*.json")):
        route = unroll(json.load(open(f)))
        json.dump(route, open(Path(out_dir) / f.name, "w"), indent=2)
        print(f"{f.name}: {len(route['tokens'])} nodes, {len(route['swaps'])} edges")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
