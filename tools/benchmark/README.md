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
| `--rpc-url` | (none) | Ethereum RPC URL for gas price (enables net-of-gas comparison) |

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

Pass `--rpc-url` to enable net-of-gas output comparison. The tool fetches the current gas price and approximates gas cost in the output token for trades involving WETH. This is important when comparing algorithms with different route depths (e.g., a 3-hop route may have higher gross output but also higher gas).

```bash
cargo run -p fynd-benchmark --release -- compare \
  --rpc-url https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY \
  -n 500
```

The net-of-gas estimate works for trades where token_in or token_out is WETH. For other pairs, the tool reports gross output and gas separately.

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

## Request Templates

By default, the benchmark uses a single WETH->USDC swap and the compare tool generates random requests from a built-in set of token pairs. Both tools accept `--requests-file` to use custom request sets. See `requests_set.json` in this directory for the format.

### Using Real Trade Data

For more representative coverage, download real on-chain trades from Dune Analytics using the `download-trades` subcommand:

```bash
# Download 1k recent Ethereum trades (requires DUNE_API_KEY)
export DUNE_API_KEY="your_key"
cargo run -p fynd-benchmark --release -- download-trades

# Then use them in a comparison
cargo run -p fynd-benchmark --release -- compare \
  --requests-file trades_1k_requests.json \
  --rpc-url https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY \
  -n 500
```

See the [Download Trades](#download-trades) section for all options.

## Download Trades

Downloads real DEX trades from Dune Analytics and saves them in the request JSON format used by `load` and `compare`.

```bash
cargo run -p fynd-benchmark --release -- download-trades [OPTIONS]
```

**Prerequisites:** Requires the `DUNE_API_KEY` environment variable. Get one at https://dune.com/settings/api.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-n` | `1000` | Number of trades to download |
| `--min-usd` | `100` | Minimum trade size in USD |
| `--hours` | `24` | Lookback window in hours |
| `--chain` | `ethereum` | Blockchain to query |
| `-o` | `trades_1k_requests.json` | Output file path |

### Examples

```bash
# Default: 1k recent Ethereum trades
cargo run -p fynd-benchmark --release -- download-trades

# 500 large trades from the last week
cargo run -p fynd-benchmark --release -- download-trades -n 500 --min-usd 10000 --hours 168

# Base chain trades
cargo run -p fynd-benchmark --release -- download-trades --chain base -o trades_base.json
```

---

## File Layout

| File | Description |
|------|-------------|
| `src/main.rs` | CLI entry point with `load`, `compare`, and `download-trades` subcommands |
| `src/benchmark.rs` | Load-test implementation |
| `src/compare.rs` | Comparison tool implementation |
| `src/download.rs` | Dune Analytics trade downloader |
| `src/config.rs` | Benchmark config, request templates, statistics types |
| `src/runner.rs` | Benchmark execution (sequential, fixed concurrency, rate-based) |
| `src/exporter.rs` | Statistics calculation and JSON export |
| `src/requests.rs` | Request generation and file loading |
| `src/pairs.json` | Token and pair definitions for random request generation |
| `requests_set.json` | Sample request templates |
