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

**Scaling efficiency.** Throughput scales near-linearly up to 8 workers, with each worker contributing ~59 req/s. Beyond 8 workers, per-worker efficiency gradually declines due to contention and shared resource overhead. At 32 workers, per-worker throughput drops to ~39 req/s.

**1000 req/s target.** The solver crosses 1000 req/s at **24 workers** (1122 req/s). For a comfortable margin, 28 workers (1206 req/s) provides ~20% headroom.

**Latency.** Median round-trip time drops from 714ms at 1 worker to 31ms at 32 workers. The P99 latency follows a similar curve, settling around 70-78ms at high worker counts. The slight P99 increase from 28 to 32 workers (70ms to 78ms) reflects growing contention at the CPU limit of the instance.

**Recommendation.** For 3-hop routing at 1000 req/s sustained throughput, provision at least 24 CPU cores. Use 28-32 cores for headroom under variable load.
