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

## record-market

Records the raw Tycho feed messages for a configured duration, plus the token list and the chain
gas price, into a zstd-compressed `MarketRecording` fixture. It then replays the recording through
the full solving pipeline (`Solver::from_recording`, `test-utils` feature) to generate
`expected_outputs.json` for the integration tests in `fynd-core/tests/integration/`.

Shared fixture types live in the `fynd-test-fixtures` crate; the `MarketRecording` module doc says
why a recording stores messages and how replay decodes them. Expected outputs are solved with the
integration tests' own pool config, `fynd-core/tests/integration/worker_pools.toml`, so the
baseline matches what the tests run; its SHA-256 is stored in the recording metadata.

See [`tools/record-market/README.md`](record-market/README.md) for usage.
