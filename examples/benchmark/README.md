# Tycho Solver Benchmark Tool

Benchmark tool for measuring tycho-solver performance with various parallelization strategies.

## Usage

```bash
export TYCHO_API_KEY=<your-key>
cargo run --example benchmark -- --rpc-url <RPC_URL> --tycho-url <TYCHO_URL> [OPTIONS]
```

### Options

```
--rpc-url <RPC_URL>              RPC endpoint URL (required) [env: RPC_URL]
--tycho-url <TYCHO_URL>          Tycho indexer URL (required) [env: TYCHO_URL]
--chain <CHAIN>                  Blockchain network [env: CHAIN] [default: Ethereum]
--protocols <PROTOCOLS>          Comma-separated protocol list [env: PROTOCOLS] [default: uniswap_v2,uniswap_v3]
--http-port <PORT>               HTTP server port [env: HTTP_PORT] [default: 3000]
--worker-pools-config <FILE>     Worker pool configuration file [env: WORKER_POOLS_CONFIG] [default: worker_pools.toml]
-n, --num-requests <NUM>         Number of requests to benchmark [env: NUM_REQUESTS] [default: 1]
-m, --parallelization-mode <MODE> Parallelization mode [env: PARALLELIZATION_MODE] [default: sequential]
--requests-file <FILE>           Path to JSON file with request templates [env: REQUESTS_FILE]
--output-file <FILE>             Output file for results (optional, not exported if omitted) [env: OUTPUT_FILE]
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

### Sequential benchmark

```bash
cargo run --example benchmark -- \
  --rpc-url https://node-provider.com/v2/YOUR_KEY \
  --tycho-url tycho-dev.propellerheads.xyz \
  -n 10
```

### Fixed concurrency with 10 parallel requests

```bash
cargo run --example benchmark -- \
  --rpc-url https://node-provider.com/v2/YOUR_KEY \
  --tycho-url tycho-dev.propellerheads.xyz \
  -m fixed:10 \
  -n 100
```

### Rate-based with custom requests

```bash
cargo run --example benchmark -- \
  --rpc-url https://node-provider.com/v2/YOUR_KEY \
  --tycho-url tycho-dev.propellerheads.xyz \
  -m rate:50 \
  -n 100 \
  --requests-file examples/benchmark/example_requests.json
```

## Output

Console output shows real-time progress, summary statistics, and ASCII histograms of timing distributions.

Optionally, results can be exported to a JSON file (using `--output-file`) with complete configuration, timing measurements, and statistics for further analysis.
