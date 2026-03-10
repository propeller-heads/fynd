# Algorithms

Fynd uses a pluggable algorithm system. Each algorithm implements the `Algorithm` trait, which defines how to find the best swap route given a graph, market data, and an order. Multiple algorithms run in parallel via separate worker pools; the `OrderManager` picks the best result.

This document explains how the built-in algorithms work and how to think about adding new ones.

---

## Algorithm Trait

**Location:** `fynd-core/src/algorithm/mod.rs`

Every algorithm must implement:

```
trait Algorithm {
    type GraphType;           // The graph representation this algorithm prefers
    type GraphManager;        // How to build/update the graph from market events

    fn find_best_route(graph, market, derived, order) -> RouteResult;
    fn computation_requirements() -> ComputationRequirements;
    fn timeout() -> Duration;
}
```

Key design choices:

- **Generic graph type.** Algorithms choose their own graph representation (petgraph, adjacency list, custom). The framework provides `PetgraphStableDiGraphManager` as a reusable default, but algorithms are not required to use it.
- **Stateless.** Algorithms receive the graph as a parameter and do not hold mutable state. All state lives in the graph manager or shared market data.
- **Derived data requirements.** Algorithms declare which pre-computed data they need (spot prices, pool depths, token gas prices) and whether it must be fresh (same block) or can be stale. Workers use this to decide when solving is safe.

---

## Graph Representation

**Location:** `fynd-core/src/graph/`

The default graph is a `petgraph::StableDiGraph` where:

- **Nodes** are token addresses
- **Edges** are directed swaps through a specific pool (component)

Each edge carries an `EdgeData<D>` containing:
- `component_id`: which pool enables this swap
- `data: Option<D>`: algorithm-specific weight data (e.g., spot price + depth)

A single token pair can have multiple parallel edges (one per pool). A multi-token pool (e.g., Balancer 3-token pool) creates edges between all token pairs in both directions.

**Example:** A 3-token pool with tokens A, B, C creates 6 directed edges: A->B, B->A, A->C, C->A, B->C, C->B.

Edge weights are updated every block by the Derived Data Pipeline, which pre-computes spot prices and pool depths. This keeps edge weights current without recomputing during route finding.

---

## MostLiquid Algorithm

**Location:** `fynd-core/src/algorithm/most_liquid.rs`

The default algorithm. It finds routes using a score-then-simulate approach: enumerate candidate paths cheaply, rank them by an estimated quality score, then simulate only the most promising ones with full protocol simulation.

### How It Works

```
find_best_route(graph, market, order):
    1. BFS path enumeration    (all paths, min_hops..max_hops)
    2. Score and sort           (spot_price * min_depth, descending)
    3. Extract market subset    (release global lock early)
    4. Simulate top paths       (ProtocolSim, state overrides, gas adjustment)
    5. Return best result       (highest net_amount_out)
```

#### Step 1: Path Enumeration (BFS)

Finds all paths from `token_in` to `token_out` using breadth-first search on the directed graph. BFS naturally produces shorter paths first. The search explores all outgoing edges at each node, including parallel edges (multiple pools connecting the same token pair).

**Parameters:**
- `min_hops` (default: 1): minimum path length. Set to 2 to skip direct swaps and force multi-hop.
- `max_hops` (default: 3): maximum path length. The BFS stops expanding paths that reach this depth.

**Note:** The BFS does *not* prevent revisiting tokens, so cyclic paths are possible. This is intentional: in DeFi, routing through the same token twice via different pools can sometimes yield better rates. The combinatorial explosion is managed by the hop limit.

#### Step 2: Scoring

Each path is scored with:

```
score = (product of spot_prices along path) * min(depths along path)
```

Where:
- **spot_price** is the exchange rate at each hop (fee-inclusive, pre-computed by the Derived Data Pipeline)
- **depth** is the maximum input amount the pool can accept before hitting a configured slippage threshold (e.g., 10% price impact), measured in token units and pre-computed by the `PoolDepthComputation` via binary search on `get_amount_out`

The score captures two things:
1. **Expected output rate** (product of spot prices): a path through three pools with rates 0.98, 1.01, 0.99 yields ~0.98 output per input.
2. **Liquidity bottleneck** (minimum depth): a path is only as liquid as its weakest hop. Multiplying by the minimum depth penalizes paths with shallow intermediary pools.

Paths that cannot be scored (missing edge weights) are dropped.

If `max_routes` is configured, only the top N scored paths proceed to simulation.

#### Step 3: Market Subset Extraction

Before simulation, the algorithm extracts the component states it needs into a local `SharedMarketData` subset and releases the global read lock. This minimizes lock contention: the global lock is held only for the subset extraction, not during the CPU-intensive simulation phase.

#### Step 4: Simulation

Paths are simulated in score order (best first) using `ProtocolSim::get_amount_out`. Each hop in the path is simulated sequentially, feeding the output of one swap as the input to the next.

