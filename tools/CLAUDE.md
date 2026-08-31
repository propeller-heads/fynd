# Tools

Developer and operational tooling for the Fynd solver.

| Tool | Crate | Description |
|---|---|---|
| [exclusive-pool-pnl](#exclusive-pool-pnl) | `tools/exclusive-pool-pnl/` | LP fee revenue and markout for the exclusive Ekubo pool |
| [fynd-benchmark](#fynd-benchmark) | `tools/benchmark/` | Load testing, solver comparison, trade dataset download |
| [fynd-swap-cli](#fynd-swap-cli) | `tools/fynd-swap-cli/` | Quote and execute token swaps (ERC-20 or Permit2) |
| [hindsight](#hindsight) | `tools/hindsight/` | Decode solver swaps from on-chain data and live-monitor re-solve quality |
| [record-market](#record-market) | `tools/record-market/` | Record Tycho market state and expected outputs for integration tests |

---

## exclusive-pool-pnl

One-shot report on the Fynd exclusive Ekubo V3 pool, hardcoded in `src/pool.rs`. Run with
`cargo run -p exclusive-pool-pnl --release --` and an `RPC_URL` pointing at an archive node.

### Module Map

| File | Purpose |
|---|---|
| `pool.rs` | Pool identity: id, extension, Ekubo core, event topics, token decimals |
| `chain.rs` | Chunked `eth_getLogs` scan, retries, and the swap / fee / signed-fee decoders |
| `prices.rs` | Binance one-minute klines used as the markout reference |
| `pnl.rs` | Fee attribution across Ekubo's one-interaction lag, plus markout math |
| `report.rs` | Per-swap table, totals block, JSON artifact |

### Key Behaviors

- **Discovery starts at the extension**, which indexes the pool id as topic1. Ekubo core emits its
  swap event with `log0` and cannot be filtered server-side, so swaps are decoded from the receipts
  of the transactions the extension points at. Position updates share the extension's topic and are
  separated by whether that receipt holds a core swap log.
- **Fees are measured from `FeesAccumulated`**, not recomputed from the signed rate. Ekubo credits
  an accrued fee on the next pool interaction, so `pnl::attribute_fees` shifts each credit back one
  interaction. A swap whose fee never reached the pool reports zero LP revenue.
- **Markout, not LVR**: no arbitrageur can trade a signature-gated pool, so the report measures
  adverse selection on Fynd's routed flow against Binance ETHUSDC.
- `price_guard` providers are deliberately unused — they are live-only and keep no history.

See [`tools/exclusive-pool-pnl/README.md`](exclusive-pool-pnl/README.md) for flags and caveats.

---

## fynd-benchmark

See [`tools/benchmark/CLAUDE.md`](benchmark/CLAUDE.md) for the full module overview.

Five subcommands via `cargo run -p fynd-benchmark --release --`:

- **`load`** — Load-test a single solver (latency, throughput, histograms)
- **`compare`** — Compare output quality between two solver instances (amount out diff in bps)
- **`scale`** — Measure how solver throughput scales with worker thread count (in-process, no external solver needed)
- **`download-trades`** — Download the full 10k aggregator trade dataset from GitHub Releases
- **`audit`** — Compare Fynd quote quality against external aggregators (Nordstern, KyberSwap, 0x); writes a JSON report

---

## fynd-swap-cli

End-to-end CLI for quoting and executing swaps. Supports both ERC-20 approval and Permit2 flows.

### Module Map

| File | Purpose |
|---|---|
| `main.rs` | CLI parsing (clap), quote → sign → execute flow |
| `permit2.rs` | Permit2 helpers: allowance checks, nonce fetching |

### Key Behaviors

- **Dry-run** (default): Uses `StorageOverrides` to simulate ERC-20 balance/approval via
  `eth_call`. No real funds or approvals needed. Uses a well-funded sender address so gas
  estimation succeeds
- **On-chain execution** (`--execute`): Requires `PRIVATE_KEY` env var. Checks
  balances/approvals and submits the transaction
- **Transfer types**: `--transfer-type transfer-from` (ERC-20 approve),
  `--transfer-type transfer-from-permit2` (off-chain signature), or
  `--transfer-type use-vaults-funds` (funds already in router vault)

See [`docs/guides/swap-cli.md`](../docs/guides/swap-cli.md) for usage instructions.

---

## hindsight

See [`tools/hindsight/CLAUDE.md`](hindsight/CLAUDE.md) for the full module overview.

Four subcommands via `cargo run -p hindsight --release --`:

- **`decode`** — Fetch a block's receipts and traces, match solver transactions, emit decoded
  trades (token in/out, amounts, venue, solver, decode tier). Accepts `--block N`,
  `--range START-END` (max 1000 blocks), or defaults to the latest block.
- **`verify`** — Diff decoded trades against Allium's `aggregator_trades` ground truth. Requires 
  `ALLIUM_API_KEY` and `ALLIUM_QUERY_ID`.
- **`monitor`** — Live mode: drives an in-process `fynd-core` solver block-by-block, solving
  each settled trade at top-of-block (N-1) and again at back-of-block (N), where the top route
  is also re-executed to measure slippage. Emits JSONL and exposes Prometheus metrics.
- **`report`** — Offline: render a self-contained HTML report from a `monitor` run's comparison
  JSONL (`--comparisons-dir`). No chain or network access.

---

## record-market

Captures live Tycho `Update` messages for a configured duration, plus the chain gas price, into a
zstd-compressed `MarketRecording` fixture, then replays the recording through the full solving
pipeline (`Solver::from_recording`, `test-utils` feature) to generate `expected_outputs.json` for
the integration tests in `fynd-core/tests/integration/`.

Shared fixture types live in the `fynd-test-fixtures` crate. Worker pool configuration comes from
the production `worker_pools.toml`; its SHA-256 is stored in the recording metadata so tests can
detect drift. VM-backed protocol states (e.g. `vm:*` components) cannot be serialized and are skipped.

See [`tools/record-market/README.md`](record-market/README.md) for usage.
