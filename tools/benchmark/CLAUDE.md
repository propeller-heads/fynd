# fynd-benchmark

Benchmark and comparison tooling for Fynd solvers. Requires one or more running Fynd solver instances before executing any command.

## Commands

Subcommands available via `cargo run -p fynd-benchmark --release --`:

- **`load`** — Load-test a single solver. Measures latency (round-trip, solve time, overhead) and throughput. Supports sequential, fixed-concurrency, and rate-based parallelization modes. Prints statistics and ASCII histograms to stdout; optionally exports results to JSON.

- **`compare`** — Compare output quality between two solver instances. Sends identical quote requests to both and reports differences in amount out (bps), net-of-gas output (server-side `amount_out_net_gas`), gas estimates, route selection, and status. Requires two solvers running on different ports (use git worktrees to run different branches simultaneously).

- **`scale`** — Measure how solver throughput scales with worker thread count. Builds and tears down the solver in-process for each iteration; no external solver instance needed. `--protocols` accepts the `all_onchain` and `native_onchain` expansion tokens (the latter drops VM-simulated `vm:*` protocols), and `--min-tvl` sets the TVL floor — both resolved via the shared `fynd_rpc::protocols::resolve_protocols`.

- **`capacity`** — Step an RPS ladder against a solver until p95 breaches the latency SLO; reports the highest sustainable rate. Measures an unloaded sequential baseline first, then fires rate-based traffic for each ladder step (after a discarded warm-up window). A step passes while p95 round-trip stays within the SLO multiplier of the baseline and error/unsolved rates stay below policy thresholds, all three tunable via `--slo-multiplier`, `--max-http-error-rate`, and `--max-excess-unsolved-rate`. Heterogeneous request sets need a few percentage points of `--max-excess-unsolved-rate` headroom above the `0.001` default to absorb step-to-step sampling noise. Prints a JSON report as the last thing on stdout, after a `=== CAPACITY REPORT JSON ===` marker line (so in-cluster Jobs can retrieve it from pod logs — marker to EOF, minus the marker line, is exactly the JSON), and optionally writes it to `--output-file`.

- **`download-trades`** — Download the full 10k aggregator trade dataset from GitHub Releases for use with `--requests-file`.

- **`audit`** — Compare Fynd quote quality against external aggregators (Nordstern, KyberSwap, 0x). Runs over a trade dataset, records per-trade participant results (amount out, gas, protocols, route, eth_call on-chain validation), and writes a JSON report.

Run `--help` on any subcommand for detailed options.

## Running the Audit

### Prerequisites

1. **Build the release binary** (required — debug is too slow for vm:curve EVM simulation):
   ```bash
   cargo build -p fynd-benchmark --release
   ```

2. **Start the solver** with the `.env` vars sourced and RFQ protocols included:
   ```bash
   set -a && source .env && set +a
   RUST_LOG=info ./target/release/fynd serve \
     -w worker_pools_pfw3.toml \
     --protocols all_onchain,rfq:bebop,rfq:hashflow \
     > /tmp/fynd_solver.log 2>&1 &
   ```
   Wait until `/v1/health` returns `healthy: true` (~5–10 min for derived data).

3. **Download the full trade dataset** (once):
   ```bash
   ./target/release/fynd-benchmark download-trades
   ```

### Standard full audit

```bash
TIMESTAMP=$(date +%Y%m%d_%H%M)
./target/release/fynd-benchmark audit \
  --fynd-url http://localhost:3000 \
  --trade-data aggregator_trades_10k.json \
  --rpc-url "$RPC_URL" \
  --output "audit_results_${TIMESTAMP}.json" \
  2>&1 | tee "audit_${TIMESTAMP}.log"
```

`--rpc-url` enables eth_call on-chain validation of every Fynd quote, populating `eth_call_amount_out`, `eth_call_gas_used`, and `gas_adjusted_diff_bps_onchain` in the output. **Always pass it** — omitting it leaves `gas_adjusted_diff_bps_onchain: null` for all trades.

`$RPC_URL` is set by sourcing `.env` (see above). The `.env` file has the canonical node endpoint.

### Targeted mini-audit (specific pairs)

