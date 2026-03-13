# Bellman-Ford Algorithm: Handover Document

## Purpose

This document explains the Bellman-Ford routing algorithm in Fynd, step by step, function by function. It covers the origin (Janos Tapolcai's research), how we adapted it, and a planned modification to forbid token/pool revisits.

## Table of Contents

1. [The Big Picture](#the-big-picture)
2. [Layer 1: Classical Bellman-Ford](#layer-1-classical-bellman-ford)
3. [Layer 2: Janos's Algorithm (Arbitrage)](#layer-2-janoss-algorithm-arbitrage)
4. [Layer 3: Our Adaptation (A-to-B Routing)](#layer-3-our-adaptation-a-to-b-routing)
5. [Function-by-Function Walkthrough](#function-by-function-walkthrough)
6. [Key Design Decisions](#key-design-decisions)
7. [Planned Modification: Forbid Revisits](#planned-modification-forbid-token-and-pool-revisits)

---

## The Big Picture

There are three layers to understand:

1. **The classical Bellman-Ford algorithm** (textbook shortest-path)
2. **Janos's modification** for arbitrage cycle detection (paper + `tycho-searcher` repo)
3. **Our adaptation** for A-to-B routing in Fynd (PR #43)

Each layer builds on the previous one, but each makes significant changes.

### Source Material

- **Janos's paper**: `tycho-searcher/doc/searcher_documentation.tex` (LaTeX, also on [Overleaf](https://www.overleaf.com/read/ksqhzzmndmqh#9e8003))
- **Janos's implementation**: [github.com/jtapolcai/tycho-searcher](https://github.com/jtapolcai/tycho-searcher), specifically `src/searcher/bellman_ford.rs`
- **Our PR**: [fynd PR #43](https://github.com/propeller-heads/fynd/pull/43), specifically `src/algorithm/bellman_ford.rs`

---

## Layer 1: Classical Bellman-Ford

The textbook algorithm finds shortest paths from a source to all nodes in a weighted graph.

**Data structures:**
- `distance[node]`: best known distance from source to each node
- `predecessor[node]`: which edge led to that best distance

**Steps:**
1. Initialize source distance to 0, all others to infinity
2. **Relax** every edge up to V-1 times. "Relaxation" means: if going through edge u->v gives a better distance to v than currently known, update v's distance and predecessor.
3. After V-1 rounds, if any edge can still be relaxed, there's a negative cycle.

**Key property:** After k rounds, the algorithm has found all shortest paths using at most k edges.

---

## Layer 2: Janos's Algorithm (Arbitrage)

Janos adapted Bellman-Ford for **arbitrage cycle detection** on DEX graphs. The goal: find cycles where you start with X tokens of WETH and end up with more than X tokens of WETH.

### Graph Model

- **Nodes** = tokens (WETH, USDC, DAI, etc.)
- **Edges** = AMM pool swap directions (each pool creates directed edges for each token pair it supports)
- **Edge weights are functions, not constants**: `w_e(x_in)` returns the output amount for a given input. This is the `get_amount_out()` simulation call. The output depends on pool state, fees, and slippage.

### Graph Decomposition (Precomputation)

Before running BF, Janos decomposes the graph to create independent subproblems:

1. Start from WETH (the start token for arbitrage)
2. Remove WETH from the graph
3. Find connected components of the remaining graph
4. Add WETH back to each component, creating independent subgraphs

Any arbitrage cycle through WETH must stay within one subgraph. A graph with ~2,400 nodes decomposes into ~2,000 components, with the largest having ~460 nodes. This enables incremental updates (only recompute affected subgraphs) and potential parallelization.

### The Modified Bellman-Ford (Algorithm 1 in the paper)

The core modifications from textbook BF:

**Two distance arrays instead of one:**
- `distance[v]` ("without loop") = best amount reachable at v via a path that visits each token at most once
- `distance_with_loop[v]` ("with a single loop") = best amount reachable at v via a path that visits one token twice

Why two arrays? Profitable arbitrage paths sometimes need to go through a token twice. Example: WETH -> USDC -> WETH -> DAI -> WETH. The paper tracks paths with zero loops and paths with exactly one loop separately.

**Pool-visit check instead of node-visit check:** The paper requires that each **pool** (AMM) is used at most once in a path, because a pool's state changes after a swap. But it allows visiting the same **token** via different pools.

**Relaxation with simulation:** Instead of adding edge weights numerically, each relaxation calls `get_amount_out()` to simulate the actual swap with the current amount. This is computationally expensive but gives exact results accounting for slippage, fees, and pool mechanics.

**Gas-aware comparison:** When comparing paths, the algorithm considers both the output amount and the gas cost: `amount_out - gas_price * gas_used`. A path that produces more tokens but costs much more gas might not be better.

**Cycle detection:** After BF converges, check all edges pointing back to the source (WETH). If `amount_out > amount_in + gas_cost`, that's a profitable cycle.

### Amount Optimization (Golden Section Search)

The BF runs with a small fixed input (e.g., 0.001 ETH). Once cycles are found, Golden Section Search (GSS) optimizes the input amount for maximum profit. This works because profit as a function of input amount is unimodal: it rises (more volume = more absolute profit) then falls (slippage overtakes the gain).

### Janos's Implementation Characteristics

Looking at `tycho-searcher/src/searcher/bellman_ford.rs`:
- Uses `f64` for amounts in the outer loop, `BigUint` for actual simulations
- Wraps edge data in `RefCell` for interior mutability
- `has_node_in_path()` walks the predecessor chain to check if a node would create a cycle
- `get_path()` and `get_path_with_loop()` reconstruct paths from predecessor arrays (two variants for the two distance arrays)
- `evaluate_cycle_profit()` re-simulates a cycle end-to-end with a given input amount
- `golden_section_search_with_gas()` finds the optimal input amount per cycle
- Comments in Hungarian throughout (e.g., "Nagyobb counter", "legalabb 10%-kal noveljuk")
- Runs synchronously, processes one subgraph at a time

---

## Layer 3: Our Adaptation (A-to-B Routing)

We adapted Janos's algorithm from **cycle detection** to **A-to-B routing**. This is a fundamental change in purpose: we're not looking for WETH->...->WETH cycles; we're finding the best path from `token_in` to `token_out` for a given order.

### Key Architectural Differences from Janos

#### 1. Layered distance instead of flat + loop arrays

**Janos:** `distance[node]` and `distance_with_loop[node]` (two flat arrays)
**Ours:** `distance[hop][node]` (2D array, one layer per hop count)

With layered distances, each hop layer is independent. `distance[k][v]` = the best amount reachable at node v using exactly k edges. This means visiting WETH at hop 3 doesn't conflict with a WETH visit at hop 1.

This structure was adopted after a debugging session where the initial flat-array implementation (with cycle prevention like Janos's) blocked too many good routes. The win rate went from 10% to 97% after switching to layered distances.

**Note:** This decision is being revisited. See [Planned Modification](#planned-modification-forbid-token-and-pool-revisits).

#### 2. BFS subgraph extraction instead of WETH decomposition

Janos decomposes around WETH (the universal hub for arbitrage). We do BFS from `token_in` up to `max_hops` depth instead. This prunes the graph from ~10K edges to a few hundred before relaxation starts.

#### 3. SPFA (Shortest Path Faster Algorithm) optimization

Janos iterates all edges every round. We maintain an `active_nodes` set: only nodes whose distance improved in the last round have their outgoing edges relaxed in the next round. This dramatically reduces simulation calls.

#### 4. No amount optimization needed

Janos needs Golden Section Search because the input amount is unknown for arbitrage. For routing, the input amount is given by the order, so we skip GSS entirely.

#### 5. Top-N re-simulation

After BF relaxation, we collect all layers where `token_out` is reachable, sort by relaxation amount descending, and re-simulate the top 3 candidates. This handles "re-simulation divergence": the relaxation-best path (evaluated against original pool states) may not be the true best after state-override re-simulation when pools are revisited.

#### 6. State overrides for revisited pools

During re-simulation, if the same pool appears twice in a path, we use the updated state from the first swap for the second. This is tracked via `native_state_overrides` (for native protocol sims like UniV2/V3) and `vm_state_override` (for EVM-based sims that share state).

### How It Fits into Fynd

The Bellman-Ford algorithm implements the `Algorithm` trait (`src/algorithm/mod.rs`). It runs as one or more **worker pools** alongside the existing `MostLiquid` algorithm. The `OrderManager` dispatches each order to all worker pools and picks the best result.

**Registration:** `src/worker_pool/registry.rs` maps the string `"bellman_ford"` to `BellmanFordAlgorithm`.

**Configuration:** `worker_pools.toml` defines pool configs with `algorithm = "bellman_ford"`, `num_workers`, `max_hops`, and `timeout`.

**Edge weights:** BF uses `()` as its edge weight type (no pre-computed data needed). The `EdgeWeightFromSimAndDerived` impl for `()` is trivial. This means BF doesn't need the `DepthAndPrice` data that MostLiquid requires, which is why BF finds routes that ML misses (when edge weight computation fails for ML).

---

## Function-by-Function Walkthrough

All code references are to `src/algorithm/bellman_ford.rs` in the PR branch.

### `BellmanFordAlgorithm` struct (lines 42-52)

Simple struct holding `max_hops` (maximum path length) and `timeout` (per-solve deadline). Created via `with_config()` from an `AlgorithmConfig`.

### `find_best_route()` (lines 63-379)

This is the `Algorithm` trait implementation. The main entry point called by solver workers.

**Phase 1: Setup (lines 70-153)**

1. Reject exact-out orders (BF only supports sell/exact-in)
2. Extract token prices from derived data (for gas cost conversion later)
3. Acquire market read lock
4. Look up `token_in` and `token_out` node indices in the graph
5. Run `extract_subgraph()` to BFS-prune the graph
6. Build `token_map` (NodeIndex -> Token) for all nodes in subgraph
7. Extract `market_subset` with simulation states for relevant components
8. Release market lock

**Phase 2: Initialize data structures (lines 163-185)**

- `distance[k][node]`: 2D array of BigUint, initialized to zero. `distance[0][token_in] = order.amount`.
- `predecessor[k][node]`: 2D array of Option<(NodeIndex, ComponentId)>, initialized to None.
- Build adjacency list from subgraph edges for O(1) neighbor lookup.
- Seed `active_nodes` with just `token_in`.

**Phase 3: SPFA relaxation (lines 191-266)** -- THE CORE

```
for each layer k from 0 to max_hops-1:
    check timeout
    if no active nodes, stop early
    for each active node u:
        if distance[k][u] is zero, skip
        for each outgoing edge (u, v, component_id):
            call get_amount_out(distance[k][u], token_u, token_v)
            if result > distance[k+1][v]:
                update distance[k+1][v]
                set predecessor[k+1][v] = (u, component_id)
                add v to next layer's active set
```

Each layer k represents paths with exactly k+1 edges. SPFA means we only process nodes that actually improved in the previous layer.

**Phase 4: Candidate selection (lines 268-291)**

Collect all layers where `distance[k][token_out] > 0`. Sort by amount descending. These are the candidate paths.

**Phase 5: Re-simulation (lines 294-345)**

For the top 3 candidates (by relaxation amount):
1. Reconstruct the path via `reconstruct_layered_path()`
2. Re-simulate via `simulate_path()` (handles state overrides for revisited pools)
3. Compute gas-adjusted `net_amount_out` via `compute_net_amount_out()`
4. Keep the best result

**Phase 6: Return (lines 347-378)**

Log solve time, hop count, amounts, route, and whether duplicate pools were used. Return the `RouteResult`.

### `extract_subgraph()` (lines 396-427)

Standard BFS from `token_in` up to `max_depth` hops. Returns `Vec<(NodeIndex, NodeIndex, ComponentId)>` of all edges reachable within the hop budget. This is the pruning step that keeps the graph manageable for simulation-heavy relaxation.

### `reconstruct_layered_path()` (lines 431-465)

Walks the `predecessor` array backward from `token_out` at the best layer:
- At layer k, look up `predecessor[k][current]` to get `(prev_node, component_id)`
- Record the edge `(prev_node, current, component_id)`
- Move to `prev_node`, decrement layer
- Continue until layer 0 (should be at `token_in`)
- Reverse the collected edges to get forward order

Returns `Vec<(NodeIndex, NodeIndex, ComponentId)>`.

### `simulate_path()` (lines 472-557)

Re-simulates the reconstructed path with proper state tracking. For each edge in the path:

1. Look up tokens and simulation state from market
2. Check if the pool was already visited (via `native_state_overrides` or `vm_state_override`)
3. If revisited, use the updated state; otherwise use the original state
4. Call `get_amount_out()` and record the result as a `Swap`
5. Store the new state for potential future revisits

Returns `(Route, BigUint)` where `Route` contains the vector of `Swap` structs and the BigUint is the final output amount.

### `compute_net_amount_out()` (lines 560-593)

Converts gas cost to output-token terms and subtracts it from gross output:

1. Sum total gas across all swaps in the route
2. Multiply by the effective gas price (from market data) to get gas cost in wei
3. Convert wei to output token using token price ratios (from derived data)
4. Subtract: `net = amount_out - gas_cost_in_output_token`

This is how we make routes with different gas costs comparable.

---

## Key Design Decisions

| Decision | Rationale |
|---|---|
| Layered `distance[hop][node]` | Allows paths of different lengths to the same node. Originally added to enable hub-token revisits; being revisited (see modification plan). |
| `()` edge weights | BF simulates during relaxation, so it doesn't need pre-computed spot prices or pool depths. This also gives BF better coverage than ML (finds routes where ML can't compute edge weights). |
| BFS subgraph extraction | Bounds the number of simulation calls. Without it, BF would call `get_amount_out()` on thousands of irrelevant edges each layer. |
| SPFA active-node set | Only re-relaxes edges from nodes that improved. Reduces simulation calls from O(V * max_hops) to O(reachable edges). |
| Top-3 re-simulation | Relaxation uses original pool states, but sequential execution changes states. Checking multiple candidates catches cases where the relaxation-best isn't the execution-best. |
| 5-hop max | Sweet spot between coverage and cost. 79% of BF's winning routes use 5 hops. Higher counts increase simulation calls significantly. |
| 200ms timeout | Matches production latency requirements. BF p50 is 12ms at 5 hops; timeout is a safety net. |

---

## Planned Modification: Forbid Token and Pool Revisits

### Rationale

The current implementation allows paths that revisit tokens (e.g., WETH->USDC->WETH->DAI) and pools. This is wrong for a router because:

1. **Token revisit implies arbitrage.** A path like A->B->A means the B->A leg is more profitable than staying at A, which means there's an arbitrage opportunity on the A/B pair. Arbitrageurs will take that opportunity (they have specialized infrastructure and are willing to pay high gas), so the pool state will shift before our transaction executes, invalidating the route.

2. **Pool revisit has no legitimate use case in routing.** Using the same pool twice adds complexity (state-override tracking) and relies on the pool being in a specific intermediate state during execution. Each pool should be visited at most once.

If both constraints are enforced:
- The re-simulation state override machinery becomes unnecessary (each pool visited once, original state is always correct)
- Re-simulation divergence largely disappears (relaxation amounts match execution amounts)
- The algorithm becomes simpler and easier to reason about

### Changes

#### 1. Add predecessor-chain checks during relaxation

Add two helper functions that walk the predecessor chain from a node at layer k back to the source:

**`path_contains_token(u, k, target_token, predecessor, graph) -> bool`**: Returns true if any node in the path has the same token address as the target. O(max_hops) per call.

**`path_contains_pool(u, k, target_pool, predecessor) -> bool`**: Returns true if any edge in the path uses the same component_id. O(max_hops) per call.

In the relaxation loop, before updating `distance[k+1][v]`:

```rust
// Skip if destination token already visited in this path
if path_contains_token(u, k, &graph[v], &predecessor, graph) {
    continue;
}
// Skip if this pool already used in this path
if path_contains_pool(u, k, component_id, &predecessor) {
    continue;
}
```

#### 2. Simplify `simulate_path()`

Remove `native_state_overrides`, `vm_state_override`, the `is_vm` check, and the state override selection/storage logic. The function becomes a straightforward forward simulation where each pool is visited once with its original state.

#### 3. Reduce top-N re-simulation from 3 to 1

Without pool revisits, relaxation amounts match re-simulation amounts. The ranking from relaxation is reliable. Change `let top_n = candidates.len().min(3)` to `let top_n = candidates.len().min(1)`.

(Alternatively keep top_n=3 as a safety margin; the cost is minimal.)

#### 4. Update tests

**Remove:**
- `test_source_token_may_be_revisited_for_better_output` (line 851)
- `test_hub_token_revisit_allowed` (line 888)
- `test_state_overrides_for_revisited_pools` (line 947)

**Add:**
- `test_token_revisit_blocked`: Verify the algorithm doesn't revisit tokens even when a hub-revisit path would give a higher output.
- `test_pool_revisit_blocked`: Verify the algorithm doesn't use the same pool twice.
- `test_no_route_when_only_cycle_path_exists`: Verify `NoPath` when the only path goes through a token twice.

#### 5. Keep layered distance structure

The layered `distance[hop][node]` still works correctly with the added constraints. Flattening to `distance[node]` would be a bigger refactor with more risk for no meaningful gain.

### Impact Assessment

**Performance:** Predecessor-chain walk adds O(max_hops) per edge, negligible vs. `get_amount_out`. Fewer valid paths may reduce total simulation calls.

**Solution quality:** Some 5-hop hub-revisit routes will be blocked. These routes exploit price inconsistencies that arbitrageurs would take first, so they wouldn't execute successfully in practice. Re-benchmark with 10K trades to quantify impact.

**Code complexity:** Net reduction (~30 lines removed from simulate_path, ~30 lines added for helpers).

### Benchmark Results (Forbid-Revisits vs Original BF)

Tested with 500 real Ethereum DEX trades (from Dune Analytics, Feb 2026) against live Tycho infrastructure. Both solvers ran BF-only (no `most_liquid` worker pools) for isolated comparison.

#### Head-to-head (381 contested trades, 119 both failed)

| | Count | % |
|---|---:|---:|
| **NEW wins** | **139** | **36.5%** |
| Ties | 234 | 61.4% |
| OLD wins | 8 | 2.1% |

#### Solve times (BF-only, ms)

| Version | Mean | Median | P95 |
|---|---:|---:|---:|
| OLD (original) | 17 | 14 | 31 |
| **NEW (forbid-revisits)** | **10** | **8** | **18** |

The new code is **42% faster** due to the revisit-check pruning skipping unnecessary simulations.

#### When NEW wins (139 trades)

Mean improvement: 0.20%, median: 0.20%, P95: 0.56%, max: 1.03%. The cleaner search space (no revisit dead ends) leads to better convergence on non-revisit routes.

#### When OLD wins (8 trades)

All 8 OLD wins involve **token revisits** (87.5% token-revisit, 0% pool-revisit). Two patterns:

1. **Source-token-revisit arbitrage loops (5 trades, ~14.3% improvement):** A single token pair where OLD finds a 5-hop route that exits the source token, loops through major tokens (WBTC, WETH, USDT), returns to the source, then takes a different pool to the destination. NEW falls back to a direct 1-hop route.

2. **Hub-token-revisit through alternative pools (3 trades, ~0.01% improvement):** WBTC-revisit paths where the old code exits WBTC, loops through cbBTC and WETH, returns to WBTC via a different pool, then continues. Improvement is negligible.

#### Conclusion

The forbid-revisits change is a **net positive**: 139 NEW wins vs 8 OLD wins (17:1 ratio), 42% faster solve times. The only meaningful regressions are source-token-revisit arbitrage loops that would likely be captured by MEV bots before execution. If recapturing these is desired, a targeted relaxation allowing source-token revisits could be explored.

---

## Benchmark Results (from PR #43)

### BF 3-hop vs ML 3-hop (equal depth, 7,580 contested trades)

| | Count | % |
|---|---:|---:|
| BF wins | 5,386 | 71.1% |
| ML wins | 74 | 1.0% |
| Ties | 2,120 | 28.0% |

### BF 5-hop vs ML 3-hop (7,556 contested trades)

| | Count | % |
|---|---:|---:|
| BF wins | 4,785 | 63.3% |
| ML wins | 794 | 10.5% |
| Ties | 1,977 | 26.2% |

### Solve times (isolated, p50)

| Algorithm | p50 | p95 |
|---|---:|---:|
| ML 2-hop | 1ms | 2ms |
| BF 2-hop | 4ms | 7ms |
| BF 3-hop | 6ms | 11ms |
| BF 5-hop | 12ms | 22ms |
| ML 3-hop | 43ms | 443ms |

BF is 7x faster than ML at equal depth.

---

## File Index

| File | Purpose |
|---|---|
| `src/algorithm/bellman_ford.rs` | Algorithm implementation (~580 lines + ~570 test lines) |
| `src/algorithm/mod.rs` | `Algorithm` trait definition, module registration |
| `src/graph/mod.rs` | `GraphManager` trait, `EdgeWeightFromSimAndDerived` (trivial `()` impl for BF) |
| `src/worker_pool/registry.rs` | Maps `"bellman_ford"` string to `BellmanFordAlgorithm` |
| `worker_pools.toml` | Worker pool configs (num_workers, max_hops, timeout) |
| `blacklist.toml` | AMPL/WETH pools blacklisted (rebasing token breaks simulation) |

## External References

| Resource | Location |
|---|---|
| Janos's paper (LaTeX) | `tycho-searcher/doc/searcher_documentation.tex` |
| Janos's paper (Overleaf) | https://www.overleaf.com/read/ksqhzzmndmqh#9e8003 |
| Janos's code | https://github.com/jtapolcai/tycho-searcher |
| Fynd PR #43 | https://github.com/propeller-heads/fynd/pull/43 |
| Collaboration agreement | DocuSign envelope 9E9977D0-4CF4-4C2E-9880-107A6B19C80B |
