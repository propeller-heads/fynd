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

Built-in address books: `ethereum`, `base`, `unichain`, `arbitrum`, `bsc`, `polygon`. Any other
name needs `--registry`.

- **`decode`** — Fetch block receipts, match solver transactions, trace each one, and emit decoded
  trades (token in/out, amounts, venue, solver, gas, sandwich evidence). Accepts `--block N`,
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
| `decode.rs` | The `TradeDecoder` trait, the matched entity → decoders mapping, `DecodeContext`, `TraderFlow` |
| `netting_decoders.rs` | Netting toolkit (`sender_flow`, `venue_flow`) plus the `SenderNetting` decoder |
| `transfer_ledger.rs` | Builds a transfer ledger from logs and native ETH flows |
| `veto.rs` | The shared `Veto` type, plus post-decode vetoes of non-comparable shapes (NFT purchases, mis-paired wrap trades) |
| `registry.rs` | Per-chain address book, loaded from TOML (see below) |
| `sandwich.rs` | Flags trades bracketed by a front/back attacker pair (see the design spec) |
| `venues/` | Per-venue `TradeDecoder` impls (Relay, MetaMask, Rabby), listed in `venues::decoders_for`. Relay has two, tried in order: `RelayCalldata` (calldata-primary, see below) then `RelayNetting` (the fallback) |
| `solvers/` | Per-solver knowledge: embedded quotes, match-time vetoes, attribution, and swap intents (`fly.rs`'s packed-calldata parser, `kyberswap.rs`'s ABI-decoded `swap` params) recovered from a solver frame's own calldata, plus the declared output recipient (`output_recipient`) that lets `RelayCalldata` anchor the settled amount |
| `intents/` | Intent-role decoders (solver-sent, trader-not-sender): `cow.rs` reads CoW's `Trade` event, `netting.rs` is the generic net-flow finder, `decoders_for` lists them |
| `trace.rs` | Transaction trace fetching and processing |

`src/verify/` contains the Allium integration:
- `allium.rs` — Allium API client for the `verify` subcommand
- `mod.rs` — Diff logic between decoded trades and Allium ground truth

Three address tiers: **venue** (order-flow owner, `tx.to`), **solver** (router that settled the
trade), **liquidity venues** (pools inside traces — not modeled here).

### The address book (`registry/<chain>.toml`)

All chain- and protocol-specific data lives in a per-chain TOML, embedded for the six chains listed
above (`registry::BUILTIN_CHAINS`) and loadable via `--registry`. A book carries only the tiers its
chain has — Unichain has no batch settlers because CoW does not settle there, and no LiFi or
integrator tier because the Diamond is not deployed; each book's header says what was checked.
Sections: `wrapped_native`, `infrastructure` (Permit2 etc. —
addresses attribution and sandwich detection skip), `usd_stablecoins` (USD anchors for
reporting), `batch_settlers`, `[solvers]` (router address → name), `[labels]` (display-only
names), and `[venues.<name>]` (entry points, fee collectors, and — for venues that declare
their solver in calldata — `solver_aliases`).

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

### Calldata-first Relay decoding

`RelayCalldata` reads `token_in`/`token_out`/`amount_in` from the settling solver frame's own
`SwapIntent` and recovers the settled `amount_out` as the gross amount of `token_out` received by
the output recipient the same calldata declares — the one field calldata can never carry. Two
guards protect the recipient-receipt query: the recovered output must clear the intent's
`min_amount_out` floor, and any declared quote must sit within `plausible_quote`'s band of it;
either failure falls through to `RelayNetting`. The solver frame's `amount_in` needs no fee
adjustment — Relay pays its input-side fee to the collector *before* forwarding into the solver
call, so it is already the post-fee figure `amount_in` is defined to be — and the recipient's
receipt is the gross output before any output-side fee, so neither amount needs adjusting; both
fees are still recorded via `venue_fee_in`/`venue_fee_out` for transparency. See
`.claude/plans/calldata-first-decoding.md` for the empirics: on a 315-transaction Base sample,
coverage rises from 60.0% (netting alone) to 91.4% (calldata-first union), with zero divergences
across the 165 trades both paths could decode.

### Key types

- `DecodedTrade` — decoded on-chain trade; amounts are venue-fee-adjusted so re-solve compares
  like-for-like. Carries `sandwich` evidence when a bracket pair was found, and `min_amount_out`,
  `declared_quote`, and `quote_timestamp` (the calldata-declared terms copied off the settling
  solver's `SwapIntent`, when one was recovered).
- `RangeComparison` — a trade solved at top and back, including gas-netted settled output and
  the top route's `Slippage` between the two states (from its re-execution at back).
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

## Adding a venue / solver / decoder / chain

- **Solver** (a router Fynd competes with): one line in the address book's `[solvers]` section is
  enough for matching, attribution, gas isolation, and metric labels. Optional code: a
  `SolverKnowledge` impl in `solvers/` (registered in `solvers::IMPLEMENTATIONS`) with a
  `solver_veto` method if some of its orders are not same-chain swaps, or a `swap_intent` method
  if a trade's terms (tokens, amounts, on-chain floor, and — when the calldata declares one —
  the solver's off-chain quote) can be recovered from the settling solver frame's own calldata.
- **Venue** (a platform users enter through): a `[venues.<name>]` address-book section plus a
  `TradeDecoder` in `venues/`, registered in the one `venues::decoders_for` arm (its `mod`
  declaration is the only other line). Most venues are sender netting + fee back-out — call
  `netting_decoders::venue_flow` and add only what is specific to the venue. The registry fails to load if
  an address-book venue has no decoder.
- **Decoder** (a new way to read a swap — calldata decoding, log parsing): a `TradeDecoder`, with
  its extraction toolkit in `netting_decoders`/`calldata`, listed in the mapping for the entities that use
  it. Netting is one shared engine; calldata is per-router, so a calldata decoder is a standalone
  parser.
- **Chain**: a new `registry/<chain>.toml` plus its entry in `registry::BUILTIN_CHAINS`, or passed
  via `--registry`. Verify each venue's fee collector on that chain before adding it — a missing
  collector leaves the fee inside the amounts, which is a wrong record rather than a miss. Check
  the monitor's pacing flags (`--max-lag-blocks`) against the chain's block time. The `verify`
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
