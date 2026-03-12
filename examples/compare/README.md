# Solver Comparison Tool

Sends identical quote requests to two running Fynd instances and compares output quality (amount out, gas, routes).

## Setup

You need two Fynd instances running simultaneously, typically from different git branches. Since both share the same binary target directory and metrics port, use **git worktrees** to avoid conflicts.

### 1. Create a worktree for the baseline branch

```bash
# From the main repo
wt switch main -b compare-baseline
# Or with plain git:
git worktree add ../fynd-baseline main
```

### 2. Start solver A (baseline) in the worktree

```bash
cd ../fynd-baseline  # or wherever the worktree lives
RUST_LOG=info cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4 \
  --http-port 3000 \
  --tycho-url <TYCHO_URL> \
  --tycho-api-key <API_KEY>
```

### 3. Start solver B (your branch) in the original repo

```bash
cd /path/to/fynd
RUST_LOG=info cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4 \
  --http-port 3001 \
  --tycho-url <TYCHO_URL> \
  --tycho-api-key <API_KEY>
```

### 4. Wait for both solvers to be healthy

```bash
curl http://localhost:3000/v1/health
curl http://localhost:3001/v1/health
```

Both should return `{"healthy": true, ...}` before running the comparison.

### 5. Run the comparison

```bash
cargo run --release --example compare -- \
  --url-a http://localhost:3000 \
  --url-b http://localhost:3001 \
  --label-a main \
  --label-b my-branch \
  -n 100
```

## Options

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

## Custom Requests

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

## Output

The tool prints a summary table to stdout and writes detailed per-request results to `comparison_results.json`. Positive bps diffs mean solver B returned more output.
