# Investigation: most_liquid 3-hop Performance Degradation with fluid_v1

## Summary

When `fluid_v1` is included in the protocol set, `most_liquid` 3-hop throughput becomes
non-monotonic and degrades significantly at high worker counts. Removing `fluid_v1` restores
clean monotonic scaling. This document records what is known, what is hypothesised, and what
instrumentation or experiments are needed to nail down the root cause.

## Benchmark Context

All runs: AWS `c7a.8xlarge` (32 vCPU, AMD EPYC), 10,000 requests, `fixed:48` concurrency, 30s warmup.

### most_liquid 3-hop — 7 protocols (no fluid_v1)

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) |
| ------: | -----------------: | -------------: | ----------: |
|       1 |              26.37 |           1801 |        2323 |
|       2 |              53.38 |            895 |        1038 |
|       4 |             105.35 |            453 |         531 |
|       8 |             195.24 |            241 |         313 |
|      12 |             290.05 |            158 |         224 |
|      16 |             362.16 |            124 |         191 |
|      20 |             409.17 |            106 |         185 |
|      24 |             450.39 |             93 |         188 |
|      28 |             461.64 |             88 |         192 |
|      32 |             449.14 |             88 |         224 |

Clean near-monotonic scaling. Peaks at 28 workers (462 req/s). P99 stays tightly bounded (≤224ms).

### most_liquid 3-hop — 8 protocols (with fluid_v1, post lock-PR)

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) |
| ------: | -----------------: | -------------: | ----------: |
|      12 |             211.99 |            213 |         338 |
|      16 |             221.04 |            201 |         334 |
|      20 |             342.76 |            120 |         238 |
|      24 |             323.45 |            113 |         289 |
|      28 |             400.13 |             94 |         238 |
|      32 |             274.16 |            123 |         337 |

Non-monotonic. Throughput dips at 16 and 32 workers with no consistent trend.

### most_liquid 3-hop — 8 protocols (with fluid_v1, pre lock-PR)

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) |
| ------: | -----------------: | -------------: | ----------: |
|      12 |             236.87 |            190 |         290 |
|      16 |             308.74 |            140 |         238 |
|      20 |             332.83 |            124 |         237 |
|      24 |             306.92 |            102 |         **794** |
|      28 |             310.44 |             93 |         **617** |
|      32 |             319.20 |             77 |         **694** |

Catastrophic P99 spikes at 24–32 workers. The lock-PR (snapshot-then-simulate) eliminated the
spikes but not the non-monotonic throughput.

## Architecture Background

### most_liquid edge weights

`most_liquid` uses pre-computed edge weights (`DepthAndPrice`) stored on every graph edge. On
every block, each worker calls `update_edge_weights_with_derived` which iterates ALL edges and
recomputes `spot_price × depth` from the derived data store. This is O(edges) synchronous work
per worker per block — it runs while holding the worker's tokio execution thread (no lock on
market data, but CPU-bound).

```rust
// fynd-core/src/algorithm/most_liquid.rs
impl EdgeWeightFromSimAndDerived for DepthAndPrice {
    fn from_sim_and_derived(..., derived: &DerivedData) -> Option<Self> {
        let spot_price = derived.spot_prices().get(key)?;
        let depth      = derived.pool_depths().get(key)?;   // ← expensive with many pools
        Some(Self { spot_price, depth })
    }
}
```

### bellman_ford edge weights

`bellman_ford` uses `()` edge weights. `update_edge_weights_with_derived` is a no-op. Its
per-block pause is negligible regardless of graph size.

### Block-update pause (confirmed via log analysis)

High-latency requests (>400ms) cluster in windows separated by ~12–15 seconds — exactly
Ethereum's block time. The pause occurs because:

1. A new block arrives → `TychoFeed` broadcasts `MarketEvent` to all workers
2. Each worker's select loop handles the event **before** processing new tasks (biased select,
   market events have priority over solve tasks)
3. `update_edge_weights_with_derived` runs synchronously inside the worker's tokio runtime
4. During this window all workers stop solving; the task queue drains to zero throughput
5. After the update, queued tasks flush simultaneously, creating a latency burst

With 8 protocols the graph is larger, making the edge update step proportionally more expensive.
The `PoolDepthComputation` logs reported `pool depths computed count=4338` (8 protocols), vs a
lower count without fluid_v1.

### Why fluid_v1 specifically worsens things

This is still hypothetical — see the investigation plan below. Leading candidates:

1. **Pool count**: fluid_v1 may contribute a disproportionately large number of pools, growing
   the edge count significantly and making `update_edge_weights_with_derived` much slower.
2. **Pool depth computation cost**: fluid_v1 pool simulations (for `PoolDepthComputation`) may
   be substantially more expensive than other protocols, making the block-update cycle irregular.
   If one block's depth computation takes 500ms+ while others take 100ms, the block-update pause
   becomes highly variable, producing non-monotonic throughput.
3. **Simulation state size (`clone_box`)**: `most_liquid`'s `find_best_route` calls
   `market.extract_subset()` which calls `clone_box()` on every matching simulation state. If
   fluid_v1 states have large EVM state (storage, memory), this clone is expensive and variable,
   causing latency variance per request.
4. **EVM simulation speed**: fluid_v1 route simulations may themselves be slower, reducing
   per-worker solve throughput even when no block-update pause is occurring.

## What Needs to Be Measured

