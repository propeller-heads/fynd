---
icon: compass
---

# Overview

Fynd ships four built-in routing algorithms. Most Liquid and Bellman-Ford each return a single
route. Path Frank-Wolfe and [Water-fill](water-fill.md) split one order across several parallel
routes to cut price impact on large trades. This section explains the routing problem and how each
algorithm works.

## The routing problem

A DEX aggregator receives a request like "swap 1 ETH for USDC" and must find the best path through a network of on-chain liquidity pools. This is a graph problem: tokens are nodes, pools are edges, and the goal is to find the path that maximizes output.

Three properties make this harder than classical shortest-path routing:

1. **Edge weights are functions, not constants.** The output of a pool depends on the input amount (price impact). A pool that gives a great rate for 0.1 ETH may give a terrible rate for 100 ETH. You cannot precompute weights once and reuse them.
2. **Weights are multiplicative, not additive.** Exchange rates multiply along a path. Shortest-path algorithms like Dijkstra assume additive costs.
3. **The best route depends on trade size.** A pool with deep liquidity wins for large trades; a shallow pool with a better spot price wins for small ones. There is no single "best route" independent of the amount.

These properties rule out off-the-shelf graph algorithms that rely on precomputed, additive,
size-independent edge weights. Fynd's algorithms handle this by simulating the actual swap math at
the points where it matters.

## How Fynd uses algorithms

Each algorithm runs inside a **worker pool**: a group of dedicated OS threads that process quote requests. Multiple worker pools can run in parallel with different algorithms and configurations (e.g., a fast 2-hop Most Liquid pool alongside a deeper 5-hop Bellman-Ford pool). The **WorkerPoolRouter** fans out each request to all pools and returns the best result.

This competition design means algorithms don't need to be perfect in isolation. A fast heuristic algorithm can win on common pairs while a thorough algorithm catches the routes the heuristic misses.

The simulation-heavy algorithms (Path Frank-Wolfe and Water-fill) simulate many candidate swaps per
request, and that cost scales with per-pool simulation time — higher for VM-simulated protocols than
for native ones. Run them in a worker pool alongside a Bellman-Ford pool: the WorkerPoolRouter
returns whichever pool answers best within the timeout, so the fast baseline still covers a request
even when the heavier pool is slow on VM-heavy routes.

See [Architecture](../ARCHITECTURE.md) for the full system design and [Custom Algorithm](../guides/custom-algorithm.md) for how to plug in your own.

## Built-in algorithms

|                        | [Most Liquid](most-liquid.md)                                       | [Bellman-Ford](bellman-ford.md)                    | [Path Frank-Wolfe](path-frank-wolfe.md)                                     | [Water-fill](water-fill.md)                                   |
| ---------------------- | ------------------------------------------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------------- | --------------------------------------------------------------- |
| **Approach**           | Enumerate paths, score by heuristic, simulate top-N                 | Simulate every reachable edge, keep best amounts   | Bellman-Ford path discovery plus Frank-Wolfe split optimization             | Tries the best single path and three ways to split the order, then returns whichever gives the most output net of gas |
| **Strengths**          | Fast; good at common, high-liquidity pairs                          | Finds non-obvious routes; no heuristic blind spots | Reduces price impact by splitting flow across parallel paths                | Never returns less than the best single path; handles every order, so it can run as the only pool; captures the gains on large trades |
| **Weaknesses**         | Path count explodes at high hop counts; heuristic can misjudge      | Single path only; suboptimal for large trades      | More simulation work per request; overkill for small trades                 | Simulates every candidate on every order, so it is slower than the single-route finders (offline 10k p50 ~15 ms) and slower still on VM-simulated protocols; run it alongside a Bellman-Ford pool |
| **Default config**     | _(not in default `worker_pools.toml`)_                              | 2 hops, 3 workers (see `worker_pools.toml`)        | _(not in default `worker_pools.toml`)_                                      | _(not in default `worker_pools.toml`)_                          |
| **Derived data needs** | Spot prices + pool depths (scoring), token gas prices (gas ranking) | Token gas prices (optional, for gas-aware mode)    | Token gas prices + spot prices (price impact, probe amount, gas cost)       | Spot prices + pool depths (candidate ranking), token gas prices (optional, gas-aware net) |
