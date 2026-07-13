# hindsight

Decode solver swaps from on-chain data and live-monitor Fynd's re-solve quality against what
actually settled.

## Commands

Three subcommands via `cargo run -p hindsight --release --`:

- **`decode`** — Fetch block receipts, match solver transactions, trace each one, and emit decoded
  trades (token in/out, amounts, client, solver, gas). Accepts `--block N`, `--range START-END`
  (max 1000 blocks), or defaults to the latest block. Use `--json` for machine-readable output.

- **`verify`** — Dev sanity check: decode a block and diff against Allium's `aggregator_trades`
  ground truth to confirm the decoder isn't missing or misclassifying trades. Requires
  `ALLIUM_API_KEY` and `ALLIUM_QUERY_ID`.

- **`monitor`** — Live mode: drives an in-process `fynd-core` solver block-by-block. For each
  block it decodes settled trades, re-solves each order at top-of-block (state N-1) then
  back-of-block (state N), and emits `RangeComparison` JSONL records. Exposes a Prometheus
  metrics endpoint (`--metrics-addr`).

## Environment

| Variable | Purpose |
|---|---|
| `RPC_URL` | Ethereum JSON-RPC endpoint |
| `ALLIUM_API_KEY` | Allium API key (`verify` only) |
| `ALLIUM_QUERY_ID` | Saved Allium query ID (`verify` only) |
| `HINDSIGHT_REGISTRY` | Override path for the decoder address-book TOML |

## Architecture

### Decode pipeline (`src/decoder/`)

Match → trace → decode → guard → record.

| File/dir | Purpose |
|---|---|
| `strategy.rs` | Selects the decode strategy per matched transaction |
| `ledger.rs` | Builds a transfer ledger from logs and native ETH flows |
| `guards.rs` | Vetoes non-comparable shapes (e.g. multi-leg, maker fills) |
| `registry.rs` | Address book: maps contract addresses to venue/solver labels |
| `venues/` | Per-venue decoders (Relay, MetaMask, …) — at `src/decoder/venues/` |
| `solvers/` | Per-solver decoders + embedded-quote extraction + attribution |
| `intent.rs` | Intent/order parsing helpers |
| `trace.rs` | Transaction trace fetching and processing |

`src/verify/` contains the Allium integration:
- `allium.rs` — Allium API client for the `verify` subcommand
- `mod.rs` — Diff logic between decoded trades and Allium ground truth

Three address tiers: **venue** (order-flow owner, `tx.to`), **solver** (router that settled the
trade), **liquidity venues** (pools inside traces — not modeled here).

### Re-solve engine (`src/resolve/`)

| File | Purpose |
|---|---|
| `mod.rs` | `SteppingSolver` trait; `resolve_block_range` — solve all trades at top, advance, solve again at back |
| `compare.rs` | `Verdict` / `Deltas` — bps diff, win/loss/coverage-miss classification |
| `monitor.rs` | Production `monitor` subcommand: in-process solver, block subscription, JSONL emission |
| `jsonl.rs` | Append-only JSONL writer used by `monitor` |

### Verdict model

Each re-solved trade produces a `top` (optimistic, state N-1) and `back` (pessimistic, state N)
result. The headline `verdict` is top-of-block. Possible verdicts: `Win`, `Loss`, `Tie`,
`CoverageMiss` (Fynd filled <10% of the settled size), `Unsolvable`.

### Key types

- `DecodedTrade` — decoded on-chain trade; amounts are client-fee-adjusted so re-solve compares
  like-for-like.
- `RangeComparison` — a trade re-solved at both block states, including gas-netted settled output.
- `Outcome` — `Solved`, `Partial`, or `Unsolvable`.

## Running

```bash
# Decode the latest block
RPC_URL=... cargo run -p hindsight --release -- decode

# Decode a range
RPC_URL=... cargo run -p hindsight --release -- decode --range 21000000-21000010 --json

# Verify against Allium
RPC_URL=... ALLIUM_API_KEY=... ALLIUM_QUERY_ID=... \
  cargo run -p hindsight --release -- verify --block 21000000

# Live monitor (requires a running Tycho feed)
RPC_URL=... cargo run -p hindsight --release -- monitor --help
```
