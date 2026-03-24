# How the Most Liquid Router Works

The Most Liquid algorithm finds swap routes by enumerating candidate paths, scoring them with a cheap heuristic, then simulating only the most promising ones. It trades completeness for speed: it won't evaluate every possible route, but it finds good routes fast.

## Overview

The algorithm runs in four phases:

1. **Enumerate** all simple paths up to `max_hops` using BFS
2. **Score and sort** paths by a heuristic (spot price and liquidity depth)
3. **Simulate** the top-N paths using actual pool math
4. **Rank** by net output after gas cost deduction

The key insight is that phases 1-2 are cheap (graph traversal and arithmetic), while phase 3 is expensive (full AMM simulation per hop). The heuristic in phase 2 acts as a filter, ensuring simulation budget is spent on paths most likely to win.

## Phase 1: Path enumeration

Starting from the source token, BFS explores all outgoing edges up to `max_hops` depth. At each step it follows every edge (including parallel edges between the same token pair from different pools), building complete paths from source to destination.

The result is a list of all simple paths (no repeated tokens) from source to destination within the hop limit.

## Phase 2: Heuristic scoring

Each path is scored without simulation using two derived data values per edge:

- **Spot price**: the marginal exchange rate at zero trade size (includes pool fees)
- **Depth**: the pool's available liquidity in USD terms

The score for a path is:

```
score = (product of spot prices along the route) × min(depth along the route)
```

The spot price product estimates the exchange rate. The minimum depth acts as a bottleneck indicator: a path is only as liquid as its shallowest pool. Paths through deep, well-priced pools score highest.

This scoring is approximate. It ignores price impact (the spot price assumes infinitesimal trade size) and doesn't account for how liquidity changes after each hop. But it's fast and good enough to rank tens of thousands of candidates so the expensive simulation phase focuses on the right ones.

Paths are sorted by score descending. If `max_routes` is configured, only the top-N proceed to simulation.

## Phase 3: Simulation

Each surviving path is simulated end-to-end. For every hop, the algorithm calls `get_amount_out()` on the actual pool state with the running amount from the previous hop. This accounts for:

- Price impact at the exact trade size
- The pool's fee structure
- Tick crossings (Uniswap V3) or other non-linear mechanics
- Reserve state as of the latest block

If a simulation fails (e.g., insufficient liquidity in a pool), the path is discarded. Otherwise, the final output amount is recorded.

## Phase 4: Gas-adjusted ranking

Each simulated path's output is adjusted for gas cost:

```
net_output = gross_output - (total_gas * gas_price * token_price_ratio)
```

Where `total_gas` is the sum of gas estimates for each swap in the route, `gas_price` is the current block's gas price, and `token_price_ratio` converts the gas cost (in the native token) to the output token.

The path with the highest `net_output` wins.

## When it works well

- **Common pairs** (WETH/USDC, WETH/WBTC): a few high-liquidity pools dominate, and the heuristic reliably ranks them correctly.
- **Low hop counts** (2-3): the path space is small enough to enumerate exhaustively, so the heuristic filter drops very little.
- **High-frequency quoting**: the algorithm is fast enough to serve latency-sensitive integrators.

## When it struggles

- **High hop counts** (4+): the number of candidate paths grows exponentially. Even with `max_routes` capping simulation, the heuristic may not surface the best path.
- **Exotic pairs**: tokens with thin liquidity often have non-obvious routes where the spot price heuristic misjudges the actual output. The Bellman-Ford algorithm, which simulates every edge without a heuristic filter, handles these better.

## Source reference

| File | Purpose |
|---|---|
| `fynd-core/src/algorithm/most_liquid.rs` | Algorithm implementation |
| `fynd-core/src/algorithm/mod.rs` | `Algorithm` trait definition |
| `fynd-core/src/graph/petgraph.rs` | Graph implementation (petgraph::StableDiGraph) |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"most_liquid"` to `MostLiquidAlgorithm` |
| `worker_pools.toml` | Worker pool configuration |
