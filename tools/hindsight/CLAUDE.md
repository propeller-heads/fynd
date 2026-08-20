# hindsight

Decode solver swaps from on-chain data and live-monitor Fynd's re-solve quality against what
actually settled.

[README.md](README.md) holds the architecture overview with pipeline diagrams, the
"where does new code go" table, and the strategy-vs-venue-knowledge placement rules.

## Commands

Four subcommands via `cargo run -p hindsight --release --`. The on-chain ones (`decode`,
`verify`, `monitor`) take `--chain` (default `ethereum`), which selects the decoder's address book,
and `--registry <path>` / `HINDSIGHT_REGISTRY` to load a custom address book. `report` is offline
and takes neither.

Built-in address books: `ethereum`, `base`, `unichain`, `arbitrum`, `bsc`, `polygon`, `robinhood`.
Any other name needs `--registry`.

- **`decode`** — Fetch a block's receipts and traces (two calls), match, and emit decoded
  trades (token in/out, amounts, venue, solver, sandwich evidence). Accepts `--block N`,
  `--range START-END` (max 1000 blocks), or defaults to the latest block. Use `--json` for
  machine-readable output.

- **`verify`** — Dev sanity check: decode a block and diff against Allium's `aggregator_trades`
  ground truth to confirm the decoder isn't missing or misclassifying trades. Requires
  `ALLIUM_API_KEY` and `ALLIUM_QUERY_ID`.

- **`monitor`** — Live mode: drives an in-process `fynd-core` solver block-by-block. For each
  block it decodes settled trades, solves each order at top-of-block (state N-1), then measures
  twice at back-of-block (state N): the top route is re-executed via `fynd_core::replay_route`
  to measure slippage between quote time and execution time, and the order is solved fresh to
  show what routing at the block's end state would deliver. Emits `RangeComparison` JSONL
  records. Exposes a Prometheus metrics endpoint (`--metrics-port`). `--max-lag-blocks` (default
  100, ~20 min on mainnet) bounds how far it may fall behind chain head before rebuilding the
  solver.

- **`report`** — Offline: read the `comparisons-YYYY-MM-DD.jsonl` files a `monitor` run wrote
  (`--comparisons-dir`) and render a single self-contained HTML file (`-o`, defaults to
  `<dir>/report.html`) with the dashboard's value views — the headline Fynd savings, win rate, and
  median savings bps; the verdict split by trade count and by volume; per-solver/venue breakdowns;
  top-saving trades; and the unsolved token tail. `--venue <name>` (repeatable, case-insensitive)
  restricts the report to those venues. No chain, Tycho, or network access.

## Environment

| Variable | Purpose |
|---|---|
| `RPC_URL` | Chain JSON-RPC endpoint (must support `debug_traceBlockByNumber`) |
| `ALLIUM_API_KEY` | Allium API key (`verify` only) |
| `ALLIUM_QUERY_ID` | Saved Allium query ID (`verify` only) |
| `HINDSIGHT_REGISTRY` | Override path for the decoder address-book TOML |

## Architecture

### Decode pipeline (`src/decoder/`)

Three steps per block: trace the whole block → decode each transaction from the solver's side →
attribute. [README.md](README.md) holds the full pipeline diagram and the two-tier decode model.

| File/dir | Purpose |
|---|---|
| `mod.rs` | The orchestrator: fetch receipts + block trace, match, decode (declared first, netting fallback), veto, attribute |
| `declared.rs` | The declared decode: the settling solver frame's own calldata, anchored by the recipient's ledger receipt |
| `netting.rs` | The netting fallback (marked `decode: "netted"`): the engine plus the venue/sender/intent arms picked by the entry point |
| `attribution.rs` | Solver attribution tiers and venue fingerprints (owner, appData, fee wallet in address order, integrator tag) |
| `transfer_ledger.rs` | Builds a transfer ledger from logs and native ETH flows |
| `veto.rs` | The shared `Veto` type, plus post-decode vetoes of non-comparable shapes (NFT purchases, mis-paired wrap trades, fee-on-transfer skims) |
| `registry.rs` | Per-chain address book, loaded from TOML; joins each solver to its `SolverDecoder` at load |
| `sandwich.rs` | Flags trades bracketed by a front/back attacker pair (see the design spec) |
| `solvers/` | One `SolverDecoder` per solver with code: declared swaps (`fly.rs` packed calldata, `kyberswap.rs` ABI `swap` params, `zeroex.rs` `AllowanceHolder.exec`/`Settler.execute`), vetoes and integrator tags (`lifi.rs`), and `cow.rs`'s `Trade`-log read for batch settlements |
| `trace.rs` | Whole-block trace fetching (`debug_traceBlockByNumber`) and frame walks |