**State overrides:** When a path routes through the same pool twice (or through multiple VM-based pools sharing EVM state), the simulation uses state overrides to reflect intermediate state changes. Without this, the second swap through the same pool would simulate against stale state and produce incorrect results. Native protocol states are tracked per-component; VM states share a single override.

**Gas-adjusted ranking:** After simulation, each path's output is adjusted for gas cost:

```
net_amount_out = amount_out - gas_cost_in_output_token
```

Gas cost is converted to output token terms using pre-computed token-to-gas-token price ratios from the Derived Data Pipeline. If token prices are unavailable (derived data not yet computed), the raw output is used.

**Timeout:** Simulation stops when the per-algorithm timeout expires. Because paths are simulated in score order, the best candidates are evaluated first. A timeout means lower-scored paths were skipped, not that the best path was missed.

#### Step 5: Result Selection

The path with the highest `net_amount_out` (after gas) wins. If no path produces a positive result, the algorithm returns `InsufficientLiquidity`. If the timeout fired before any path was simulated, it returns `Timeout`.

### Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `min_hops` | 1 | Minimum path length |
| `max_hops` | 3 | Maximum path length |
| `timeout` | 500ms | Per-solve timeout |
| `max_routes` | None | Cap on paths to simulate |

### Derived Data Requirements

- `token_prices` (stale OK): Used for gas cost conversion. Not blocking; if unavailable, gas cost is not deducted.
- Spot prices and pool depths are consumed via edge weights, not through the derived data API directly.

### Strengths and Limitations

**Strengths:**
- Simple, predictable behavior. Easy to reason about and debug.
- Exhaustive within the hop budget: finds all paths up to `max_hops`.
- Score-based pruning ensures the best candidates are simulated first.

