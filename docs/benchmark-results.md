---
description: Throughput and latency benchmarks across routing configurations.
icon: chart-line
layout:
  width: default
  title:
    visible: true
  description:
    visible: true
  tableOfContents:
    visible: true
  outline:
    visible: true
  pagination:
    visible: true
  metadata:
    visible: true
  tags:
    visible: true
---

# Performance

This page documents Fynd solver performance benchmarks across different configurations. All benchmarks use the `fynd-benchmark scale` subcommand, which builds a solver in-process for each worker count, runs a sustained load test, and reports throughput and latency statistics. See [benchmarking.md](guides/benchmarking.md "mention") for how to run these yourself.

All results below were produced using `scripts/bench-remote.sh`, which provisions an EC2 instance, builds the solver from source, and runs the full scaling sweep automatically. Pool configuration files used by each benchmark are in [`tools/benchmark/`](../tools/benchmark). To reproduce the `most_liquid` results:

```bash
WORKER_COUNTS="1,2,3,4,6,8" \
NUM_REQUESTS=10000 \
POOL_CONFIG="tools/benchmark/most_liquid_2hop.toml" \
TYCHO_URL="$TYCHO_URL" \
TYCHO_API_KEY="$TYCHO_API_KEY" \
RPC_URL="$RPC_URL" \
  bash scripts/bench-remote.sh
```

## CPU Scaling: 2-Hop Routing

Measures how throughput scales with worker thread count for 2-hop route finding using the `most_liquid` algorithm.

### Setup

| Parameter | Value |
| --------- | ----- |
| Instance | AWS `c7a.8xlarge` (32 vCPU, AMD EPYC) |
| Algorithm | `most_liquid` |
| Max hops | 2 |
| Protocols | `uniswap_v2`, `uniswap_v3`, `uniswap_v4`, `sushiswap_v2`, `pancakeswap_v2`, `pancakeswap_v3`, `ekubo_v2`, `fluid_v1` |
| Requests per iteration | 10,000 |
| Concurrency | `fixed:48` |
| Warmup | 30s after health check |
| Config | [`tools/benchmark/most_liquid_2hop.toml`](../tools/benchmark/most_liquid_2hop.toml) |

### Results

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) | RPS/Worker |
| ------: | -----------------: | -------------: | ----------: | ---------: |
|       1 |             397.19 |            120 |         129 |     397.19 |
|       2 |             743.16 |             64 |          69 |     371.58 |
|       3 |            1035.84 |             46 |          50 |     345.28 |
|       4 |            1444.04 |             33 |          36 |     361.01 |
|       6 |            2109.26 |             22 |          25 |     351.54 |
|       8 |            2820.08 |             16 |          18 |     352.51 |

### Analysis

Throughput scales nearly linearly across all tested worker counts (~350-397 req/s per worker). The solver crosses 1000 req/s at **3 workers** (1036 req/s). Latency stays tight throughout — P99 is only 18ms at 8 workers.

**Recommendation.** For `most_liquid` 2-hop routing at 1000 req/s sustained throughput, provision at least 3 CPU cores. Use 4 cores for comfortable headroom.

## CPU Scaling: 3-Hop Routing

Measures how throughput scales with worker thread count for 3-hop route finding using the `most_liquid` algorithm.

### Setup

| Parameter | Value |
| --------- | ----- |
| Instance | AWS `c7a.8xlarge` (32 vCPU, AMD EPYC) |
| Algorithm | `most_liquid` |
| Max hops | 3 |
| Protocols | `uniswap_v2`, `uniswap_v3`, `uniswap_v4`, `sushiswap_v2`, `pancakeswap_v2`, `pancakeswap_v3`, `ekubo_v2`, `fluid_v1` |
| Requests per iteration | 10,000 |
| Concurrency | `fixed:48` |
| Warmup | 30s after health check |
| Config | [`tools/benchmark/most_liquid_3hop.toml`](../tools/benchmark/most_liquid_3hop.toml) |

### Results

The 12–32 worker results below reflect the lock-handling improvements merged after the initial run (snapshot-then-simulate, two-phase locking in derived computations). The P99 spikes previously observed at 24+ workers are eliminated.

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) | RPS/Worker |
| ------: | -----------------: | -------------: | ----------: | ---------: |
|      12 |             211.99 |            213 |         338 |      17.67 |
|      16 |             221.04 |            201 |         334 |      13.82 |
|      20 |             342.76 |            120 |         238 |      17.14 |
|      24 |             323.45 |            113 |         289 |      13.48 |
|      28 |             400.13 |             94 |         238 |      14.29 |
|      32 |             274.16 |            123 |         337 |       8.57 |

