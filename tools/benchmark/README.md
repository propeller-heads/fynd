# Benchmark & Comparison Tools

Tools for measuring Fynd's performance and comparing output quality between solver instances.

**Prerequisites:** Both tools require a running solver. See the [Quickstart](../../docs/get-started/quickstart/README.md) for setup instructions.

---

## Benchmark

Measures Fynd's performance with various parallelization strategies.

```bash
cargo run -p fynd-benchmark --release -- load [OPTIONS]
```

**Important:** Always use `--release` for accurate performance measurements.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--solver-url` | `http://localhost:3000` | Solver URL to benchmark against |
| `-n` | `1` | Number of requests to benchmark |
| `-m` | `sequential` | Parallelization mode |
| `--requests-file` | (none) | Path to JSON file with request templates |
| `--output-file` | (none) | Output file for results |

### Parallelization Modes

- **sequential** - Wait for each response before firing the next request
- **fixed:N** - Maintain exactly N concurrent requests (e.g., `fixed:5`)
- **rate:Nms** - Fire requests every N milliseconds (e.g., `rate:100`)

### Examples

```bash
# Sequential benchmark (10 requests)
cargo run -p fynd-benchmark --release -- load -n 10

# Fixed concurrency with 10 parallel requests
cargo run -p fynd-benchmark --release -- load -m fixed:10 -n 100

# Rate-based with custom requests
cargo run -p fynd-benchmark --release -- load \
  -m rate:50 -n 100 \
  --requests-file tools/benchmark/requests_set.json

# Export results to file
cargo run -p fynd-benchmark --release -- load -m fixed:10 -n 1000 --output-file results.json
```

### Output

Console output shows real-time progress, summary statistics, and ASCII histograms of timing distributions. Results can optionally be exported to JSON.

---

## Compare

Sends identical quote requests to two running Fynd instances and compares output quality (amount out, gas, routes).

```bash
cargo run -p fynd-benchmark --release -- compare [OPTIONS]
```

### Setup

You need two Fynd instances running simultaneously, typically from different git branches. Since both share the same binary target directory and metrics port, use **git worktrees** to avoid conflicts.

#### 1. Create a worktree for the baseline branch

```bash
# From the main repo
wt switch main -b compare-baseline
# Or with plain git:
git worktree add ../fynd-baseline main
```

#### 2. Start solver A (baseline) in the worktree

```bash
cd ../fynd-baseline
RUST_LOG=info cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4 \
  --http-port 3000 \
  --tycho-url <TYCHO_URL> \
  --tycho-api-key <API_KEY>
```

#### 3. Start solver B (your branch) in the original repo

```bash
cd /path/to/fynd
RUST_LOG=info cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4 \
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
|------|---------|-------------|
| `--url-a` | `http://localhost:3000` | Solver A base URL |
| `--url-b` | `http://localhost:3001` | Solver B base URL |
| `--label-a` | `main` | Label for solver A in output |
| `--label-b` | `branch` | Label for solver B in output |
| `-n` | `500` | Number of requests to send |
| `--requests-file` | (none) | Path to JSON file with custom requests |
| `--output` | `comparison_results.json` | Path for full results JSON |
| `--timeout-ms` | `15000` | Per-request timeout |
| `--seed` | `42` | Random seed for reproducibility |
### Custom Requests

You can supply your own requests via `--requests-file`. The file should be a JSON array of quote request bodies:

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

### Net-of-Gas Comparison

The compare tool uses the server-computed `amount_out_net_gas` field for net-of-gas comparisons. This value is calculated by the solver and represents the output amount minus gas cost denominated in the output token. It works for all token pairs, not just WETH-paired trades.

### Output

Prints a summary table to stdout and writes detailed per-request results to `comparison_results.json`. The summary includes:

- **Coverage**: how many trades each solver found routes for
- **Head-to-head win rate**: which solver returns more output (gross and net-of-gas)
- **Gas estimate comparison**: which solver uses less gas
- **Solve time**: latency percentiles for each solver
- **Route depth**: average number of swaps per solver
- **Significant outliers**: trades with >1 bps difference

Positive bps diffs mean solver B returned more output.

---

## Capacity

