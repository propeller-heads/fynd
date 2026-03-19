---
description: Solver performance benchmarks across configurations.
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

# Benchmark Results

This page documents Fynd solver performance benchmarks across different configurations. All benchmarks use the `fynd-benchmark scale` subcommand, which builds a solver in-process for each worker count, runs a sustained load test, and reports throughput and latency statistics. See [benchmarking.md](benchmarking.md "mention") for how to run these yourself.

All results below were produced using `scripts/bench-remote.sh`, which provisions an EC2 instance, builds the solver from source, and runs the full scaling sweep automatically. To reproduce:

```bash
WORKER_COUNTS="1,2,3,4,6,8" \
NUM_REQUESTS=10000 \
POOL_CONFIG="single_pool.toml" \
TYCHO_URL="$TYCHO_URL" \
TYCHO_API_KEY="$TYCHO_API_KEY" \
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
