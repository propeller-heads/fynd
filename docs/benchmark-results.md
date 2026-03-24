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
| Protocols | `uniswap_v2`, `uniswap_v3` |
| Requests per iteration | 10,000 |
| Concurrency | `fixed:48` |
| Warmup | 30s after health check |
| Min TVL | 10.0 (native token) |

### Results

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) | RPS/Worker |
| ------: | -----------------: | -------------: | ----------: | ---------: |
|       1 |             501.55 |             95 |         102 |     501.55 |
|       2 |             905.47 |             53 |          56 |     452.73 |
|       3 |            1264.86 |             37 |          40 |     421.62 |
|       4 |            1687.48 |             28 |          30 |     421.87 |
|       6 |            2449.78 |             19 |          21 |     408.30 |
|       8 |            3338.90 |             14 |          15 |     417.36 |

### Analysis

Throughput scales nearly linearly across all tested worker counts, with each worker contributing ~420-500 req/s. Per-worker efficiency remains stable from 3 to 8 workers (~420 req/s), indicating minimal contention at these counts. The solver crosses 1000 req/s at just **3 workers** (1265 req/s). Latency stays tight throughout — P99 is only 15ms at 8 workers.

**Recommendation.** For 2-hop routing at 1000 req/s sustained throughput, provision at least 3 CPU cores. Use 4 cores for comfortable headroom.

## CPU Scaling: 3-Hop Routing

Measures how throughput scales with worker thread count for 3-hop route finding using the `most_liquid` algorithm.

### Setup

| Parameter | Value |
| --------- | ----- |
| Instance | AWS `c7a.8xlarge` (32 vCPU, AMD EPYC) |
| Algorithm | `most_liquid` |
| Max hops | 3 |
| Protocols | `uniswap_v2`, `uniswap_v3` |
| Requests per iteration | 10,000 |
| Concurrency | `fixed:48` |
| Warmup | 30s after health check |
| Min TVL | 10.0 (native token) |

### Results

| Workers | Throughput (req/s) | Median RT (ms) | P99 RT (ms) | RPS/Worker |
| ------: | -----------------: | -------------: | ----------: | ---------: |
|       1 |              66.68 |            714 |         884 |      66.68 |
|       2 |             128.07 |            372 |         458 |      64.04 |
|       4 |             240.02 |            198 |         256 |      60.01 |
|       8 |             469.44 |             99 |         136 |      58.68 |
|      12 |             642.88 |             70 |         110 |      53.57 |
|      16 |             820.75 |             53 |          91 |      51.30 |
|      20 |             939.58 |             45 |          92 |      46.98 |
|      24 |            1122.21 |             37 |          73 |      46.76 |
|      28 |            1205.98 |             33 |          70 |      43.07 |
|      32 |            1237.47 |             31 |          78 |      38.67 |

### Analysis

Throughput scales near-linearly up to 8 workers (~59 req/s per worker). Beyond that, per-worker efficiency gradually declines due to contention — dropping to ~39 req/s at 32 workers. The solver crosses 1000 req/s at **24 workers** (1122 req/s). Median latency drops from 714ms at 1 worker to 31ms at 32 workers, though P99 ticks up slightly at 32 workers (78ms vs 70ms at 28) reflecting the CPU limit of the instance.

**Recommendation.** For 3-hop routing at 1000 req/s sustained throughput, provision at least 24 CPU cores. Use 28-32 cores for headroom under variable load.

## Comparison: 2-Hop vs 3-Hop

| Target RPS | 2-Hop Workers | 3-Hop Workers | Ratio |
| ---------: | ------------: | ------------: | ----: |
|      1,000 |             3 |            24 |    8x |

2-hop routing requires roughly **8x fewer CPU cores** than 3-hop to reach the same throughput target. This reflects the combinatorial growth in route search space as hop count increases.

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
