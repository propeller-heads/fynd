---
icon: arrows-split-up-and-left
---

# Path Frank-Wolfe

The Path Frank-Wolfe algorithm extends Bellman-Ford with **split routing**: instead of sending the entire input through a single path, it discovers multiple candidate paths and optimally distributes the input across them. For large trades where price impact is a binding constraint, splitting across parallel paths can produce meaningfully better output than any single route.

## Overview

The algorithm runs in three stages:

1. **Initial route**: Run Bellman-Ford at full input to find the best single path
2. **Frank-Wolfe loop**: Iteratively discover additional paths and shift flow toward them
3. **Final comparison**: Return the split route if it beats the single-path result net of gas

The key insight is that price impact is the enemy. A constant-product pool that returns a good rate on 100 USDC returns a progressively worse rate as you push more through it. By splitting the same total input across multiple pools, each pool sees a smaller amount and operates at a better rate.

Two properties hold throughout the optimization:

* **Honest shared-pool accounting**: whenever a set of paths is evaluated, they are simulated sequentially against a shared post-swap state map. A path that reuses a pool another path already touched sees the depleted reserves, so a split can never double-count the same liquidity. Reported route amounts therefore correspond to what the route delivers when executed.
* **Never lose the single path**: the single-path route is a floor. Every candidate split must beat it net of gas, and any failure inside the split search (missing derived data, invalid split route) falls back to the single-path result instead of failing the solve.

## When does splitting help?

The algorithm computes a **price impact estimate** before attempting any split. If price impact is negligible relative to gas costs, splitting can't pay for itself — extra swaps cost gas, and the marginal gain from reducing impact is too small. The algorithm skips the Frank-Wolfe loop and returns the single-path result directly.

Splitting is most valuable when:

* **Large trades relative to pool depth**: A trade of 10% of a pool's reserves has meaningful price impact; 0.01% does not.
* **Multiple parallel pools exist**: Splitting only helps when there are alternative paths to absorb the extra flow.
* **Multi-hop paths share entry pools**: Paths that start differently but converge on the same intermediate token can be split at the entry, reducing impact on all shared segments.

## Stage 1: Initial route

Bellman-Ford (BF) runs at the full order amount to find the best single-path route. This serves two purposes: it gives a quality baseline for the final comparison, and it provides an initial allocation — 100% of flow on one path — from which the Frank-Wolfe loop starts.

## Stage 2: Frank-Wolfe loop

### Probe amount

Before each iteration, the algorithm computes a **probe amount**: the minimum trade size where an additional path would pay for its gas cost. This is `gas_cost / price_impact`. If price impact has fallen enough (because prior splits already reduced it), the probe exceeds the configured `max_probe` cap and the loop stops.

### Finding a candidate path

To find the next candidate path, the algorithm constructs a **post-swap market state** that reflects the current allocation:

1. Simulate all current path allocations through their respective pools
2. Store the post-swap pool states as overrides
3. Zero out gas costs for pools already committed in the current solution (they are already executed once on-chain; their gas is already priced in)

Bellman-Ford then runs on this degraded state at `probe_amount`. It finds the best path *given that prior allocations have already moved the committed pools*. If a previously-committed pool has absorbed so much flow that its rate degraded, BF naturally routes around it.

### Duplicate detection

If BF returns the same path that already exists in the allocation (same sequence of `(pool, token_in, token_out)` triples), exploration is exhausted and the loop stops. Paths that share only a prefix but diverge at a later hop are not duplicates — shared prefixes are handled by the route builder, not by splitting.

### Line search

Once a candidate path is found, the algorithm uses **golden-section search** to find the optimal `step_size ∈ [0, 1]`: the fraction of total flow to shift from the existing allocation to the new candidate. At each probe point, the algorithm re-simulates all paths **sequentially against a shared post-swap state map** — later paths see the reserves earlier paths consumed — and scores the result by **net output**: gross output minus the trial's total gas converted into output-token terms. The step size that maximises net output is selected.

This line search is the Frank-Wolfe "descent direction" computation. It runs `line_search_evals` function evaluations (default: 12), which is enough for ~4-5 decimal digits of precision.

### Gas-aware activation

A path's gas cost is constant once it carries any flow, so the line search alone cannot reject a candidate whose extra output never covers its extra gas — it just finds the least-bad step. The activation check handles this: the net output at the chosen step is compared against the net output at step 0, where the candidate carries no flow and no gas. The candidate is only activated when the split genuinely nets more than the current allocation. This mirrors the water-fill activation rule from the split-routing research: a path must pay for its own gas before it gets flow.

### Applying the step

The chosen step size is applied:

* All existing path fractions are scaled by `(1 - step_size)`
* The candidate is added at `step_size`
* Paths whose fraction falls below `min_split` are dropped and the remaining fractions are renormalized
* All paths are re-simulated at their new allocations, sequentially against a shared post-swap state map, to refresh amounts and per-hop outputs. Paths that share a pool (including a pool reused by a later hop of the same path) are priced on depleted reserves, so the recorded per-hop amounts match executable reality

The loop then repeats with the updated allocation as the new starting point.

## Stage 3: Final comparison

After the loop completes (due to `max_paths`, timeout, price impact exit, or duplicate detection), the algorithm builds the full split route, validates it, and compares it against the initial single-path result by **net amount out** (gross output minus gas cost). The better result is returned.

