---
icon: route
---

# Split

The Split algorithm routes a sell order across several paths when one path would create too much
price impact. It is intentionally split-focused: if it cannot assemble a real split route, it
returns no route and lets another worker pool, such as Bellman-Ford or Path Frank-Wolfe, cover the
single-path case.

Single-path algorithms answer one question: "which path should receive the whole order?" Split asks
a larger question: "which portfolio of paths should receive the order, and how much flow should
each path get?"

## Overview

The algorithm evaluates two split route families for every order:

1. **Pool-disjoint split**: split the order across paths that do not reuse pools.
2. **Shared-pool split**: split the order across paths that may share pools, while simulating the
   shared pool state in execution order.

The final selector compares the split candidates by net output and returns the best one. If neither
candidate uses at least two paths, Split returns `InsufficientLiquidity`. Production deployments
should run Split alongside single-path algorithms and let the worker router pick the best net result.

## Why splitting helps

DEX pools have price impact. The more input a route pushes through the same pool, the worse the
marginal price gets. For large trades, two medium-sized swaps through different liquidity can return
more output than one large swap through the best-looking pool.

For example, if a 1,000 WETH order can use two WETH/USDC pools:

```
Pool A: best for the first 500 WETH
Pool B: worse at spot, but deeper after Pool A starts moving
```

A single-path router sends all 1,000 WETH through Pool A or Pool B. Split can send part through
each pool, reducing price impact and improving net output after gas.

## Candidate discovery

Split starts with the same path enumeration machinery as Most Liquid:

1. Enumerate simple paths from input token to output token up to `max_hops`.
2. Score paths by spot price and pool depth.
3. Keep the top candidates, bounded by `max_routes` or the internal default.

This gives Split a broad set of plausible paths without simulating every path in the graph.

The default candidate cap is intentionally modest because Split runs as one worker pool in a larger
algorithm portfolio. It does not try to preserve every path that might win as a standalone route.

## Candidate ranking

Every candidate path is probed at the full order amount with real pool math. These probes are used
only to rank paths before allocation. They are not returned as fallback routes.

This keeps the split search focused:

* Better full-size paths are considered first for pool-disjoint splitting.
* The top full-size paths seed the shared-pool candidate set.
* If no path can be simulated, Split returns `InsufficientLiquidity`.

## Pool-disjoint split

The first split candidate uses paths that do not share pools. This is the conservative route family:
each path can be simulated independently because no path changes the pool state seen by another.

The allocation uses water-filling:

1. Divide the order into fixed chunks.
2. For each chunk, test which active or inactive path gives the best marginal net output.
3. Charge activation gas when a path receives its first chunk.
4. Put the chunk on the best marginal path.
5. Repeat until all chunks are allocated.

The result is simple and execution-safe. It captures the common case where parallel pools or
parallel token paths can absorb a large trade better than one route.

## Shared-pool split

The second split candidate allows paths to overlap. This is needed for routes that share an entry
pool, split downstream, or converge before the final output token.

The hard part is pool state. If two candidate paths use the same pool, the second use must see the
state left by the first use. Split handles this by committing chunks through a shared override map:

1. Pick shared-pool candidates from the top full-size paths and best first-chunk marginal probes.
2. Walk through the order in fixed chunks.
3. For each chunk, simulate every eligible path against the current override state.
4. Choose the path with the best marginal net output.
5. Commit that path's pool-state updates before evaluating the next chunk.

This avoids the main shared-pool benchmark bug: double-counting the same liquidity as if every path
could use the original pool state.

## Route assembly

After allocation, Split builds an executable route from the simulated path allocations.

The route builder does four important things:

1. **Merge shared hops.** If two paths contain the same pool in the same direction, the route emits
   one combined swap instead of duplicate swaps.
2. **Count gas once for merged hops.** A shared hop pays for one on-chain swap, not one swap per
   path that used it in the allocation model.
3. **Emit swaps in token dependency order.** If several upstream swaps produce USDC, the USDC
   outgoing swap waits until all upstream inflows are ready.
4. **Use the router split convention.** In each branch collection, all but the last swap carry an
   explicit split fraction. The last swap uses `split = 0.0`, meaning "use the remaining balance".

The route also includes the token map needed by Tycho execution encoding. The shared-prefix
regression test converts the resulting quote into a Tycho execution `Solution` to catch lossy route
encoding.

## Gas and ranking

Split ranks candidates by net output:

```
net_output = gross_output - (total_gas * gas_price * token_price_ratio)
```

If token price data is unavailable, the algorithm falls back to gross output for that comparison.
When price data is available, gas can prevent unnecessary fragmentation inside the split allocation.
A split route is only returned if the allocator uses at least two paths.

## When it works well

* **Large trades** where one pool would create high price impact.
* **Pairs with parallel liquidity** across several pools or token paths.
* **Routes with shared prefixes or downstream splits**, for example one entry pool feeding two
  output-side pools.

## When it struggles

* **Small trades** where gas dominates the price-impact benefit. These should usually be handled by
  Bellman-Ford, Path Frank-Wolfe, or another single-path-capable worker pool.
* **Strict latency budgets**. Split does more simulation work than Bellman-Ford and Path
  Frank-Wolfe.
* **Fine-grained allocation cases**. The allocation is chunk-based, not a continuous optimizer, so a
  smoother method can still win some trades.
* **Heuristic path ranking**. Split starts from Most Liquid's spot-depth path ranking, so a useful
  route that is buried by that heuristic can still be missed.

## Source reference

| File | Purpose |
| --- | --- |
| `fynd-core/src/algorithm/split.rs` | Candidate discovery, allocation, route selection |
| `fynd-core/src/algorithm/split_primitives.rs` | Split math, shared-hop merging, executable route assembly |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"split"` to `SplitAlgorithm` |