### 1. Pool count per protocol

Add to the `TychoFeed` or `ProtocolRegistry` a startup log of pool count per protocol:

```rust
// In TychoFeed after initial snapshot is processed
for (protocol, components) in &market.component_topology() {
    info!(protocol, count = components.len(), "protocol pool count");
}
```

Expected output to check: how many edges does fluid_v1 add vs uniswap_v3, ekubo_v2, etc.?

### 2. Edge weight update duration per block

In `SolverWorker::process_event` (worker.rs), time the `update_edge_weights_with_derived` call:

```rust
let start = Instant::now();
let updated = self.graph_manager.update_edge_weights_with_derived(&market, &derived);
let elapsed = start.elapsed();
info!(worker_id, updated, elapsed_ms = elapsed.as_millis(), "edge weights updated");
```

Compare mean and max across blocks for 7-protocol vs 8-protocol runs.

### 3. Pool depth computation duration per protocol

In `PoolDepthComputation::compute`, break out timing by protocol system. The computation
iterates all components — add a per-protocol count and timing summary:

```rust
// After the computation loop
for (protocol, stats) in &per_protocol_stats {
    info!(protocol, count = stats.computed, skipped = stats.skipped,
          elapsed_ms = stats.elapsed_ms, "pool depth by protocol");
}
```

This will reveal whether fluid_v1 depths take disproportionately long, causing variable
block-update durations.

### 4. extract_subset clone cost

In `MostLiquidAlgorithm::find_best_route`, time the `extract_subset` call:

```rust
let subset_start = Instant::now();
let market = { let m = market.read().await; m.extract_subset(&component_ids) };
let subset_elapsed = subset_start.elapsed();
if subset_elapsed.as_millis() > 10 {
    warn!(elapsed_ms = subset_elapsed.as_millis(), components = component_ids.len(),
          "slow extract_subset");
}
```

If fluid_v1 states are large, `clone_box()` will show up here as a source of tail latency.

### 5. Per-protocol solve time

Add a protocol breakdown to the existing `solve_time_ms` log in `fynd-rpc/src/api/handlers.rs`.
The `Quote` already carries route information including protocol names. Logging p50/p99 solve
time broken down by which protocols appear in the route will show whether fluid_v1 paths are
inherently slower to simulate.

### 6. Block-update pause measurement

Instrument the worker to record the time from receiving a `MarketEvent` to resuming task
processing:

```rust
// In SolverWorker event handling
let pause_start = Instant::now();
// ... handle event, update edge weights ...
let pause_ms = pause_start.elapsed().as_millis();
if pause_ms > 20 {
    warn!(worker_id, pause_ms, "block-update pause");
}
```

Plot the distribution of pause durations across blocks. With fluid_v1, expect higher and more
variable pauses than without.

## Experiments to Run

### Experiment A: isolate fluid_v1 alone

Run the benchmark with ONLY `fluid_v1` as the protocol:
```bash
PROTOCOLS="fluid_v1" WORKER_COUNTS="1,4,8,16,32" bash scripts/bench-remote.sh
```
This isolates fluid_v1's individual contribution to the graph and gives a baseline solve rate
for fluid_v1-only paths.

### Experiment B: add fluid_v1 back one protocol at a time

Start with the clean 7-protocol baseline and add protocols back one at a time, re-running at
16 and 32 workers each time. This pinpoints the threshold at which behaviour degrades and
whether it's fluid_v1 specifically or an additive graph-size effect.

### Experiment C: fix worker count, vary protocol count

At 32 workers, run 5-protocol, 6-protocol, 7-protocol, 8-protocol sets. Measure whether
throughput degradation is proportional to pool count or step-changes when fluid_v1 is added.

### Experiment D: measure edge weight update time directly

Add the instrumentation from §2 and §3 above, run a short benchmark (100 requests, `fixed:8`)
with RUST_LOG=info on a single worker with both 7 and 8 protocols. Compare the per-block
edge weight update logs side by side.

## Key Files

| File | Relevance |
| ---- | --------- |
| `fynd-core/src/algorithm/most_liquid.rs` | `update_edge_weights_with_derived`, `find_best_route`, `extract_subset` call |
| `fynd-core/src/derived/computations/pool_depth.rs` | `PoolDepthComputation` — most expensive per-block computation |
| `fynd-core/src/worker_pool/worker.rs` | Block-update pause: `process_event` calls edge weight update before resuming tasks |
| `fynd-core/src/feed/market_data.rs:181` | `extract_subset` — clones simulation states via `clone_box()` |
| `fynd-core/src/graph/petgraph.rs:322` | `update_edge_weights_with_derived` — iterates all edges per block |

## Known Unknowns

- Exact pool count contributed by fluid_v1 (not logged today)
- Whether fluid_v1 pool depth computation is slower per pool or just adds more pools
- Whether fluid_v1 `ProtocolSim` states are large (making `clone_box` slow)
- Whether the issue is specific to fluid_v1 or any protocol with >N pools (ekubo_v2 was also
  present in all runs without issue, suggesting this may be fluid_v1 specific)
- Whether BF also degrades with fluid_v1 included (not yet tested — BF benchmarks used 8
  protocols including fluid_v1 and still scaled cleanly, which is consistent with BF having a
  no-op edge update; but BF solve throughput per worker may also be lower with fluid_v1 due to
  the larger graph traversal)