`src/verify/` contains the Allium integration:
- `allium.rs` — Allium API client for the `verify` subcommand
- `mod.rs` — Diff logic between decoded trades and Allium ground truth

Three address tiers: **venue** (order-flow owner, `tx.to` — pure data), **solver** (router that
settled the trade — the only tier with code), **liquidity venues** (pools inside traces — not
modeled here).

### The two decode tiers

Every record carries `decode: "declared" | "netted"`. Declared records (solver calldata or a
batch settler's `Trade` log) are the trusted tier and the report's default scope. Netted records
(balance netting) can hide an unaccounted fee inside the amounts; the report excludes them unless
`--include-netted`.

### The address book (`registry/<chain>.toml`)

All chain- and protocol-specific data lives in a per-chain TOML, embedded for the seven chains
listed above (`registry::BUILTIN_CHAINS`) and loadable via `--registry`. A book carries only the
tiers its chain has; each book's header says what was checked. An address also moves per chain:
Robinhood Chain's LiFi Diamond, 0x Settler, OKX router and MetaMask router all sit at
chain-specific addresses, re-derived rather than copied. Sections: `wrapped_native`,
`infrastructure` (Permit2 etc.), `usd_stablecoins` (USD anchors for reporting), `batch_settlers`,
`bridge_order_events` (topic0s marking a transaction as not a same-chain swap),
`[solvers]` (router address → name; the name joins to a `SolverDecoder` at load), `[labels]`
(display-only names), `[venues.<name>]` (entry points and fee collectors), and the venue
fingerprints
(`[venue_owners]`, `[venue_fees]`, `[venue_integrators]`, `[venue_appdata]`).

### Re-solve engine (`src/resolve/`)

| File | Purpose |
|---|---|
| `mod.rs` | `SteppingSolver` trait; `resolve_block_range` — solve all trades at top, advance, then re-execute each top route and solve each trade fresh at back |
| `compare.rs` | `Verdict` / `Deltas` / `Slippage` — bps diff, win/loss/coverage-miss classification, quote-vs-re-execution slippage |
| `monitor.rs` | Production `monitor` subcommand: in-process solver, block subscription, JSONL emission |
| `jsonl.rs` | Append-only JSONL writer used by `monitor` |

### Report (`src/report/`)

The offline `report` subcommand — reads a `monitor` run's comparison JSONL and writes one
self-contained HTML file.

| File | Purpose |
|---|---|
| `mod.rs` | `ReportArgs`; reads every `.jsonl` in the dir, skipping malformed lines |
| `record.rs` | The subset of the JSONL record the report deserializes; round-trip tested against `jsonl::write_comparisons` |
| `aggregate.rs` | Pure aggregations over the records (verdicts, coverage, savings, per-group, movers) |
| `html.rs` | Renders the aggregates to a self-contained HTML file (inline CSS, `<div>` bars, no assets) |

### Verdict model

Each trade produces a `top` result (optimistic, solved fresh at state N-1) and a `back` result
(pessimistic: solved fresh at state N, after the block's swaps moved the pools). The headline
`verdict` is top-of-block. Possible verdicts: `Win`, `Loss`, `CoverageMiss` (Fynd
filled <50% of the settled size — `MIN_FILL_RATIO = 0.5`), `Unsolvable`, and `Sandwiched` (a
solved comparison whose settled output was moved by MEV — excluded from the savings aggregates;
unsolved states keep their coverage verdicts).

### Slippage model

`Slippage` measures how the top route's output moved between quote time (N-1) and execution time
(N): re-executed output vs quoted output, signed, in bps (JSONL also carries USD valued at the
back-of-block price snapshot). Positive slippage is the surplus we would keep if we charged it —
Prometheus records the signed bps distribution (`hindsight_slippage_bps`) and the signed USD
value (`hindsight_slippage_usd`), both labeled by the trade's headline verdict, plus a
positive-only USD histogram (`hindsight_positive_slippage_usd`) whose sum is the running
hypothetical revenue. Absent when the top was unsolved or the re-execution failed (e.g. a pool
vanished at N).

### The declared decode

`declared_flow` reads `token_in`/`token_out`/`amount_in` from the settling solver frame's own
`SwapIntent` and recovers the settled `amount_out` as the gross amount of `token_out` received by
the output recipient the same calldata declares (falling back to the transaction sender) — the
one field calldata can never carry. Two guards protect the recipient-receipt query: the recovered
output must clear the intent's `min_amount_out` floor, and any declared quote must sit within
`plausible_quote`'s band of it; either failure falls through to the netting fallback. The
declared amounts are already on the solver-task basis — a venue's input-side fee left before the
solver frame, and the recipient's receipt is the gross output — so venue fees are recorded via
`venue_fee_in`/`venue_fee_out` for transparency without adjusting the amounts. See
`.claude/plans/calldata-first-decoding.md` for the empirics behind calldata-first ordering: on a
315-transaction Base sample, coverage rises from 60.0% (netting alone) to 91.4% (calldata-first
union), with zero divergences across the 165 trades both paths could decode.

### Key types

- `DecodedTrade` — decoded on-chain trade; amounts are venue-fee-adjusted so re-solve compares
  like-for-like. Carries `sandwich` evidence when a bracket pair was found, and `min_amount_out`,
  `declared_quote`, and `quote_timestamp` (the calldata-declared terms copied off the settling
  solver's `SwapIntent`, when one was recovered).
- `RangeComparison` — a trade solved at top and back, plus the top route's `Slippage` between
  the two states (from its re-execution at back). All comparisons are gross of gas.
- `Outcome` — `Solved`, `Partial`, or `Unsolvable`.
- `SolvedAmount` — a solved state's amounts plus `algorithm` (which worker pool won the quote) and
  `solved_route` (the full `fynd_core::types::Route`, kept in memory to replay at back-of-block).
  The readable path is not stored: `resolve::render_route` derives it from `solved_route` at
  serialization/log time, reading token symbols off the route's own token map via
  `Route::token_symbol`.

### Route attribution

A solved state records the algorithm whose route won the quote — the worker pool that beat the
others on that order — and, derived from its route at serialization time, that route rendered as
a readable path:

```
USDT -[uniswap_v2]-> DAI -[vm:balancer]-> WETH
```

Protocol ids are Tycho's own, so a newly integrated DEX reads correctly with no lookup table here.
A token the solver has no entry for falls back to a shortened address (`0xababab…`).

A **split** fans several legs out of one token, so its legs cannot share a single arrow chain. Each
becomes its own path, joined by ` + `, and every leg carries its share of the input. `Route`'s split
convention declares an explicit fraction on each leg but the last, which declares `0.0` meaning
"all the remaining balance"; `split_shares` reconstructs that remainder so both legs read as a
percentage. A split that reconverges still chains its continuation onto the leg it belongs to:

```
USDC -[uniswap_v3 60%]-> WETH + USDC -[vm:curve 40%]-> WETH
USDT -[uniswap_v3 25%]-> WETH + USDT -[vm:curve 75%]-> DAI -[uniswap_v2]-> USDC
```

It surfaces three ways:

- **Prometheus**: an `algorithm` label on `hindsight_trades_total`, `hindsight_savings_bps`,
  `hindsight_savings_usd`, and `hindsight_improvement_usd`. Split any of them by venue to see
  which algorithm serves that venue's flow best. Unsolved states carry `algorithm="none"`. The
  path is deliberately *not* a label — it is per-trade and would explode series cardinality.
- **Loki**: `algorithm` and `route` on the `trade comparison` line. `route` is the **last** field
  on purpose: its value contains spaces, so a LogQL regexp can only bound it by end-of-line. Move
  it and the dashboard's route column silently swallows every field after it.
- **JSONL**: flat `algorithm` and `route` per state, next to the nested per-hop route (which keeps
  the pools and amounts the string leaves out).

## Adding a solver / venue / chain

- **Solver** (a router Fynd competes with): one line in the address book's `[solvers]` section
  covers matching, attribution, and metric labels. To make its trades declared (trusted) instead
  of netted: a `SolverDecoder` impl in `solvers/` with a `declared_swap` method, registered as one
  row in `solvers::IMPLEMENTATIONS`. One parse fills the whole `SwapIntent`, including the output
  recipient when the calldata declares one. If some of its orders are not same-chain swaps, add
  the marking event's topic0 to the address book's `bridge_order_events` — no code.
- **Venue** (a platform users enter through): a `[venues.<name>]` address-book section — entry
  points and fee collectors. No code. Verify each fee collector on-chain before adding it: a missing
  collector leaves the fee inside the netted amounts (declared amounts are immune).
- **Chain**: a new `registry/<chain>.toml` plus its entry in `registry::BUILTIN_CHAINS`, or
  passed via `--registry`. Re-verify each venue's fee collectors on that chain. Check the
  monitor's pacing flags (`--max-lag-blocks`) against the chain's block time. The `verify`
  subcommand's saved Allium query is per-chain.

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
