---
icon: route
---

# Split

`split` is Fynd's production split-routing algorithm: it splits one order across multiple parallel
paths to reduce price impact on large trades. It is a portfolio router — it evaluates several
allocation strategies and returns the best net-of-gas result, never worse than the best single
path.

It runs on the `DepthAndPrice` weighted graph (like Most Liquid) and declares no hard derived-data
requirement: it can use token gas prices when available but tolerates stale ones and never waits on
them. When token gas prices are present, ranking is gas-aware — path activation costs and the final
route net subtract gas in output-token terms — and without them it falls back to gross output.

## What it returns

For each order the portfolio assembles up to four candidates and returns the one with the highest
net output, or the best single path if none beats it:

1. **Best single path** — the highest-net full-order route from the candidate set. This is the
   floor: the portfolio never returns less than this.
2. **Coarse disjoint floor** — a single gated coarse water-fill over pool-disjoint paths on the
   same 20-chunk grid, so it does exactly the incumbent split's allocation work at the same cost.
   Because it shares the solve clock with that work, a tight timeout cannot starve it into a
   single-path fallback while a split would still win.
3. **Refined disjoint split** — a two-phase pool-disjoint allocation: pick the active path set at
   coarse (20-chunk) granularity where the gas-activation gate is correct, then refine the
   allocation over that fixed set on a fine 256-chunk grid with no gate (the gas is already
   justified). An exchange-refinement pass then polishes the result below the grid.
4. **Fill-and-spill** — a shared-pool overlay allocation with marginal-probe candidate selection,
   which can express splits that diverge at an intermediate token (tree routes sharing a pool) that
   no pool-disjoint allocation can.

## Incremental water-fill

Every allocation pass water-fills the order in chunks, probing each path's marginal output for the
next chunk against a committed pool-state overlay. Because constant-product and tick AMMs are
path-independent in cumulative input (one swap of `x` equals two sequential swaps summing to `x`),
probing the marginal of the next chunk against a committed overlay is identical to re-simulating at
the cumulative amount, but costs O(chunks) instead of O(chunks²). That saved work is what funds the
fine 256-chunk grid.

## Two-phase activation

Naive fine-graining regresses on large trades: a smaller first chunk can fail a path's per-chunk
gas-activation gate, so a profitable second path never turns on. The fix is two phases — decide the
active path set at coarse granularity, where the activation gate is correct, then refine the
allocation over that fixed set with the gate off, since the gas is already justified.

## Exchange refinement

A greedy water-fill can never un-commit a chunk, so the refined disjoint allocation is quantized to
one fine chunk (1/256 of the order) and can sit up to a chunk off the true equal-marginal split. The
exchange pass fixes that: warm-starting from the fine allocation, it shifts a delta of input from an
over-allocated path to an under-allocated one whenever the pair's summed net output strictly
improves, halving the delta once no move helps, down to a sub-chunk floor. Active paths are
pool-disjoint, so a trial re-simulates only the two paths it touches, and the pass is bounded by the
solve timeout and a hard cap on trial simulations. Because only strictly-improving moves are
accepted, the result never scores below the water-fill allocation.

## Candidate discovery

Discovery unions two sources so no useful route is dropped:

1. **Exhaustive BFS** enumeration of paths between the input and output tokens (the Most Liquid path
   finder), ranked by a spot-price × depth heuristic.
2. **Bounded amount-aware search** — a Penumbra-inspired frontier search (the discovery section of
   `split.rs`), expanding from the sell token with the full amount and preferring edges into the
   output token, configured `connector_tokens`, or a default anchor set. Its anchors include the
   native ETH sentinel `0x0000000000000000000000000000000000000000`, so WETH → ETH → token routes
   survive on full Fynd setups where Tycho models native ETH as the zero address.

The bounded set is unioned ahead of the heuristic-ranked set, so connector and anchor routes survive
the spot × depth truncation. Discovery failure in the bounded search is not fatal: the exhaustive
set already guarantees a route.

## Route assembly

Every returned candidate is assembled through the shared split primitives (`build_split_route`),
which emit swaps in topological order, apply the tycho-execution remainder-split convention, merge
shared hops (paths that share a prefix pool produce one combined swap with split downstream legs),
and attach the route's token map. If a candidate's legs cannot be topologically ordered — for
example two disjoint legs that form a token cycle — assembly returns nothing and that candidate is
dropped, so the portfolio falls back to the floor or the single path. Assembly is the same code the
router encodes on-chain, so every route the portfolio returns encodes.

## Never-lose floor

The portfolio's guarantee is that it never returns less net than the best single path, and it holds
under time pressure because the coarse disjoint floor does exactly the incumbent's work on the
shared clock. Because `split` always returns at least the best single path, it can run as the only
pool for a chain — a split router that returned nothing whenever it declined to split would need a
single-path pool running alongside it to answer those orders.

The property is enforced by unit tests: `small_order_does_not_lose_to_single_path`,
`portfolio_output_near_two_pool_optimum` (within 0.1% of the analytical two-pool optimum),
`portfolio_no_loss_under_tight_timeout` (1/5/50 ms), and `split_beats_single_path_on_two_equal_pools`.

## Configuration

```toml
[pools.split_4_hops]
algorithm = "split"
num_workers = 1
task_queue_capacity = 1000
min_hops = 1
max_hops = 4
timeout_ms = 60000
max_routes = 1024
```

Use `connector_tokens` when a deployment has domain-specific intermediate tokens that should be kept
in the search even if they are not part of the default anchor set.

## Source Reference

| File | Purpose |
| --- | --- |
| `fynd-core/src/algorithm/split.rs` | `SplitAlgorithm`: portfolio allocation (floor, refined disjoint, fill-and-spill) and bounded candidate discovery |
| `fynd-core/src/algorithm/split_primitives.rs` | Shared-hop merging and executable route assembly |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"split"` to `SplitAlgorithm` |
