---
icon: compass
---

# Overview

Fynd ships two built-in routing algorithms. This section explains the problem they solve and how each one works.

## The routing problem

A DEX aggregator receives a request like "swap 1 ETH for USDC" and must find the best path through a network of on-chain liquidity pools. This is a graph problem: tokens are nodes, pools are edges, and the goal is to find the path that maximizes output.

Three properties make this harder than classical shortest-path routing:

1. **Edge weights are functions, not constants.** The output of a pool depends on the input amount (price impact). A pool that gives a great rate for 0.1 ETH may give a terrible rate for 100 ETH. You cannot precompute weights once and reuse them.
2. **Weights are multiplicative, not additive.** Exchange rates multiply along a path. Shortest-path algorithms like Dijkstra assume additive costs.
3. **The best route depends on trade size.** A pool with deep liquidity wins for large trades; a shallow pool with a better spot price wins for small ones. There is no single "best route" independent of the amount.

These properties rule out off-the-shelf graph algorithms that rely on precomputed, additive, size-independent edge weights. Both of Fynd's algorithms handle this by simulating the actual swap math at every step.

> **Note:** Fynd currently finds the single best path for each order. Order splitting across parallel paths is planned and will improve output for large trades where a single path exhausts pool liquidity.

## How Fynd uses algorithms

Each algorithm runs inside a **worker pool**: a group of dedicated OS threads that process quote requests. Multiple worker pools can run in parallel with different algorithms and configurations (e.g., a fast 2-hop Most Liquid pool alongside a deeper 5-hop Bellman-Ford pool). The **WorkerPoolRouter** fans out each request to all pools and returns the best result.

This competition design means algorithms don't need to be perfect in isolation. A fast heuristic algorithm can win on common pairs while a thorough algorithm catches the routes the heuristic misses.

See [Architecture](../ARCHITECTURE.md) for the full system design and [Custom Algorithm](../guides/custom-algorithm.md) for how to plug in your own.

## Built-in algorithms

|                        | [Most Liquid](most-liquid.md)                                       | [Bellman-Ford](bellman-ford.md)                    |
| ---------------------- | ------------------------------------------------------------------- | -------------------------------------------------- |
| **Approach**           | Enumerate paths, score by heuristic, simulate top-N                 | Simulate every reachable edge, keep best amounts   |
| **Strengths**          | Fast; good at common, high-liquidity pairs                          | Finds non-obvious routes; no heuristic blind spots |
| **Weaknesses**         | Path count explodes at high hop counts; heuristic can misjudge      | Slower per request; benefits less from pre-scoring |
| **Default config**     | 2-3 hops, 5+3 workers (see `worker_pools.toml`)                     | 5 hops, 3 workers                                  |
| **Derived data needs** | Spot prices + pool depths (scoring), token gas prices (gas ranking) | Token gas prices (optional, for gas-aware mode)    |
