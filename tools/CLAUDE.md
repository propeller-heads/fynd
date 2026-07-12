# Tools

Developer and operational tooling for the Fynd solver.

| Tool | Crate | Description |
|---|---|---|
| [fynd-benchmark](#fynd-benchmark) | `tools/benchmark/` | Load testing, solver comparison, trade dataset download |
| [fynd-swap-cli](#fynd-swap-cli) | `tools/fynd-swap-cli/` | Quote and execute token swaps (ERC-20 or Permit2) |
| [record-market](#record-market) | `tools/record-market/` | Record Tycho market state and expected outputs for integration tests |

---

## fynd-benchmark

See [`tools/benchmark/CLAUDE.md`](benchmark/CLAUDE.md) for the full module overview.

Subcommands via `cargo run -p fynd-benchmark --release --`:

- **`load`** — Load-test a single solver (latency, throughput, histograms)
- **`compare`** — Compare output quality between two solver instances (amount out diff in bps)
- **`scale`** — Measure how solver throughput scales with worker thread count (in-process, no external solver needed)
- **`capacity`** — Step an RPS ladder against a solver until p95 breaches the latency SLO; reports the highest sustainable rate
- **`download-trades`** — Download the full 10k aggregator trade dataset from GitHub Releases

---

## fynd-swap-cli

End-to-end CLI for quoting and executing swaps. Supports both ERC-20 approval and Permit2 flows.

### Module Map

| File | Purpose |
|---|---|
| `main.rs` | CLI parsing (clap), quote → sign → execute flow |
| `erc20.rs` | ERC-20 helpers: balance checks, storage slot detection for dry-run overrides |
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

## record-market

Captures live Tycho `Update` messages for a configured duration, plus the chain gas price, into a
zstd-compressed `MarketRecording` fixture, then replays the recording through the full solving
pipeline (`Solver::from_recording`, `test-utils` feature) to generate `expected_outputs.json` for
the integration tests in `fynd-core/tests/integration/`.

Shared fixture types live in the `fynd-test-fixtures` crate. Worker pool configuration comes from
the production `worker_pools.toml`; its SHA-256 is stored in the recording metadata so tests can
detect drift. VM-backed protocol states (e.g. `vm:*` pools) cannot be serialized and are skipped.

See [`tools/record-market/README.md`](record-market/README.md) for usage.
