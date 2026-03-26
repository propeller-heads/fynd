# How the Bellman-Ford Router Works

This document explains how Fynd's Bellman-Ford routing algorithm finds the best swap route through a network of decentralized exchange pools.

## Table of Contents

1. [The Algorithm](#1-the-algorithm)
2. [Forbid Revisits](#2-forbid-revisits)
3. [Subgraph Extraction](#3-subgraph-extraction)
4. [The Complete Algorithm](#4-the-complete-algorithm)
5. [A Worked Example](#5-a-worked-example)
6. [Design Tradeoffs](#6-design-tradeoffs)
7. [FAQ](#7-faq)

---

## 1. The Algorithm

The algorithm maintains one number per token: the best amount of that token reachable from the source via any path found so far. It improves these numbers by repeatedly simulating swaps through pools, keeping only improvements. This process is called **relaxation**.

### What the algorithm tracks

Two arrays, both indexed by token:

- **`amount[token]`**: the best amount of this token reachable from the source. Initialized to `order_amount` for the source token, 0 for everything else.
- **`predecessor[token]`**: which token and pool led to this best amount. Used to reconstruct the path at the end.

For a trade starting from WETH, partway through execution the array might look like:

```
amount[WETH]  = 1000000000000000000   (1 ETH, the input)
amount[USDC]  = 2015000000            (best USDC reachable: ~2015)
amount[DAI]   = 2008000000000000000   (best DAI reachable: ~2008)
amount[WBTC]  = 6120000              (best WBTC reachable: ~0.0612)
amount[LINK]  = 0                     (no path found yet)
```

Each token competes only with itself. The algorithm doesn't compare a path to USDC against a path to DAI. It compares paths that end at the same token, keeping only the best one.

A consequence: a single run finds the best route from the source to *every* reachable token, not just the destination. If multiple orders share the same source token and amount in the same block, you could serve them all from a single run. The current code doesn't exploit this (each order triggers a fresh run), but the structure supports it.

### Relaxation

For each edge (pool connecting two tokens), relaxation calls `get_amount_out()`, which runs the actual pool math for the exact input amount at the current pool state:

```
new_amount = pool.get_amount_out(amount[u], token_u, token_v)
if new_amount > amount[v]:
    amount[v] = new_amount
    predecessor[v] = (u, pool)
```

This simulation accounts for the pool's reserves, its fee structure, and price impact at the exact trade size. A Uniswap V2 pool runs the constant product formula. A Uniswap V3 pool steps through price ticks. The simulation returns the precise output for that specific input.

This is the core insight: instead of precomputing approximate edge weights and searching over them, we simulate the actual swap at every step. The search and the evaluation happen together.

When the algorithm relaxes an edge from USDC to DAI, it asks: "starting from the best USDC amount I've found (2015), swapping through this USDC/DAI pool, do I get more DAI than the 2008 I already have?" If yes, update. If no, move on.

### The active set

Relaxation is expensive (each call runs real AMM math), so we avoid wasting simulation calls on tokens that haven't improved. The algorithm maintains an **active set**: the set of tokens whose amount improved in the previous round. Only outgoing edges from active tokens are relaxed.

```
amount[source] = order_amount
active = {source}

for round = 1 to max_hops:
    next_active = {}
    for each node u in active:
        for each outgoing edge (u, v) through pool P:
            out = P.get_amount_out(amount[u], token_u, token_v)
            if out > amount[v]:
                amount[v] = out
                predecessor[v] = (u, P)
                next_active.add(v)
    active = next_active
```

Round 1 starts with just the source. Its active set is the source's direct neighbors (tokens reachable in one hop). Round 2's active set is the neighbors that actually improved. The active set expands outward like a wavefront, but only through nodes where something changed.

If a token's amount didn't improve, its outgoing edges can't produce new results either: same input, same pool states, same outputs. Only tokens that received a higher amount can propagate improvements.

The full graph might have 2,400 tokens. At round 2, perhaps 50 are active. The algorithm processes 50 nodes' outgoing edges instead of 2,400. With 5 rounds, the savings compound: thousands of simulation calls are avoided.

### Gas-aware relaxation

The basic relaxation above compares gross output amounts. But a 5-hop route with slightly more gross output can lose to a 3-hop route after gas costs. To handle this, relaxation optionally compares **net** amounts: gross output minus cumulative gas cost, converted to the output token.

At each node, the algorithm tracks `cumul_gas[v]`: the total gas units along the best path to v. When comparing a candidate path against the current best, it computes:

```
net_candidate = gross_output - cumul_gas_candidate * gas_price * token_price[v]
net_existing  = amount[v]    - cumul_gas[v]         * gas_price * token_price[v]
if net_candidate > net_existing: update
```

`token_price[v]` converts gas cost (in wei) to the token at node v. The algorithm resolves this from derived data when available, or falls back to a cumulative spot price product along the path (multiplying the spot prices of each pool traversed). If neither gas price nor token prices are available, it falls back to gross comparison automatically.

This is configurable via the `gas_aware` setting in the algorithm config (defaults to true).

---

## 2. Forbid Revisits

The algorithm above has a problem. Consider this graph:

```
ETH -[pool A]-> USDC -[pool B]-> ETH -[pool C]-> DAI
```

If pool B gives a great rate for USDC-to-ETH, the algorithm might find that going ETH -> USDC -> ETH -> DAI gives more DAI than going ETH -> DAI directly. The route visits ETH twice, passing through a USDC/ETH roundtrip in the middle.

This route looks profitable on paper, but it will fail in practice. Here's why.

### Token revisit implies arbitrage

If the path goes ETH -> USDC -> ETH, the USDC -> ETH leg is getting more ETH back than you started with (otherwise the roundtrip would lose money and the algorithm wouldn't choose it). That means there's a price discrepancy between pool A and pool B for the ETH/USDC pair. This is an **arbitrage opportunity**.

Arbitrageurs monitor these discrepancies with specialized infrastructure. They will execute the ETH -> USDC -> ETH cycle themselves, pocketing the profit. Their transaction will adjust the pool reserves, eliminating the discrepancy. By the time our transaction executes, the pools will have moved, and the route will no longer produce the expected output.

Building a route that depends on an arbitrage opportunity is building on sand.

### The fix: check before relaxing

Before each relaxation, we walk the predecessor chain backward from the current node to the source and check two things:

1. **Token check**: does the destination token already appear in the path? If yes, skip this edge.
2. **Pool check**: has this pool already been used in the path? If yes, skip. For two-token pools this is redundant with the token check, but multi-token pools (e.g., Balancer weighted pools or Curve tri-pools) connect more than two tokens. Without the pool check, the algorithm could route through the same pool twice on different token pairs, which would produce incorrect results because the pool state changes after the first swap.

```
for each edge (u, v) through pool P:
    if v's token is already in the path to u: skip
    if P is already used in the path to u: skip
    // ... proceed with simulation and relaxation
```

Each check walks the predecessor chain, which has at most `max_hops` entries (typically 5). The cost is negligible compared to the simulation call that follows.

With these constraints, every path the algorithm considers visits each token at most once and uses each pool at most once. The route is a simple chain through distinct pools, which is exactly what we want for execution.

---

## 3. Subgraph Extraction

Before relaxation even starts, we prune the graph.

On Ethereum, the full token graph has ~2,400 tokens and ~10,000 directed edges. For a trade from ETH to USDC with a 3-hop budget, most of these are irrelevant. Tokens that are 4 hops away from ETH can't appear in any 3-hop route.

**BFS from the source**: starting from `token_in`, we run breadth-first search following outgoing edges, stopping at depth `max_hops`. Only edges encountered during this BFS are kept. Everything else is discarded.

The result is a subgraph of a few hundred edges, down from 10,000. All subsequent work (building the adjacency list, running relaxation, walking predecessor chains) operates on this smaller graph.

This complements the active set. BFS removes structurally unreachable nodes before relaxation. The active set skips nodes that are structurally reachable but haven't received tokens yet. Together, they keep the number of simulation calls affordable.

---

## 4. The Complete Algorithm

Here is the full sequence, end to end. Each step maps to a specific section of `fynd-core/src/algorithm/bellman_ford.rs`.

### Step 1: Setup

Validate that the order is a sell order (exact input amount, find best output). Look up the source and destination tokens in the graph. Acquire a read lock on the shared market data (pool states update every block), snapshot what we need, and release the lock. All subsequent steps are lock-free.

### Step 2: Subgraph extraction

BFS from the source token up to `max_hops` depth. Collect all reachable edges. Build a token map (node index to token metadata) and extract a market data subset containing only the relevant pools.

### Step 3: Initialize

Set `amount[source] = order_amount`. Build an adjacency list from the subgraph edges. Seed the active set with the source node.

### Step 4: Relaxation

For each round up to `max_hops`:
- Check the timeout. If exceeded, stop with whatever we have.
- If the active set is empty, stop early (no more improvements possible).
- For each active node u, for each outgoing edge (u, v) through pool P:
  - Check if v's token already appears in the path (predecessor walk). Skip if yes.
  - Check if pool P already appears in the path. Skip if yes.
  - Call `P.get_amount_out(amount[u], token_u, token_v)`.
  - If the result exceeds `amount[v]`, update and add v to the next active set.

### Step 5: Check destination

If `amount[destination]` is still zero, no path was found. Return an error.

### Step 6: Reconstruct path

Walk the predecessor array backward from the destination to the source. At each node, `predecessor[node]` tells us which node and pool led here. Collect these into a list of (from, to, pool) edges and reverse to get forward order.

### Step 7: Re-simulate

Run the reconstructed path forward, calling `get_amount_out()` at each hop with the actual running amount. This produces the authoritative output and the `Swap` structs needed for on-chain execution.

### Step 8: Gas adjustment

Compute the total gas cost of the route (sum of each swap's gas estimate), multiply by the current gas price, convert to the output token using price ratios, and subtract from the gross output. The result is `net_amount_out`: the output after accounting for execution cost.

Return the route and net amount.

---

## 5. A Worked Example

Let's trace the algorithm on a small graph.

**Trade**: sell 1,000 units of token A for token D.
**Max hops**: 3.

**Graph** (each pool has a simplified exchange rate for illustration):

```
         [pool1: 1 in -> 2 out]         [pool3: 1 in -> 3 out]
    A -----------------> B -----------------> D
    |                    |
    |   [pool2: 1 -> 5]  |   [pool4: 1 -> 0.5]
    +---------> C --------+
                |
                | [pool5: 1 -> 4]
                +-----------------------------> D
```

Edges (directed, with output per unit of input):
- pool1: A->B, 2 out per 1 in
- pool2: A->C, 5 out per 1 in
- pool3: B->D, 3 out per 1 in
- pool4: C->B, 0.5 out per 1 in
- pool5: C->D, 4 out per 1 in

### Step 2: Subgraph

BFS from A with depth 3 reaches all nodes and all edges. The full graph is the subgraph.

### Step 3: Initialize

```
amount = { A: 1000, B: 0, C: 0, D: 0 }
predecessor = { A: none, B: none, C: none, D: none }
active = { A }
```

### Step 4, Round 1

Process A's outgoing edges:

- **A -> B via pool1**: `get_amount_out(1000) = 2000`. `2000 > 0`, so update.
  `amount[B] = 2000`, `predecessor[B] = (A, pool1)`, add B to next_active.
- **A -> C via pool2**: `get_amount_out(1000) = 5000`. `5000 > 0`, so update.
  `amount[C] = 5000`, `predecessor[C] = (A, pool2)`, add C to next_active.

```
amount = { A: 1000, B: 2000, C: 5000, D: 0 }
active = { B, C }
```

### Step 4, Round 2

Process B's outgoing edges:

- **B -> D via pool3**: `get_amount_out(2000) = 6000`. `6000 > 0`, so update.
  `amount[D] = 6000`, `predecessor[D] = (B, pool3)`, add D to next_active.

Process C's outgoing edges:

- **C -> B via pool4**: Before simulating, check: is B's token already in the path to C? Path to C is: A -> C. B is not in it. Is pool4 already used? No. Proceed. `get_amount_out(5000) = 2500`. `2500 > 2000` (current B), so update.
  `amount[B] = 2500`, `predecessor[B] = (C, pool4)`, add B to next_active.
- **C -> D via pool5**: `get_amount_out(5000) = 20000`. `20000 > 6000`, so update.
  `amount[D] = 20000`, `predecessor[D] = (C, pool5)`, add D to next_active.

```
amount = { A: 1000, B: 2500, C: 5000, D: 20000 }
active = { B, D }
```

### Step 4, Round 3

Process B's outgoing edges (B was re-activated because its amount improved):

- **B -> D via pool3**: `get_amount_out(2500) = 7500`. `7500 < 20000`. No update.

Process D's outgoing edges: D has no outgoing edges in this graph.

```
amount = { A: 1000, B: 2500, C: 5000, D: 20000 }
active = {}  (nothing improved)
```

### Step 6: Reconstruct

Start at D. `predecessor[D] = (C, pool5)`. Move to C.
`predecessor[C] = (A, pool2)`. Move to A. That's the source. Done.

Path (reversed): **A -[pool2]-> C -[pool5]-> D**

### Step 7: Re-simulate

- A -> C via pool2: `get_amount_out(1000) = 5000`
- C -> D via pool5: `get_amount_out(5000) = 20000`

Final output: **20,000 units of D**.

Note that the algorithm also explored A -> B -> D (6,000) and A -> C -> B -> D (7,500) but found A -> C -> D (20,000) to be the best.

---

## 6. Design Tradeoffs

### Gas-aware vs. gross relaxation

With `gas_aware` enabled (the default), the algorithm compares net amounts during relaxation, steering path selection toward routes with better value after gas deduction. This requires token prices and a gas price from derived data; when either is missing, it falls back to gross comparison transparently.

The gas cost conversion uses two strategies: a direct lookup in the derived token-gas-price table (primary), or a cumulative spot price product along the path (fallback for tokens not in the price table). The fallback multiplies spot prices hop by hop, so it degrades in accuracy for long paths, but it extends coverage to tokens that lack a direct WETH price.

The improvement is most visible on routes where a cheap 2-hop path beats an expensive 4-hop path with marginally higher gross output.

### Forward-only BFS vs. bidirectional

Subgraph extraction uses forward BFS from the source, not bidirectional BFS (forward from source, backward from destination). Bidirectional BFS produces a tighter subgraph (edges must lie on a viable source-to-destination path), but forward-only is simpler and the active set mechanism already avoids processing nodes that don't lead anywhere useful.

---

## 7. FAQ

### Why not Dijkstra or A*?

Three properties of DEX pools break the assumptions these algorithms rely on:

1. **Edge weights are functions, not constants.** The output of a pool depends on how much you push through it (price impact). You cannot precompute weights once and reuse them.
2. **Weights are multiplicative, not additive.** Exchange rates multiply along a path; shortest-path algorithms add. (The log-transform trick exists but doesn't handle price impact.)
3. **The best route depends on trade size.** A pool with deep liquidity wins for big trades; a shallow pool wins for small ones. No single "best route" exists independently of the amount.

Our algorithm handles all three by simulating the actual swap at each step instead of operating on precomputed weights.

---

## Source Reference

| File | Purpose |
|---|---|
| `fynd-core/src/algorithm/bellman_ford.rs` | Algorithm implementation |
| `fynd-core/src/algorithm/mod.rs` | `Algorithm` trait definition |
| `fynd-core/src/graph/petgraph.rs` | Graph implementation (petgraph::StableDiGraph) |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"bellman_ford"` to `BellmanFordAlgorithm` |
| `worker_pools.toml` | Worker pool configuration |