Steps an RPS ladder against a single solver until p95 round-trip latency breaches the SLO
(default: 1.2x the unloaded baseline), then reports the highest sustainable rate.

```bash
cargo run -p fynd-benchmark --release -- capacity [OPTIONS]
```

**Important:** Always use `--release` for accurate performance measurements.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--solver-url` | `http://localhost:3000` | Solver URL to measure |
| `--requests-file` | (none, uses embedded 50-trade sample) | Path to JSON file with request templates |
| `--ladder` | `5:5:200` | RPS ladder as `start:step:max` |
| `--step-duration-secs` | `60` | Seconds each ladder step is measured (after warm-up) |
| `--warmup-secs` | `5` | Seconds of discarded warm-up traffic before each step |
| `--baseline-requests` | `50` | Number of sequential requests for the unloaded baseline |
| `--slo-multiplier` | `1.2` | p95 degradation multiplier that fails a step |
| `--max-http-error-rate` | `0.001` | Maximum fraction of requests that may fail at the HTTP level per step |
| `--max-excess-unsolved-rate` | `0.001` | Maximum unsolved-order rate above baseline per step |
| `--timeout-ms` | `5000` | Per-request quote timeout |
| `--encoding` | `false` | Attach standard encoding options to every request |
| `--target-label` | (none) | Free-form label recorded in the report |
| `--output-file` | (none) | Also write the JSON report to this file |
| `--seed` | `42` | RNG seed for request sampling |

### Example

```bash
cargo run -p fynd-benchmark --release -- capacity \
  --ladder 5:5:100 \
  --step-duration-secs 30 \
  --output-file capacity_report.json
```

### Output

Prints a one-line capacity summary, then an `=== CAPACITY REPORT JSON ===` marker line followed
by the full `CapacityReport` JSON as the last thing on stdout — everything from the marker to EOF
(minus the marker line) is exactly the JSON, so in-cluster Jobs can retrieve it from pod logs.
Capacity is the highest ladder rate whose step passed the SLO; if the first step fails, capacity
is reported as unmeasured.

**Rate quantization:** request intervals are whole milliseconds, so offered rates quantize above
~30 rps (e.g. a target of 175 rps fires at a 5ms interval, i.e. 200 rps). When reporting capacity,
prefer the last passing step's `achieved_rps` over its `target_rps`.

**Sampling noise:** a heterogeneous request set's unsolved rate fluctuates step to step from
binomial sampling noise alone, so `--max-excess-unsolved-rate` may need a few percentage points of
headroom above the `0.001` default to avoid failing healthy steps.

---

## Request Data

By default, the load test uses a single WETH->USDC swap and the compare tool samples from a built-in set of 50 real aggregator trades. Both commands accept `--requests-file` to supply custom requests. See `requests_set.json` in this directory for the format.

### Using the Full 10k Trade Dataset

A 50-trade sample is embedded in the binary for zero-config use. For broader coverage, download the full 10k aggregator trade dataset:

```bash
# Download the dataset (~4.5 MB)
cargo run -p fynd-benchmark --release -- download-trades

# Use it with the compare tool
cargo run -p fynd-benchmark --release -- compare \
  --requests-file aggregator_trades_10k.json \
  -n 500
```

The dataset contains real aggregator trades pulled from Dune Analytics, covering ~2,500 unique token pairs.

## File Layout

| File | Description |
|------|-------------|
| `src/main.rs` | CLI entry point with `load`, `compare`, `scale`, `capacity`, `audit`, and `download-trades` subcommands |
| `src/benchmark.rs` | Load-test implementation |
| `src/compare.rs` | Comparison tool implementation |
| `src/capacity.rs` | Capacity subcommand: ladder orchestration against a single solver |
| `src/capacity_report.rs` | Capacity report types and SLO pass/fail evaluation |
| `src/config.rs` | Benchmark config, request templates, statistics types |
| `src/runner.rs` | Benchmark execution (sequential, fixed concurrency, rate-based) |
| `src/exporter.rs` | Statistics calculation and JSON export |
| `src/requests.rs` | Request generation, embedded trades, and file loading |
| `src/pairs.json` | Token definitions for symbol lookups in request labels |
| `src/trades_sample.json` | 50 real aggregator trades embedded in the binary |
| `requests_set.json` | Sample request templates |
