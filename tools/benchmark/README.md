# Benchmark & Comparison Tools

Tools for measuring Fynd's performance and comparing output quality between solver instances.

**Prerequisites:** Both tools require a running solver. See the [Quickstart](../../docs/get-started/quickstart/README.md) for setup instructions.

---

## Benchmark

Measures Fynd's performance with various parallelization strategies.

```bash
cargo run --example benchmark --release -- [OPTIONS]
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
cargo run --example benchmark --release -- -n 10

# Fixed concurrency with 10 parallel requests
cargo run --example benchmark --release -- -m fixed:10 -n 100

# Rate-based with custom requests
cargo run --example benchmark --release -- \
  -m rate:50 -n 100 \
  --requests-file tools/benchmark/requests_set.json

# Export results to file
cargo run --example benchmark --release -- -m fixed:10 -n 1000 --output-file results.json
```

### Output

Console output shows real-time progress, summary statistics, and ASCII histograms of timing distributions. Results can optionally be exported to JSON.

---

## Compare

Sends identical quote requests to two running Fynd instances and compares output quality (amount out, gas, routes).

```bash
cargo run --example compare --release -- [OPTIONS]
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
cargo run --example compare --release -- \
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
| `-n` | `100` | Number of requests to send |
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

### Output

Prints a summary table to stdout and writes detailed per-request results to `comparison_results.json`. Positive bps diffs mean solver B returned more output.

---

## Request Templates

By default, the benchmark uses a single WETH->USDC swap and the compare tool generates random requests from a built-in set of token pairs. Both tools accept `--requests-file` to use custom request sets. See `requests_set.json` in this directory for the format.

## File Layout

| File | Description |
|------|-------------|
| `benchmark.rs` | Performance benchmark entry point |
| `config.rs` | Benchmark config, request templates, statistics types |
| `runner.rs` | Benchmark execution (sequential, fixed concurrency, rate-based) |
| `exporter.rs` | Statistics calculation and JSON export |
| `compare.rs` | Comparison tool entry point |
| `requests.rs` | Request generation and file loading |
| `pairs.json` | Token and pair definitions for random request generation |
| `requests_set.json` | Sample request templates |
