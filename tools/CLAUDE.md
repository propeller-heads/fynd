# Tools

Developer and operational tooling for the Fynd solver.

| Tool | Crate | Description |
|---|---|---|
| [fynd-benchmark](#fynd-benchmark) | `tools/benchmark/` | Load testing, solver comparison, scaling analysis |
| [fynd-swap-cli](#fynd-swap-cli) | `tools/fynd-swap-cli/` | Quote and execute token swaps (ERC-20 or Permit2) |

---

## fynd-benchmark

See [`tools/benchmark/CLAUDE.md`](benchmark/CLAUDE.md) for the full module overview.

Three subcommands via `cargo run -p fynd-benchmark --release --`:

- **`load`** — Load-test a single solver (latency, throughput, histograms)
- **`compare`** — Compare output quality between two solver instances (amount out diff in bps)
- **`scale`** — Measure throughput scaling across different worker counts

---

## fynd-swap-cli

End-to-end CLI for quoting and executing swaps. Supports both ERC-20 approval and Permit2 flows.

### Module Map

| File | Purpose |
|---|---|
| `main.rs` | CLI parsing (clap), solver setup (optional embedded `FyndRPCBuilder`), quote → sign → execute flow |
| `erc20.rs` | ERC-20 helpers: balance checks, approval transactions, storage slot computation for dry-run overrides |
| `permit2.rs` | Permit2 helpers: allowance checks, approval transactions, nonce fetching |

### Key Behaviors

- **Embedded solver mode**: When `--tycho-url` is provided, spawns a full `FyndRPCBuilder`
  in-process instead of connecting to an external Fynd instance at `--fynd-url`
- **Dry-run** (default): Uses `StorageOverrides` to simulate ERC-20 balance/approval via
  `eth_call`. No real funds or approvals needed
- **On-chain execution** (`--execute`): Requires `PRIVATE_KEY` env var. Checks balances/approvals,
  prompts for confirmation, submits the transaction
- **Transfer types**: `--transfer-type transfer-from` (ERC-20 approve) or
  `--transfer-type transfer-from-permit2` (off-chain signature)
