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
  `--propamm-pair <IN,OUT>` additionally injects a mock PropAMM pool (see below).

- **`report`** — Offline: read the `comparisons-YYYY-MM-DD.jsonl` files a `monitor` run wrote
  (`--comparisons-dir`) and render a single self-contained HTML file (`-o`, defaults to
  `<dir>/report.html`) with the dashboard's value views — the headline Fynd savings, win rate, and
  median savings bps; the verdict split by trade count and by volume; per-solver/venue breakdowns;
  top-saving trades; and the unsolved token tail. `--venue <name>` (repeatable, case-insensitive)
  restricts the report to those venues. No chain, Tycho, or network access. Records from a
  `--propamm-pair` run also get a "Mock PropAMM" section (winrate, captured flow, fee headroom, and
  a per-order-pair breakdown).

## Environment

| Variable | Purpose |
|---|---|
| `RPC_URL` | Chain JSON-RPC endpoint (must support `debug_traceTransaction`) |
| `ALLIUM_API_KEY` | Allium API key (`verify` only) |
| `ALLIUM_QUERY_ID` | Saved Allium query ID (`verify` only) |
| `HINDSIGHT_REGISTRY` | Override path for the decoder address-book TOML |
| `PROPAMM_PAIR` | Token pair the mock PropAMM mirrors, comma-separated (`monitor` only) |
| `PROPAMM_OFFSETS_BPS` | Price offsets in bps off the public best route, e.g. `-5,0,5` |
| `PROPAMM_PROBE_UNITS` | Trade size used to pick which real pool the mock mirrors |

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
| `venues/` | Per-venue `TradeDecoder` impls (Relay, MetaMask, Rabby), listed in `venues::decoders_for` |
| `solvers/` | Per-solver knowledge: embedded quotes, match-time vetoes, attribution |
| `intents/` | Intent-role decoders (solver-sent, trader-not-sender): `cow.rs` reads CoW's `Trade` event, `netting.rs` is the generic net-flow finder, `decoders_for` lists them |
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

### Report (`src/report/`)

The offline `report` subcommand — reads a `monitor` run's comparison JSONL and writes one
self-contained HTML file.

| File | Purpose |
|---|---|
| `mod.rs` | `ReportArgs`; reads every `.jsonl` in the dir, skipping malformed lines |
| `record.rs` | The subset of the JSONL record the report deserializes; round-trip tested against `jsonl::write_comparisons` |
| `aggregate.rs` | Pure aggregations over the records (verdicts, coverage, savings, per-group, movers) |
| `html.rs` | Renders the aggregates to a self-contained HTML file (inline CSS, `<div>` bars, no assets) |

### Mock PropAMM (`src/propamm/`)

Test scaffolding for ENG-6157 — sizes what a dynamic-underbidding PropAMM pool would win before the
pool exists. Off unless `monitor --propamm-pair` is set.

| File | Purpose |
|---|---|
| `mod.rs` | `Injector` — writes a synthetic exclusive component into the running solver's `MarketState` once per block and announces it on the market-event channel |
| `mirror.rs` | `MirrorPool` — a `ProtocolSim` that delegates to the best real pool for the pair and scales its price by `--propamm-price-pct`, charging no fee |
| `report.rs` | Per-order outcomes and run totals; `Record` is what lands in the comparisons JSONL |

Each order on the mirrored pair is solved **twice**: once with the mock neutralised (scaled to one
part per million, so it stays in the graph but loses every comparison), which yields the public best
route Fynd would otherwise have quoted; then again with the mock rescaled so its output lands
`--propamm-offsets-bps` off that number. So an offset means "this much better than the route Fynd
would have quoted", not "this much better than some single pool".

That makes every calibrated order an assertion. The report groups them by offset and judges each
group against the behaviour its price implies:

| offset | expectation |
|---|---|
| below market | never selected — the router requires a strict beat |
| at market | can only win on gas, and then there is no surplus, so the fee must be zero |
| above market | the fee taken cannot exceed the offset, since the offset is all the surplus there is |

A group with no selections above market is reported as *no data*, not a failure: winning there
depends on gas as well as price. Orders off the mirrored pair carry no offset and form no group —
calibration is only exact when the mock serves the whole order in one hop.

Fynd's existing exclusive-access routing does the rest: `FyndBuilder::exclusivity_policy` hides the
mock from every configured worker pool, and each pool is twinned with a `liquidity_scope = "all"`
copy that sees it. Because the router pins a surplus quote's `amount_out` to the public commitment,
the mock never changes hindsight's own win/loss verdicts — it only adds this second measurement.

Requires `fynd-core`'s `experimental` feature for `Solver::market_event_sender`. Not for production:
the mock prices a pool that does not exist on chain, so any calldata it produces is unexecutable.

Two things to get right when running it:

- **Set `EXCLUSIVE_SWAP_CONTROLLER_KEY`** (any throwaway key — nothing is executed). The encoder
  fails fast on an exclusive leg with no signer, which turns every win into a failed quote and
  silently reports a zero winrate.
- **Pick a pair that has flow.** Only orders whose own pair is the mirrored one get calibrated, and
  those are a small slice of settled flow — 40 mainnet blocks yielded 9 calibrated orders on
  ETH/USDT. On ethereum, ETH/USDC carries roughly twice ETH/USDT's volume; check a run's per-pair
  table before committing to a long one. Filling the groups is a matter of blocks, so budget for it.
- **`--min-tvl 10`** is mandatory against `tycho-fynd-ethereum`, which rejects any other value with
  `tvl_gt must be == 10`.

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

## Adding a venue / solver / decoder / chain

- **Solver** (a router Fynd competes with): one line in the address book's `[solvers]` section is
  enough for matching, attribution, gas isolation, and metric labels. Optional code: a
  `SolverKnowledge` impl in `solvers/` (registered in `solvers::IMPLEMENTATIONS`) with an
  `embedded_quote` method if its calldata declares an off-chain quote, or a `solver_veto` method
  if some of its orders are not same-chain swaps.
- **Venue** (a platform users enter through): a `[venues.<name>]` address-book section plus a
  `TradeDecoder` in `venues/`, registered in the one `venues::decoders_for` arm (its `mod`
  declaration is the only other line). Most venues are sender netting + fee back-out — call
  `netting_decoders::venue_flow` and add only what is specific to the venue. The registry fails to load if
  an address-book venue has no decoder.
- **Decoder** (a new way to read a swap — calldata decoding, log parsing): a `TradeDecoder`, with
  its extraction toolkit in `netting_decoders`/`calldata`, listed in the mapping for the entities that use
  it. Netting is one shared engine; calldata is per-router, so a calldata decoder is a standalone
  parser.
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
