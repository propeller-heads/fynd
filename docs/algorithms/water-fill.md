---
icon: route
---

# Water-fill

`water_fill` splits one order across several parallel routes to reduce price impact on large trades.
It tries a few ways to split the order, simulates each, and returns whichever gives the most output
after gas — and never less than the best single route.

It runs on the `DepthAndPrice` weighted graph (like Most Liquid). It has no hard derived-data
requirement: it uses token gas prices when they are available but tolerates stale ones and never
waits on them. With gas prices, ranking is gas-aware — it subtracts each path's activation cost and
the route's gas from the output. Without them, it ranks on gross output.

## What it returns

For each order, water-fill builds up to four candidate routes and returns the one with the highest
output net of gas. If none beats the single path, it returns the single path.

1. **Best single path** — the best route that carries the whole order on its own. This is the
   floor: water-fill never returns less than this.
2. **20-chunk disjoint split** — splits the order across paths that share no pool ("pool-disjoint"),
   allocating in 20 chunks. This is the safety net: it is cheap and always finishes, even under a
   tight timeout, so water-fill can still beat the single path when a split would win (see
   [Never-lose floor](#never-lose-floor)).
3. **256-chunk disjoint split** — the same pool-disjoint paths, allocated in 256 chunks for a finer
   split. It runs in two phases (see [Two-phase activation](#two-phase-activation)) and then an
   [exchange-refinement](#exchange-refinement) pass.
4. **Fill-and-spill** — a split that lets paths share a pool and branch at an intermediate token
   (a "tree route"). The pool-disjoint splits above cannot express these.

## Incremental water-fill

Every split fills the order in small chunks. For each chunk, water-fill checks how much each path
would return for that chunk, gives the chunk to the best one, and moves on.

Constant-product and tick AMMs are path-independent in cumulative input: one swap of `x` returns the
same as two back-to-back swaps that sum to `x`. So instead of re-simulating each path at its full
running total for every chunk (O(chunks²)), water-fill simulates only the next chunk against the
pool state committed so far (O(chunks)). That saved work is what makes the 256-chunk split
affordable.

## Two-phase activation

Splitting into smaller chunks can backfire on large trades. A path only pays off once it carries
enough volume to cover its gas cost (its "activation gate"). If the first chunk given to a path is
too small, the path fails that gate and never turns on — even when it would be profitable at a
larger share.

Water-fill avoids this in two phases: first decide which paths to use at coarse (20-chunk)
granularity, where each path's share is large enough for the gate to be meaningful; then allocate
across the chosen paths at fine (256-chunk) granularity with the gate off, since their gas is
already covered.

## Exchange refinement

A chunk-by-chunk split can never take back a chunk it already gave out, so the refined split is only
accurate to one chunk (1/256 of the order) and can sit slightly off the ideal split. The exchange
pass corrects this. Starting from the refined split, it moves a small amount of input from an
over-allocated path to an under-allocated one, keeping the move only if the two paths' combined
output goes up. When no move helps, it halves the amount and tries again, down to a floor below one
chunk. The paths share no pool, so each trial re-simulates only the two paths it touches. The pass
stops at the solve timeout or a cap on trial simulations. Because it keeps only moves that improve
output, the result is never worse than the split it started from.

## Candidate discovery

Water-fill finds candidate routes two ways and combines them, so no useful route is dropped:

1. **Exhaustive enumeration** — lists paths between the sell and buy tokens (breadth-first) and
   ranks them by a spot-price × depth heuristic, reusing [Most Liquid](most-liquid.md) on the same
   `DepthAndPrice` graph (see the Dependency note below).
2. **Bounded amount-aware search** — a frontier search (the discovery section of `water_fill.rs`)
   that expands from the sell token with the full order amount, preferring edges toward the buy
   token, the `connector_tokens` allowlist, or a set of anchor tokens.

Anchors are a **soft ranking hint**, distinct from the `connector_tokens` **hard allowlist**: when
no `connector_tokens` allowlist is set, discovery prefers to expand through anchors but still
reaches any other token. The anchor set is derived per solve from the graph itself — the most
connected tokens (highest pool-edge degree, the same signal `derive-connector-tokens` ranks by)
plus the native-ETH sentinel `0x0000000000000000000000000000000000000000`. Deriving from live
connectivity keeps anchoring correct on every chain with no hardcoded per-chain list; the sentinel
is anchored explicitly so WETH → ETH → token routes survive on full Fynd setups where Tycho models
native ETH as the zero address.

The bounded set is placed ahead of the heuristic-ranked set, so connector and anchor routes survive
the spot × depth cutoff. If the bounded search fails, it is not fatal: the exhaustive set already
guarantees a route.

> **Dependency:** water-fill's discovery and single-path ranking are built on Most Liquid's
> `find_paths`, `try_score_path`, and `simulate_path`. Changing those Most Liquid internals changes
> water-fill's candidate set and ranking — keep both in mind when editing either algorithm.

## Route assembly

Every returned candidate is assembled through the shared split primitives (`build_split_route`),
which emit swaps in topological order, apply the tycho-execution remainder-split convention, merge
shared hops (paths that share a prefix pool produce one combined swap with split downstream legs),
and attach the route's token map. If a candidate's legs cannot be topologically ordered — for
example two disjoint legs that form a token cycle — assembly returns nothing and that candidate is
dropped, so water-fill falls back to another candidate or the single path. Assembly is the same code
the router encodes on-chain, so every route water-fill returns can be encoded.

## Never-lose floor

Water-fill never returns less net output than the best single path. This holds even under a tight
timeout because the 20-chunk disjoint split (candidate 2) is cheap and always finishes on the same
solve clock — a timeout cannot cut off the split while leaving the single path, so if a split wins,
water-fill returns it.

Because water-fill always returns at least the best single path, it can be the only pool on a chain.
A split router that returned nothing when it declined to split would need a single-path pool beside
it to answer those orders.


## Configuration

```toml
[pools.water_fill_4_hops]
algorithm = "water_fill"
num_workers = 1
task_queue_capacity = 1000
min_hops = 1
max_hops = 4
timeout_ms = 60000
max_routes = 1024
```

Set `connector_tokens` to restrict intermediate hops to a trusted allowlist (a hard filter); see
[Server Configuration](../guides/server-configuration.md). The soft anchor preference used when no
allowlist is set is derived automatically from the graph and needs no configuration.

## Source Reference

| File | Purpose |
| --- | --- |
| `fynd-core/src/algorithm/water_fill.rs` | `WaterFillAlgorithm`: candidate allocation (20-chunk disjoint split, 256-chunk disjoint split, fill-and-spill) and bounded candidate discovery |
| `fynd-core/src/algorithm/most_liquid.rs` | [Most Liquid](most-liquid.md) path finder reused for discovery and ranking: `find_paths`, `try_score_path`, `simulate_path`, and the `DepthAndPrice` graph weight |
| `fynd-core/src/algorithm/split_primitives.rs` | Shared-hop merging and executable route assembly |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"water_fill"` to `WaterFillAlgorithm` |
