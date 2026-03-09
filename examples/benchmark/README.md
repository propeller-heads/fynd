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