Create a trade-data file matching the dataset schema:
```json
[
  {
    "orders": [
      {
        "id": "",
        "token_in": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        "token_out": "<TOKEN_OUT>",
        "amount": "10000000000",
        "side": "sell",
        "sender": "0x0000000000000000000000000000000000000001",
        "receiver": null
      }
    ],
    "options": { "timeout_ms": 5000, "min_responses": null, "max_gas": null }
  }
]
```

Then run:
```bash
./target/release/fynd-benchmark audit \
  --fynd-url http://localhost:3000 \
  --trade-data /tmp/my_pairs.json \
  --rpc-url "$RPC_URL" \
  --output /tmp/my_audit_out.json
```

### Reading results

```python
import json
data = json.load(open("audit_results_TIMESTAMP.json"))
for r in data["results"]:
    for p in r["participants"]:
        print(p["name"], p["raw_diff_bps"], p["gas_adjusted_diff_bps_onchain"])
```

Key fields per participant:
- `protocols` — list of DEX protocols used
- `route` — `[[protocol, pool_address], ...]` pairs (Fynd only)
- `raw_diff_bps` — output diff vs Fynd ignoring gas
- `gas_adjusted_diff_bps_reported` — diff using solver-reported gas estimates
- `gas_adjusted_diff_bps_onchain` — diff using actual on-chain gas (requires `--rpc-url`)

## Module Overview

| Module | Purpose |
|---|---|
| `main.rs` | CLI entry point. Parses subcommands via clap and dispatches to the corresponding handler. |
| `benchmark.rs` | `load` subcommand handler. Builds a `FyndClient`, checks solver health, loads request templates, runs the benchmark via `runner`, and prints results via `exporter`. |
| `compare.rs` | `compare` subcommand handler. Builds two `FyndClient` instances, sends identical requests sequentially to both, computes per-request metrics (amount out diff in bps, gas diff, route match), prints a summary table, and exports full results to JSON. |
| `config.rs` | Shared types: `ParallelizationMode` enum (`Sequential`, `FixedConcurrency`, `RateBased`), `BenchmarkConfig`, `BenchmarkResults`, `TimingStats`. |
| `runner.rs` | Benchmark execution engine. Implements three strategies: sequential (one-at-a-time), fixed concurrency (semaphore-bounded), and rate-based (fire at fixed intervals). Returns timing vectors and order counts. |
| `exporter.rs` | Statistics calculation (`TimingStats::from_measurements` — min/max/mean/median/p95/p99/stddev), ASCII histogram rendering, and JSON export of `BenchmarkResults`. |
| `requests.rs` | Request generation and loading. Provides a default WETH→USDC request, loads embedded aggregator trades, downloads the full 10k dataset, and loads custom requests from a JSON file. |
| `scale.rs` | `scale` subcommand handler. Resolves protocols once via `resolve_protocols` (`all_onchain`/`native_onchain` expansion), then builds and tears down an in-process Fynd instance for each worker-count iteration (applying `--min-tvl`), runs load tests via `runner`, and exports scaling results to JSON. |
| `capacity.rs` | `capacity` subcommand handler. Measures an unloaded sequential baseline, then steps a `LadderSpec` of RPS targets (rate-based traffic, discarded warm-up, `evaluate_step` verdict per step), stopping at the first failing step. Prints the `CapacityReport` JSON to stdout after a marker line and optionally to `--output-file`. |
| `capacity_report.rs` | Report types and pass/fail evaluation for `capacity`: `LadderSpec` (`start:step:max` parsing), `SloPolicy` (p95 multiplier, error/unsolved-rate thresholds), `BaselineStats`, `StepStats`/`StepOutcome`, `evaluate_step`, `CapacityReport`, and `sha256_hex` for request-set fingerprinting. |

## Data Files

- **`pairs.json`** — Token definitions for symbol lookups in request labels, embedded via `include_str!`.
- **`trades_sample.json`** — 50 real aggregator trades from Dune Analytics, embedded via `include_str!`. Used by `compare` as the default request source.
- **`requests_set.json`** — Sample request templates file. Both commands accept `--requests-file` to use custom request sets in this format.
