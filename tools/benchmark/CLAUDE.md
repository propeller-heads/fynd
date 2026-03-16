# fynd-benchmark

Benchmark and comparison tooling for Fynd solvers. Requires one or more running Fynd solver instances before executing any command.

## Commands

Three subcommands available via `cargo run -p fynd-benchmark --release --`:

- **`load`** — Load-test a single solver. Measures latency (round-trip, solve time, overhead) and throughput. Supports sequential, fixed-concurrency, and rate-based parallelization modes. Prints statistics and ASCII histograms to stdout; optionally exports results to JSON.

- **`compare`** — Compare output quality between two solver instances. Sends identical quote requests to both and reports differences in amount out (bps), gas estimates, route selection, and status. Requires two solvers running on different ports (use git worktrees to run different branches simultaneously).

- **`download-trades`** — Download real DEX trades from Dune Analytics for benchmarking. Queries the `dex.trades` table and saves results in the request JSON format. Requires the `DUNE_API_KEY` environment variable.

Run `--help` on either subcommand for detailed options.

## Module Overview

| Module | Purpose |
|---|---|
| `main.rs` | CLI entry point. Parses `load` / `compare` subcommands via clap and dispatches to the corresponding handler. |
| `benchmark.rs` | `load` subcommand handler. Builds a `FyndClient`, checks solver health, loads request templates, runs the benchmark via `runner`, and prints results via `exporter`. |
| `compare.rs` | `compare` subcommand handler. Builds two `FyndClient` instances, sends identical requests sequentially to both, computes per-request metrics (amount out diff in bps, gas diff, route match), prints a summary table, and exports full results to JSON. |
| `config.rs` | Shared types: `ParallelizationMode` enum (`Sequential`, `FixedConcurrency`, `RateBased`), `BenchmarkConfig`, `BenchmarkResults`, `TimingStats`. |
| `runner.rs` | Benchmark execution engine. Implements three strategies: sequential (one-at-a-time), fixed concurrency (semaphore-bounded), and rate-based (fire at fixed intervals). Returns timing vectors and order counts. |
| `exporter.rs` | Statistics calculation (`TimingStats::from_measurements` — min/max/mean/median/p95/p99/stddev), ASCII histogram rendering, and JSON export of `BenchmarkResults`. |
| `requests.rs` | Request generation and loading. Provides a default WETH→USDC request, generates random requests from `pairs.json` (embedded at compile time), and loads custom requests from a JSON file. |

## Data Files

- **`pairs.json`** — Token definitions and trading pairs with sample amounts, embedded into the binary via `include_str!`. Used by `compare` to generate random requests.
- **`requests_set.json`** — Sample request templates file. Both commands accept `--requests-file` to use custom request sets in this format.
