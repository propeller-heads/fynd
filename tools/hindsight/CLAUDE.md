# hindsight

Decode solver swaps from on-chain data and live-monitor Fynd's re-solve quality against what
actually settled.

[README.md](README.md) holds the architecture overview with pipeline diagrams, the
"where does new code go" table, and the strategy-vs-venue-knowledge placement rules.

## Commands

Three subcommands via `cargo run -p hindsight --release --`. All of them take `--chain`
(default `ethereum`), which selects the decoder's address book, and `--registry <path>` /
`HINDSIGHT_REGISTRY` to load a custom address book.

- **`decode`** — Fetch block receipts, match solver transactions, trace each one, and emit decoded
  trades (token in/out, amounts, venue, solver, gas, sandwich evidence). Accepts `--block N`,
  `--range START-END` (max 1000 blocks), or defaults to the latest block. Use `--json` for
  machine-readable output.

- **`verify`** — Dev sanity check: decode a block and diff against Allium's `aggregator_trades`
  ground truth to confirm the decoder isn't missing or misclassifying trades. Requires
  `ALLIUM_API_KEY` and `ALLIUM_QUERY_ID`.

- **`monitor`** — Live mode: drives an in-process `fynd-core` solver block-by-block. For each
  block it decodes settled trades, re-solves each order at top-of-block (state N-1) then
  back-of-block (state N), and emits `RangeComparison` JSONL records. Exposes a Prometheus
  metrics endpoint (`--metrics-port`). `--max-lag-blocks` (default 100, ~20 min on mainnet)
  bounds how far it may fall behind chain head before rebuilding the solver.

## Environment

| Variable | Purpose |
|---|---|
| `RPC_URL` | Chain JSON-RPC endpoint (must support `debug_traceTransaction`) |
| `ALLIUM_API_KEY` | Allium API key (`verify` only) |
| `ALLIUM_QUERY_ID` | Saved Allium query ID (`verify` only) |
| `HINDSIGHT_REGISTRY` | Override path for the decoder address-book TOML |

## Architecture

### Decode pipeline (`src/decoder/`)

Match → trace → decode → veto → record.

| File/dir | Purpose |
|---|---|
| `matching.rs` | Receipt-only filter: is this transaction a solver trade at all, plus match-time vetoes |
| `strategies/` | Decode methods behind the `DecodeStrategy` trait, tried in precedence order (`netting` today) |
| `transfer_ledger.rs` | Builds a transfer ledger from logs and native ETH flows |
| `veto.rs` | The shared `Veto` type, plus post-decode vetoes of non-comparable shapes (NFT purchases, mis-paired wrap trades) |
| `registry.rs` | Per-chain address book, loaded from TOML (see below) |
| `sandwich.rs` | Flags trades bracketed by a front/back attacker pair (see the design spec) |
| `venues/` | Per-venue decoders (Relay, MetaMask), called through one `VenueContext` seam |
| `solvers/` | Per-solver knowledge: embedded quotes, match-time vetoes, attribution |
| `maker.rs` | Maker-finding for intent fills and batch settlements |
| `trace.rs` | Transaction trace fetching and processing |

`src/verify/` contains the Allium integration:
- `allium.rs` — Allium API client for the `verify` subcommand
- `mod.rs` — Diff logic between decoded trades and Allium ground truth

Three address tiers: **venue** (order-flow owner, `tx.to`), **solver** (router that settled the
trade), **liquidity venues** (pools inside traces — not modeled here).

### The address book (`registry/<chain>.toml`)

All chain- and protocol-specific data lives in a per-chain TOML, embedded for ethereum and
loadable via `--registry`. Sections: `wrapped_native`, `infrastructure` (Permit2 etc. —
addresses attribution and sandwich detection skip), `usd_stablecoins` (USD anchors for
reporting), `batch_settlers`, `[solvers]` (router address → name), `[labels]` (display-only
names), and `[venues.<name>]` (entry points, fee collectors, and — for venues that declare
their solver in calldata — `solver_aliases`).

### Re-solve engine (`src/resolve/`)

| File | Purpose |
|---|---|
| `mod.rs` | `SteppingSolver` trait; `resolve_block_range` — solve all trades at top, advance, solve again at back |
| `compare.rs` | `Verdict` / `Deltas` — bps diff, win/loss/coverage-miss classification |
| `monitor.rs` | Production `monitor` subcommand: in-process solver, block subscription, JSONL emission |
| `jsonl.rs` | Append-only JSONL writer used by `monitor` |

### Verdict model

Each re-solved trade produces a `top` (optimistic, state N-1) and `back` (pessimistic, state N)
result. The headline `verdict` is top-of-block. Possible verdicts: `Win`, `Loss`,
`CoverageMiss` (Fynd filled <50% of the settled size — `MIN_FILL_RATIO = 0.5`), `Unsolvable`,
and `Sandwiched` (a solved comparison whose settled output was moved by MEV — excluded from the
savings aggregates; unsolved states keep their coverage verdicts).

### Key types

- `DecodedTrade` — decoded on-chain trade; amounts are venue-fee-adjusted so re-solve compares
  like-for-like. Carries `sandwich` evidence when a bracket pair was found.
- `RangeComparison` — a trade re-solved at both block states, including gas-netted settled output.
- `Outcome` — `Solved`, `Partial`, or `Unsolvable`.

## Adding a venue / solver / strategy / chain

- **Solver** (a router Fynd competes with): one line in the address book's `[solvers]` section is
  enough for matching, attribution, gas isolation, and metric labels. Optional code: a
  `SolverKnowledge` impl in `solvers/` (registered in `solvers::IMPLEMENTATIONS`) with an
  `embedded_quote` method if its calldata declares an off-chain quote, or a `solver_veto` method
  if some of its orders are not same-chain swaps.
- **Venue** (a platform users enter through): a `[venues.<name>]` address-book section plus a
  `VenueKnowledge` impl in `venues/` and its `venues::from_name` binding. Most venues are sender
  netting + fee back-out — delegate to `venue_fee_flow` and add only what is specific to the
  venue. The registry fails to load if an address-book section has no code binding. All of a
  venue's knowledge — transfer-based corrections and calldata parsing alike — lives in its one
  `venues/` module (see README.md).
- **Decode strategy** (a new method for extracting swaps — calldata decoding, log parsing): a
  module in `strategies/` implementing `DecodeStrategy`, plus one entry in `default_strategies`
  placed by trust. Strategies are methods, never venues or solvers; a venue-scoped method still
  keeps its venue parsing in that venue's module.
- **Chain**: a new `registry/<chain>.toml` (all sections required) wired into `Registry::load`,
  or passed via `--registry`. Check the monitor's pacing flags (`--max-lag-blocks`) against the
  chain's block time. The `verify` subcommand's saved Allium query is per-chain.

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