### Analysis

Throughput is non-monotonic across worker counts, reflecting the periodic block-update pause rather than a clean CPU-bound scaling curve. The peak is **400 req/s at 28 workers**. P99 stays bounded (≤338ms) across all tested counts — a significant improvement over the previous run where P99 reached 794ms at 24 workers due to write-lock contention during derived data recomputation. The solver does not reach 1000 req/s on this instance with 8 protocols.

**Recommendation.** For `most_liquid` 3-hop routing with a large protocol set, expect a throughput ceiling around 300–400 req/s on a 32-vCPU instance. Consider `bellman_ford` for higher 3-hop throughput (see below).

## Comparison: 2-Hop vs 3-Hop

| Target RPS | 2-Hop Workers | 3-Hop Workers | Notes |
| ---------: | ------------: | ------------: | ----- |
|      1,000 |             3 |             — | 3-hop peaks at ~400 req/s; 1000 req/s not reached |

With 8 protocols, `most_liquid` 3-hop does not reach 1000 req/s on a 32-vCPU instance. The combinatorial growth in the 3-hop search space over a large protocol set creates a hard throughput ceiling for this algorithm.

## CPU Scaling: Bellman-Ford 2-Hop

Measures how throughput scales with worker thread count for 2-hop route finding using the `bellman_ford` algorithm.

### Setup

| Parameter | Value |
| --------- | ----- |
| Instance | AWS `c7a.8xlarge` (32 vCPU, AMD EPYC) |
| Algorithm | `bellman_ford` |
| Max hops | 2 |
| Protocols | `uniswap_v2`, `uniswap_v3`, `uniswap_v4`, `sushiswap_v2`, `pancakeswap_v2`, `pancakeswap_v3`, `ekubo_v2`, `fluid_v1` |
| Requests per iteration | 10,000 |
| Concurrency | `fixed:48` |
| Warmup | 30s after health check |
| Config | [`tools/benchmark/bellman_ford_2hop.toml`](../tools/benchmark/bellman_ford_2hop.toml) |

To reproduce:

```bash
WORKER_COUNTS="1,2,3,4,6,8" \
NUM_REQUESTS=10000 \
POOL_CONFIG="tools/benchmark/bellman_ford_2hop.toml" \
PROTOCOLS="uniswap_v2,uniswap_v3,uniswap_v4,sushiswap_v2,pancakeswap_v2,pancakeswap_v3,ekubo_v2,fluid_v1" \
TYCHO_URL="$TYCHO_URL" \
TYCHO_API_KEY="$TYCHO_API_KEY" \
RPC_URL="$RPC_URL" \
  bash scripts/bench-remote.sh
```

### Results

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) | RPS/Worker |
| ------: | -----------------: | -------------: | ----------: | ---------: |
|       1 |              85.31 |            562 |         586 |      85.31 |
|       2 |             154.58 |            310 |         328 |      77.29 |
|       3 |             220.12 |            217 |         228 |      73.37 |
|       4 |             290.93 |            164 |         173 |      72.73 |
|       6 |             406.87 |            117 |         124 |      67.81 |
|       8 |             518.54 |             92 |          99 |      64.82 |

### Analysis

Throughput scales near-linearly across all tested worker counts. Per-worker efficiency declines gradually from ~85 req/s at 1 worker to ~65 req/s at 8 workers. The solver does not cross 1000 req/s within the tested 8-worker range; linear extrapolation places that threshold at approximately **16 workers**.

These benchmarks subscribe to 8 protocols vs. 2 for the `most_liquid` results above, significantly expanding the routing graph. Direct per-worker throughput comparisons between the two algorithms should account for this difference.

**Recommendation.** For Bellman-Ford 2-hop routing at 1000 req/s sustained throughput, provision at least 16 CPU cores.

## CPU Scaling: Bellman-Ford 3-Hop

Measures how throughput scales with worker thread count for 3-hop route finding using the `bellman_ford` algorithm.

### Setup

| Parameter | Value |
| --------- | ----- |
| Instance | AWS `c7a.8xlarge` (32 vCPU, AMD EPYC) |
| Algorithm | `bellman_ford` |
| Max hops | 3 |
| Protocols | `uniswap_v2`, `uniswap_v3`, `uniswap_v4`, `sushiswap_v2`, `pancakeswap_v2`, `pancakeswap_v3`, `ekubo_v2`, `fluid_v1` |
| Requests per iteration | 10,000 |
| Concurrency | `fixed:48` |
| Warmup | 30s after health check |
| Config | [`tools/benchmark/bellman_ford_3hop.toml`](../tools/benchmark/bellman_ford_3hop.toml) |

