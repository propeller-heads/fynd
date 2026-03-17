---
description: Measure solver performance and compare output quality between branches.
icon: gauge-max
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

# Benchmarking

Fynd ships with a benchmark tool (`fynd-benchmark`) for load-testing a solver and comparing output quality between two solver instances. Both features live in `tools/benchmark/` and run against live solver instances.

{% hint style="info" %}
**Prerequisite:** You need a running solver before using any benchmark command. See [quickstart](quickstart/ "mention") for setup instructions.
{% endhint %}

## Load Testing

Measures latency (round-trip, solve time, overhead) and throughput for a single solver instance.

```bash
cargo run -p fynd-benchmark --release -- load [OPTIONS]
```

{% hint style="warning" %}
Always build with `--release`. Debug builds produce misleading latency numbers.
{% endhint %}

### Options

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--solver-url` | `http://localhost:3000` | Solver URL to benchmark against |
| `-n` | `1` | Number of requests to send |
| `-m` | `sequential` | Parallelization mode |
| `--requests-file` | _(none)_ | Path to JSON file with request templates |
| `--output-file` | _(none)_ | Output file for JSON results |

### Parallelization Modes

Control how requests are dispatched with the `-m` flag:

* **`sequential`** — Send one request at a time, wait for each response before sending the next. Good for measuring single-request latency.
* **`fixed:N`** — Maintain exactly N concurrent in-flight requests (e.g., `fixed:5`). Good for simulating sustained load.
* **`rate:Nms`** — Fire a new request every N milliseconds regardless of pending responses (e.g., `rate:100`). Good for testing behavior under a fixed request rate.

### Examples

```bash
# Measure single-request latency (10 sequential requests)
cargo run -p fynd-benchmark --release -- load -n 10

# Simulate 10 concurrent users sending 100 total requests
cargo run -p fynd-benchmark --release -- load -m fixed:10 -n 100

# Fire a request every 50ms using custom request templates
cargo run -p fynd-benchmark --release -- load \
  -m rate:50 -n 100 \
  --requests-file tools/benchmark/requests_set.json

# Export results to JSON for further analysis
cargo run -p fynd-benchmark --release -- load \
  -m fixed:10 -n 1000 \
  --output-file results.json
```

### Output

The tool prints real-time progress, summary statistics (min, max, mean, median, p95, p99, stddev), and ASCII histograms of timing distributions. Pass `--output-file` to export the full results as JSON.

---

## Comparing Two Solvers

Sends identical quote requests to two running solver instances and compares output quality: amount out (in bps), gas estimates, and route selection.

```bash
cargo run -p fynd-benchmark --release -- compare [OPTIONS]
```

### Setup

You need two Fynd instances running simultaneously — typically from different git branches. Since both share the same binary target directory and metrics port, use **git worktrees** to avoid conflicts.

#### 1. Create a worktree for the baseline

```bash
# From the main repo
git worktree add ../fynd-baseline main
```

#### 2. Start solver A (baseline) in the worktree

```bash
cd ../fynd-baseline
RUST_LOG=info cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3 \
  --http-port 3000 \
  --tycho-url <TYCHO_URL> \
  --tycho-api-key <API_KEY>
```

#### 3. Start solver B (your branch) in the original repo

```bash
cd /path/to/fynd
RUST_LOG=info cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3 \
  --http-port 3001 \
  --tycho-url <TYCHO_URL> \
  --tycho-api-key <API_KEY>
```

#### 4. Wait for both solvers to be healthy

```bash
curl http://localhost:3000/v1/health
curl http://localhost:3001/v1/health
```

Both should return `{"healthy": true, ...}` before running the comparison.

#### 5. Run the comparison

```bash
cargo run -p fynd-benchmark --release -- compare \
  --url-a http://localhost:3000 \
  --url-b http://localhost:3001 \
  --label-a main \
  --label-b my-branch \
  -n 100
```

### Options

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--url-a` | `http://localhost:3000` | Solver A (baseline) URL |
| `--url-b` | `http://localhost:3001` | Solver B (candidate) URL |
| `--label-a` | `main` | Label for solver A in output |
| `--label-b` | `branch` | Label for solver B in output |
| `-n` | `500` | Number of requests to send |
| `--requests-file` | _(none)_ | Path to JSON file with custom requests |
| `--output` | `comparison_results.json` | Path for full results JSON |
| `--timeout-ms` | `15000` | Per-request timeout in milliseconds |
| `--seed` | `42` | Random seed for reproducibility |

### Net-of-Gas Comparison

The compare tool uses the server-computed `amount_out_net_gas` field for net-of-gas output comparison. This value represents the output amount minus gas cost denominated in the output token, calculated by the solver. It works for all token pairs.

### Output

Prints a summary table to stdout showing win/loss counts and bps differences (both gross and net-of-gas). Writes detailed per-request results to the output JSON file. Positive bps diffs mean solver B returned more output than solver A.

---

## Request Data

By default, the load test uses a single WETH→USDC swap and the compare tool samples from a built-in set of 50 real aggregator trades. Both commands accept `--requests-file` to supply custom requests.

### Downloading the Full Dataset

For broader coverage, download the full 10k aggregator trade dataset:

```bash
cargo run -p fynd-benchmark --release -- download-trades
```

Then use it with either command via `--requests-file aggregator_trades_10k.json`.

### Custom Request Format

The file should be a JSON array of quote request bodies:

```json
[
  {
    "orders": [{
      "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "amount": "1000000000000000000000",
      "side": "sell",
      "sender": "0x0000000000000000000000000000000000000001"
    }]
  }
]
```

See `tools/benchmark/requests_set.json` in the repository for a complete example.
