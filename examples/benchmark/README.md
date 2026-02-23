# Fynd Benchmark

Benchmark tool for measuring Fynd's performance with various parallelization strategies.

**Prerequisites:** This benchmark requires a running solver. See [examples/README.md](../README.md) for instructions on starting the solver.

## Usage

```bash
cargo run --example benchmark --release -- [OPTIONS]
```

**Important:** Always use `--release` for accurate performance measurements!

## Options

```
--solver-url <URL>               Solver URL to benchmark against [env: SOLVER_URL] [default: http://localhost:3000]
-n, --num-requests <NUM>         Number of requests to benchmark [env: NUM_REQUESTS] [default: 1]
-m, --parallelization-mode <MODE> Parallelization mode [env: PARALLELIZATION_MODE] [default: sequential]
--requests-file <FILE>           Path to JSON file with request templates [env: REQUESTS_FILE]
--output-file <FILE>             Output file for results (optional) [env: OUTPUT_FILE]
-h, --help                       Print help
```

CLI flags take precedence over environment variables.

## Parallelization Modes

- **sequential** - Wait for each response before firing the next request
- **fixed:N** - Maintain exactly N concurrent requests (e.g., `fixed:5`)
- **rate:Nms** - Fire requests every N milliseconds (e.g., `rate:100`)

## Request Templates

By default, uses a WETH→USDC swap. To use custom requests, create a JSON file with an array of `SolutionRequest` objects and use `--requests-file` to specify it. The benchmark will randomly select from your templates for each request. See `requests_set.json` for the format.

## Examples

### Sequential benchmark (10 requests)

```bash
cargo run --example benchmark --release -- \
  --solver-url http://localhost:3000 \
  -n 10
```

### Fixed concurrency with 10 parallel requests

```bash
cargo run --example benchmark --release -- \
  --solver-url http://localhost:3000 \
  -m fixed:10 \
  -n 100
```

### Rate-based with custom requests

```bash
cargo run --example benchmark --release -- \
  --solver-url http://localhost:3000 \
  -m rate:50 \
  -n 100 \
  --requests-file examples/benchmark/example_requests.json
```

### Export results to file

```bash
cargo run --example benchmark --release -- \
  --solver-url http://localhost:3000 \
  -m fixed:10 \
  -n 1000 \
  --output-file results.json
```

## Output

Console output shows real-time progress, summary statistics, and ASCII histograms of timing distributions.

Optionally, results can be exported to a JSON file (using `--output-file`) with complete configuration, timing measurements, and statistics for further analysis.

## A/B Algorithm Comparison

To compare two algorithm variants (e.g., old vs new Bellman-Ford), register both as separate worker pools in the same solver instance. The OrderManager runs all pools in parallel for every trade, so you get a head-to-head comparison on identical market state.

### 1. Register both algorithms

Create a second algorithm module (e.g., `bellman_ford_v2.rs`) implementing the `Algorithm` trait with a distinct `name()` return value. Register it in `src/worker_pool/registry.rs`:

```rust
// registry.rs
pub(crate) const AVAILABLE_ALGORITHMS: &[&str] = &[
    "most_liquid", "bellman_ford", "bellman_ford_v2"
];

// Add match arm in spawn_workers()
"bellman_ford_v2" => Ok(spawn_bellman_ford_v2_workers(params)),
```

### 2. Configure both pools in `worker_pools.toml`

Give both the same settings so the comparison is fair:

```toml
[pools.bellman_ford_pool]
algorithm = "bellman_ford"
num_workers = 2
task_queue_capacity = 1000
max_hops = 5
timeout_ms = 200

[pools.bellman_ford_v2_pool]
algorithm = "bellman_ford_v2"
num_workers = 2
task_queue_capacity = 1000
max_hops = 5
timeout_ms = 200
```

### 3. Start the solver with debug logging

The `order_manager` debug logs emit per-pool solution amounts for every trade:

```bash
RUST_LOG="fynd::order_manager=debug,fynd=info" \
cargo run --example solver --release -- \
  --rpc-url $RPC_URL \
  --tycho-url tycho-beta.propellerheads.xyz \
  --tycho-api-key $TYCHO_API_KEY \
  --min-tvl 25 \
  > solver.log 2>&1 &
```

### 4. Generate requests from Dune trades

Convert the 10K reference trade CSV to JSON requests:

```bash
python3 examples/benchmark/csv_to_requests.py \
  examples/benchmark/trades_10k_dune_eth_feb2026.csv \
  examples/benchmark/trades_10k_requests.json
```

### 5. Run the benchmark

```bash
cargo run --example benchmark --release -- \
  -n 10000 \
  --requests-file examples/benchmark/trades_10k_requests.json
```

### 6. Parse results

The `parse_pool_comparison.py` script extracts per-pool `amount_out_net_gas` from the debug logs and compares two pools head-to-head:

```bash
python3 examples/benchmark/parse_pool_comparison.py \
  solver.log \
  bellman_ford_pool \
  bellman_ford_v2_pool
```

Output shows wins, ties, and improvement percentages for contested trades (where both pools returned a route).

### 7. Clean up

After benchmarking, remove the v2 module and revert `registry.rs` / `worker_pools.toml` to their single-algorithm state. Merge the winning algorithm's code into the main module.