To reproduce:

```bash
WORKER_COUNTS="1,2,4,8,12,16,20,24,28,32" \
NUM_REQUESTS=10000 \
POOL_CONFIG="tools/benchmark/bellman_ford_3hop.toml" \
PROTOCOLS="uniswap_v2,uniswap_v3,uniswap_v4,sushiswap_v2,pancakeswap_v2,pancakeswap_v3,ekubo_v2,fluid_v1" \
TYCHO_URL="$TYCHO_URL" \
TYCHO_API_KEY="$TYCHO_API_KEY" \
RPC_URL="$RPC_URL" \
  bash scripts/bench-remote.sh
```

### Results

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) | RPS/Worker |
| ------: | -----------------: | -------------: | ----------: | ---------: |
|       1 |              65.36 |            735 |         780 |      65.36 |
|       2 |             121.68 |            394 |         416 |      60.84 |
|       4 |             233.09 |            205 |         221 |      58.27 |
|       8 |             429.44 |            111 |         121 |      53.68 |
|      12 |             584.86 |             81 |          90 |      48.74 |
|      16 |             760.92 |             62 |          71 |      47.56 |
|      20 |             874.51 |             54 |          65 |      43.73 |
|      24 |             974.94 |             48 |          57 |      40.62 |
|      28 |            1201.92 |             39 |          49 |      42.93 |
|      32 |            1219.96 |             38 |          49 |      38.12 |

### Analysis

Throughput scales near-linearly up to 8 workers (~54 req/s per worker). Beyond that, per-worker efficiency gradually declines — from ~49 req/s at 12 workers to ~38 req/s at 32 workers — as the instance approaches its CPU ceiling. The solver crosses 1000 req/s at **28 workers** (1202 req/s). Median latency falls from 735ms at 1 worker to 38ms at 32 workers; P99 stabilises at 49ms from 28 workers onward.

**Recommendation.** For Bellman-Ford 3-hop routing at 1000 req/s sustained throughput, provision at least 28 CPU cores. Use 32 cores for headroom under variable load.

## Comparison: Bellman-Ford 2-Hop vs 3-Hop

| Target RPS | 2-Hop Workers | 3-Hop Workers | Ratio |
| ---------: | ------------: | ------------: | ----: |
|      1,000 |           ~16 |            28 |  ~1.8x |

Bellman-Ford 3-hop requires roughly **1.8× more CPU cores** than 2-hop to reach the same throughput target. This is a much smaller penalty than seen with `most_liquid` (8×), reflecting Bellman-Ford's more uniform search cost growth across hop counts — it already explores the full path space at 2 hops, so adding a third hop grows the search space less dramatically relative to the base cost.

## Algorithm Comparison: most_liquid vs bellman_ford

All results in this section use identical hardware, protocol set, and request load.

### 2-Hop

| Workers | most_liquid (req/s) | bellman_ford (req/s) | Ratio |
| ------: | ------------------: | -------------------: | ----: |
|       1 |              397.19 |                85.31 |  4.7x |
|       2 |              743.16 |               154.58 |  4.8x |
|       3 |             1035.84 |               220.12 |  4.7x |
|       4 |             1444.04 |               290.93 |  5.0x |
|       6 |             2109.26 |               406.87 |  5.2x |
|       8 |             2820.08 |               518.54 |  5.4x |

`most_liquid` is consistently **~5× faster** for 2-hop routing. Its greedy liquidity-ranked search terminates early once the best path is found, while Bellman-Ford explores all paths exhaustively.

### 3-Hop

| Workers | most_liquid (req/s) | bellman_ford (req/s) | Winner |
| ------: | ------------------: | -------------------: | ------ |
|      16 |              221.04 |               760.92 | bellman_ford (3.4x) |
|      20 |              342.76 |               874.51 | bellman_ford (2.6x) |
|      24 |              323.45 |               974.94 | bellman_ford (3.0x) |
|      28 |              400.13 |              1201.92 | bellman_ford (3.0x) |
|      32 |              274.16 |              1219.96 | bellman_ford (4.4x) |

At 3 hops with 8 protocols, the results **reverse**: `bellman_ford` is **3–4× faster** than `most_liquid` and keeps scaling linearly while `most_liquid` plateaus around 300–400 req/s. The `most_liquid` greedy search over a large 3-hop graph is bounded by periodic block-update pauses (edge weight recomputation on every block); Bellman-Ford carries no pre-computed edge state so its per-block pause is negligible.