**Limitations:**
- Combinatorial explosion at higher hop counts. At `max_hops=3`, the path count is just manageable. At 4+, it grows too large to solve all in a few seconds.
- Scoring is an approximation. The spot_price * min_depth heuristic does not account for slippage from the actual trade size. A path with high depth but poor price-impact characteristics *for this specific swap amount* may score well but simulate poorly. Similarly in the inverse: A swap might not need as much depth and do fine on shallow pools.
- No negative-cycle detection. Cannot exploit arbitrage loops to improve output. However, since arbitrage loops are usually taken by bots, avoiding them makes it more likely that your trade settles (you don't compete to access the pool state a bot wants in the same block).

---

## Bellman-Ford Algorithm (In Development)

**Status:** PR [#43](https://github.com/propeller-heads/fynd/pull/43), not yet merged.

A simulation-driven Bellman-Ford algorithm that runs actual `get_amount_out()` simulations during edge relaxation instead of using heuristic scores. This finds better paths because it accounts for actual slippage, fees, and pool mechanics at the given trade size. It enables efficient exploration of longer paths (5+ hops) that the MostLiquid BFS approach cannot reach within timeout.

**This is not arbitrage detection.** The algorithm finds better A-to-B routes through intermediary tokens (e.g., WETH -> USDT -> DAI -> USDC may beat WETH -> USDC direct because the intermediary pools have better liquidity for those pairs at this trade size). It does not seek or exploit circular arbitrage loops.

### Core Idea

Classical Bellman-Ford finds shortest paths by iteratively relaxing edges. This algorithm adapts the structure but replaces additive edge weights with **actual swap simulations**:

- `distance[k][node]` stores the **real token amount** reachable at that node after exactly k hops, computed via `get_amount_out()`.
- Relaxation compares amounts at the same node: `if simulated_amount_out > distance[k+1][v]`, update. Since both values are denominated in token v, they are directly comparable.
- The final comparison only looks at `distance[k][token_out]` across layers, which are all in the output token.

This means the algorithm never needs to convert between token denominations during relaxation. Comparability is inherent: you only ever compare two amounts of the same token.

### How It Works

```
find_best_route(graph, market, order):
    1. Build subgraph           (BFS neighborhood from token_in, up to max_hops)
    2. Layered relaxation       (simulate swaps, SPFA-optimized, up to max_hops layers)
    3. Top-N re-simulation      (re-simulate best hop counts with state overrides)
    4. Return best result       (highest net_amount_out after re-sim)
```

#### Subgraph Construction

Rather than running relaxation on the full graph (thousands of nodes), the algorithm first extracts a subgraph via BFS from `token_in` up to `max_hops` depth. This keeps the working set small while including all potentially useful intermediary tokens.

#### Layered Relaxation with SPFA

The core loop runs `max_hops` layers. At each layer k, for every active node u, the algorithm simulates `get_amount_out(distance[k][u], token_u, token_v)` on each outgoing edge. If the result exceeds `distance[k+1][v]`, it updates the distance and records the predecessor.

**Key difference from MostLiquid:** This uses real pool simulation during the search, not just spot prices. The trade amount propagates through the graph, so slippage and pool-specific mechanics are captured at each hop.

**SPFA optimization:** Instead of scanning all nodes per layer, only nodes whose distances changed in the previous layer are processed. At layer 0, only `token_in` is active. This reduces simulation calls from O(V * max_hops) to O(reachable edges) and provides early termination when no distances change.

**Layer independence:** `distance[k][node]` tracks the best amount using *exactly* k edges. This means the destination may be reachable at multiple hop counts (e.g., both 3-hop and 5-hop routes reach `token_out`), and the best hop count is not necessarily the deepest.

#### Top-N Re-simulation

After relaxation, the algorithm collects every layer where `token_out` was reached and sorts them by the relaxation amount (descending). It then **re-simulates** the top 3 hop counts using the full `simulate_path` function with proper state overrides.

"Top 3" here means "the 3 best hop counts", not "3 out of thousands of paths". Each layer already represents the single best path of that length found during relaxation. For example, with max_hops=5 the candidates might be:

| Layer (hop count) | Relaxation amount |
|---|---|
| 3 | 3250 USDC |
| 4 | 3240 USDC |
| 2 | 3200 USDC |
| 5 | 3180 USDC |

The algorithm re-simulates layers 3, 4, and 2. Why not just pick the relaxation winner? Because the relaxation simulates each edge against the *original* pool state, not accounting for state changes from earlier hops in the same path. A path that looks best during relaxation may produce less after full sequential simulation with state overrides. Re-simulating the top 3 catches cases where the second or third-best relaxation candidate actually produces the highest output.

### Benchmark Results

From the 10K trade benchmark (max_hops=5):

| Metric | MostLiquid | Bellman-Ford |
|--------|------------|--------------|
| Win rate | 2.3% | 97.7% |
| p50 latency | - | 15ms |
| p95 latency | - | 37ms |

The optimal `max_hops` is 5. Going deeper (6-7) adds latency without improving win rate. Going shallower (3-4) significantly reduces the win rate because the algorithm cannot explore enough intermediary paths.

### Strengths and Limitations

**Strengths:**
- Simulation-driven: captures real slippage and pool mechanics during the search, not just spot prices.
- Scales to 5+ hops efficiently. SPFA optimization keeps per-layer work proportional to active nodes, not total nodes.
- Finds paths that BFS enumeration misses at higher hop counts.
- Top-N re-simulation mitigates the gap between per-edge simulation (original state) and full-path simulation (with state overrides).

**Limitations:**
- Relaxation simulates each edge against the original pool state, not accounting for state changes from earlier hops. The top-N re-simulation mitigates but doesn't eliminate this divergence.
- More simulation calls per solve than MostLiquid. For simple 1-2 hop swaps, MostLiquid is more direct.
- Subgraph construction adds fixed overhead per solve.

### Why Bellman-Ford and not Dijkstra/A*?

The edge "weight" here is not a fixed number you can precompute. The output of a pool depends on how much token you bring in, which depends on the entire path taken so far. This makes the problem inherently path-dependent: Dijkstra and A* require fixed, additive edge weights known upfront.

BF's layered structure is the natural fit because `distance[k][node]` = "best amount at this node after exactly k hops". Each layer extends the previous by simulating one more hop. This is closer to dynamic programming than classical shortest-path.

You could use Dijkstra with log(spot_price) as fixed weights (they're positive and additive in log-space), but then you lose the key advantage: simulation-driven search that accounts for actual slippage at the trade size. That's exactly what MostLiquid already does with its scoring heuristic.

---

## Multi-Algorithm Competition

In production, multiple worker pools run different algorithms (or the same algorithm with different parameters) in parallel. For example:

```toml
[pools.fast_2hop]
algorithm = "most_liquid"
num_workers = 5
max_hops = 2
timeout_ms = 100

[pools.deep_3hop]
algorithm = "most_liquid"
num_workers = 3
max_hops = 3
timeout_ms = 5000
```

Each pool produces its own solution independently. The `OrderManager` collects all solutions within the request timeout and returns the one with the highest `amount_out_net_gas`. This means:

- Fast, shallow pools provide a baseline quickly
- Slower, deeper pools can improve on it if time allows
- Different algorithms can compete on the same request

The `order_manager_best_solution_pool` Prometheus metric tracks which pool wins most often.

---

## Adding a New Algorithm

1. Create `fynd-core/src/algorithm/your_algo.rs`
2. Implement the `Algorithm` trait, choosing your graph type
3. Register it in `fynd-core/src/worker_pool/registry.rs`
4. Add a `[pools.your_pool]` entry in `worker_pools.toml`

Your algorithm can use the provided `PetgraphStableDiGraphManager` or bring its own graph representation. The `simulate_path` helper from `MostLiquidAlgorithm` is `pub(crate)` and can be reused for the simulation phase.