If only one path survived (because all splits were too small), the initial single-path result is returned directly without building a split route.

The entire split search runs behind a fallback boundary: if any step of it errors — a pool missing derived data, a malformed split route, a failed validation — the algorithm logs the failure and returns the single-path result. A solvable order is never failed by the split optimization.

## Shared pools and route assembly

Paths can share intermediate pools. For example:

```
Path 1: WETH → [P1] → USDC → [P2] → DAI
Path 2: WETH → [P1] → USDC → [P3] → DAI
```

Both paths use pool P1. On-chain, P1 is called once with the combined WETH input; P2 and P3 each receive a fraction of the resulting USDC. The route builder:

1. **Merges shared hops**: identifies hops with the same `(pool, token_in, token_out)` across all paths and combines them into a single swap
2. **Assigns split fractions**: sorts within each branch by flow fraction (largest first); the last swap in each branch receives `split = 0.0` (the TychoRouter "use remaining balance" convention)
3. **Topological ordering** (Kahn's algorithm): swaps are emitted only after all upstream swaps producing their input token are done. This is necessary when paths of different lengths converge on the same intermediate token — the shared downstream pool must wait for all inflows to complete before its swap is emitted

Gas for shared pools is counted once, not once per path.

## Design tradeoffs

### Versus Bellman-Ford

Bellman-Ford finds the best **single path**. PathFrankWolfe wraps BF and adds a split optimization layer on top. For trades with negligible price impact, PFW produces the same result as BF. For large trades with meaningful impact, PFW can produce better net output by spreading flow across multiple pools.

The cost is additional simulation work: each Frank-Wolfe iteration runs one BF solve plus `line_search_evals` evaluations of the total-output function. With `max_paths = 4` and `line_search_evals = 12`, the worst case is roughly 3 BF solves and ~36 path simulations on top of the initial BF run.

### Single-path fallback

The algorithm always compares the split result against the single-path baseline before returning. If the split route doesn't beat the single path net of gas (this can happen when the gas overhead of extra swaps outweighs the reduced impact), the single-path result wins. Errors during the split search degrade to the single-path result the same way, so split optimization can only add output, never cost coverage.

### Sequential evaluation versus independent evaluation

An earlier version of the evaluator simulated every path from the original pool states. For pool-disjoint splits the two are identical, but for splits that reuse a pool, independent simulation counts the same liquidity twice and overstates output — the route then under-delivers when executed. Sequential evaluation prices each path on the state the previous paths left behind, which matches how the merged route executes on-chain (a shared hop becomes one combined swap). The optimizer's objective, the recorded per-hop amounts, and the reported net all use the sequential semantics.

### Timeout safety

The Frank-Wolfe loop checks elapsed time at the start of each iteration. If the timeout is exceeded, the loop stops and the algorithm proceeds with however many paths it has found. The result is always valid — just potentially less optimal than a full-budget run.

## Suggested configuration

```toml
[pools.path_frank_wolfe_3_hops]
algorithm = "path_frank_wolfe"
num_workers = 3
task_queue_capacity = 1000
max_hops = 3
timeout_ms = 1000
```

* `timeout_ms = 1000` is roughly double a single-path pool's budget, because each Frank-Wolfe
  iteration runs a full Bellman-Ford solve plus a line search on top of the initial solve. On timeout
  the loop stops and the algorithm returns the paths it has, so a tight budget silently reduces the
  number of splits it can find.
* `max_hops = 3` bounds the inner Bellman-Ford solve, which runs once per iteration — hop cost is
  multiplied here, not paid once.
* Run this pool **alongside** a Bellman-Ford or Most Liquid pool. PFW does more simulation work per
  request, and the WorkerPoolRouter returns whichever pool answers best within the timeout, so the
  cheap pool still covers requests where PFW is slow on VM-heavy routes.

{% hint style="warning" %}
**Set connector tokens.** With `connector_tokens` unset, routes can pass through illiquid long-tail
intermediates, which raises reversion risk — and a split route multiplies that exposure across
every path it activates. Restrict intermediate hops to a small trusted set — generate one for your
chain with `fynd derive-connector-tokens --chain Ethereum --top-n 10 --output toml` and paste it
into the pool. See [Connector tokens](../guides/server-configuration.md#connector-tokens).
{% endhint %}

The PFW-specific tuning parameters are not currently exposed in `worker_pools.toml`; they use defaults:

| Parameter | Default | Description |
| --- | --- | --- |
| `max_paths` | 4 | Maximum number of distinct paths to split across |
| `max_probe` | 25% | Probe amount cap as a fraction of total input |
| `min_split` | 5% | Minimum flow fraction for any path; smaller shares are dropped |
| `line_search_evals` | 12 | Golden-section evaluations per step size search |

## Source reference

| File | Purpose |
| --- | --- |
| `fynd-core/src/algorithm/path_frank_wolfe.rs` | Algorithm implementation and Frank-Wolfe loop |
| `fynd-core/src/algorithm/split_primitives.rs` | Shared primitives: path simulation, route assembly, line search |
| `fynd-core/src/algorithm/bellman_ford.rs` | Inner BF solver used for path discovery |
| `fynd-core/src/algorithm/mod.rs` | `Algorithm` trait definition |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"path_frank_wolfe"` to `PathFrankWolfeAlgorithm` |
| `worker_pools.toml` | Worker pool configuration (add a `path_frank_wolfe` pool to enable) |
