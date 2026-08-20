# Hindsight

Decode solver swaps from on-chain data and live-monitor Fynd's re-solve quality against what
actually settled.

The idea: for every swap a competitor settled on-chain, ask "what would Fynd have returned for
the same order, at the same block state?" and compare. The answer, aggregated over time, is the
measurable value of adding Fynd to a venue.

## Subcommands

| Command | Purpose |
|---|---|
| `decode` | Decode the solver trades in a block or range and print/JSON them |
| `verify` | Diff decoded trades against Allium's `aggregator_trades` ground truth (dev check) |
| `monitor` | Live: drive an in-process Fynd solver block-by-block, re-solve every settled trade, emit JSONL + Prometheus metrics |
| `report` | Offline: render a monitor run's comparison JSONL into one HTML file |

The on-chain subcommands take `--chain` (selects the address book) and `--registry` /
`HINDSIGHT_REGISTRY` to load a custom address book. See `--help` per subcommand and
[CLAUDE.md](CLAUDE.md) for environment variables. The RPC endpoint must support
`debug_traceBlockByNumber`.

## Terminology: the three address tiers

- **Venue** — the platform the user entered through (`tx.to`): Relay, MetaMask. Owns the order
  flow, picks a solver, may take a fee. A venue is pure address-book data; no code is written
  per venue.
- **Solver** — the router that computed and settled the route: 1inch, 0x, KyberSwap. These are
  Fynd's competitors, and the only tier with code: a solver can have a `SolverDecoder`.
- **Liquidity venue** — the pools a route executes against (Uniswap, Curve). Not modeled here;
  they only appear inside traces.

## Architecture

The pipeline is solver-first: a trade's authoritative terms live in the settling solver's own
call, so decoding starts there, and the venue is attributed afterwards as a label.

### Decode pipeline (`src/decoder/`)

```
    eth_getBlockReceipts + debug_traceBlockByNumber      (two RPC calls per block)
                    │
                    ▼
           ┌─────────────────┐  keep a transaction when a known solver's frame is in its
           │      match      │  trace, its entry point is a known venue / solver / batch
           └────────┬────────┘  settler, or a known solver emitted one of its logs; skip
                    │           everything else, never decoded. A solver's veto
                    │           A bridge-order event in the logs rejects it here.
                    ▼
           ┌─────────────────┐  the settling solver's own declaration:
           │ declared decode │    calldata — find the solver frame, ask its registry entry's
           └────────┬────────┘      SolverDecoder for the declared swap and the output
                    │               recipient; anchor amount_out as the recipient's ledger
                    │               receipt (guards: the min_amount_out floor and the
                    │               plausible_quote band)
                    │             logs — a batch settler's single Trade event (CoW)
                    │
                    │ declined ──▶ ┌──────────────────┐  net the balances instead, picked by
                    │              │ netting fallback │  the entry point: venue → sender
                    │              └────────┬─────────┘  netting + fee back-out; batch settler
                    │                       │            or solver log → find the trader in the
                    │                       │            transfers; solver → sender netting.
                    │                       │            Records are marked decode: "netted".
                    ▼                       ▼
           ┌─────────────────┐  veto (reject non-trades) → venue attribution (entry point →
           │ post-processing │  owner → appData → fee wallet → integrator tag) → solver
           └────────┬────────┘  attribution (venue-declared id → entry point → trace frame →
                    │           guess) → sandwich scan
                    ▼
              DecodedTrade      carries decode: "declared" or "netted"
```

### The two decode tiers

**Declared** (`decoder/declared.rs`, `decoder/solvers/cow.rs`): the trade as the settlement's own
data states it. Calldata carries `token_in`/`token_out`/`amount_in`/`min_amount_out` (and
sometimes the solver's off-chain quote); the one field it can never carry — the settled
`amount_out` — is anchored as the gross amount the declared output recipient received in the
transfer ledger. The declared amounts are already on the solver-task basis: any venue fee left
the input before the solver frame, so no venue knowledge is needed to decode. These are the
trusted records, and the report's default scope.

**Netted** (`decoder/netting.rs`): recover the swap from what moved — ERC-20 transfers plus
native flows from the trace. Works for any solver with no parser, but a fee the ledger does not
show (or whose collector is not in the address book) sits inside the amounts. Netted records are
marked (`decode: "netted"`) and excluded from the report unless `--include-netted`.

### SolverDecoder (`src/decoder/solvers/`)

One trait per solver — everything the solver's own calldata and logs can say:

```rust
trait SolverDecoder {
    /// The swap terms in the solver call's own calldata — tokens, amounts, the on-chain floor,
    /// and (when declared) the off-chain quote and the output recipient whose receipt anchors
    /// amount_out.
    fn declared_swap(&self, input: &[u8], amount_in_hint: Option<U256>) -> Option<SwapIntent>;
    /// The frontend tag this solver records in its logs (LiFi fronts other apps).
    fn integrator(&self, logs: &[Log]) -> Option<String>;
}
```

The calldata question is one method: a solver's parse fills the whole `SwapIntent` in one pass,
including the recipient. `integrator` is the only log question left on the trait — it decodes a
string out of an event, so it needs code. Rejecting a non-swap order shape needs none: the marking
event's topic0 is address-book data (`bridge_order_events`, read by `Registry::log_veto`).

Every method defaults to "this solver's data does not carry that", so most solvers need no code
at all — one address-book line covers matching, attribution, and labels. An implementation is one
row in `solvers::IMPLEMENTATIONS`, joined onto the registry's `Solver` entry when the address
book loads; at trade time every lookup is `registry.solver(address)`, never a name search.
Today: Fly (packed calldata), KyberSwap (ABI `swap` params + `clientData` quote), 0x
(`AllowanceHolder.exec` / `Settler.execute`), ParaSwap (quote scan), LiFi (veto + integrator
tag), and CoW's `Trade`-log read keyed by the batch-settler entry.

### Why decoding is per solver, not per venue

One solver serves many venues: the same KyberSwap call settles a direct trade, a Relay trade,
and a MetaMask trade. Decoding by venue would re-implement the same read per venue — and could
give the same solver call different results depending on the wrapper. Decoding by solver reads
the call once, identically everywhere; the venue is looked up afterwards from the entry point
and the registry fingerprints.

Venue fees never change the declared amounts: the fee is charged on top whichever solver fills,
so it cancels out of the Fynd comparison. It is recorded on the trade
(`venue_fee_in`/`venue_fee_out`) for transparency, from the venue's fee collectors in the
address book. Only the netting fallback must *back fees out* of its amounts — the reason netted
records are the marked tier.

### Venue attribution (`attribution.rs`)

The venue is normally the contract the trader entered through (`tx.to`). Some order-flow venues
own the flow without being that contract, so after a flow is decoded one step can override the
venue from a registry fingerprint. Nothing in `attribution.rs` names a specific venue — it reads
four maps from the address book:

- **owning trader** (`[venue_owners]`) — the flow was read from a known venue address (kpk's
  Safes, surfaced from the CoW decode's owner).
- **CoW appData tag** (`[venue_appdata]`) — the settled order committed a frontend tag
  (`appCode`) whose appData hash maps to a venue (LlamaSwap).
- **fee wallet** (`[venue_fees]`) — a known venue fee wallet took a cut (Phantom, Robinhood).
  On a netted flow the fee is backed out of the amounts; on a declared flow it is recorded only.
  Wallets are checked in address order, so a trade cut by two venues' wallets resolves the same
  way on every run.
- **provider integrator tag** (`[venue_integrators]`) — a provider's event carried an integrator
  string mapped to a venue (LiFi frontends), read by that provider's `SolverDecoder::integrator`.

The solver label comes from its own evidence tiers, most- to least-trusted: the venue-declared
calldata id (MetaMask's `aggregatorId`, normalized via that venue's `solver_aliases`), the entry
point itself, the solver frame in the trace, the largest external call (a guess, for unknown
routers), and the entry-point label as the honest "don't know". The tier is recorded on the
record (`solver_source`).

### Per protocol, not per chain

A venue or solver deployed on several chains behaves the same everywhere, so one `SolverDecoder`
serves all of them; what differs per chain — entry points, router addresses, fee collectors,
stablecoins — lives in the per-chain address book.

A wrong sameness assumption mostly surfaces as trades failing to decode or `verify` — but not
always: a diverged fee scheme, with the fee collector missing from that chain's book, nets
trades with the fee still inside the amounts. Those records are marked netted either way, but
fee collectors are still re-verified on every chain a venue is added on.

### Where does new code go?

| You want to… | Touch | Without it |
|---|---|---|
| Track a new solver | One line in the address book's `[solvers]` section. No code — its trades match and net like any other | Trades sent directly to the solver's router never match; trades a known venue routed through it decode, but the solver is recorded as "unknown" |
| Make a solver's trades declared (trusted) instead of netted | A `SolverDecoder` impl in `solvers/` with `declared_swap`, one row in `solvers::IMPLEMENTATIONS` | The solver's trades stay netted: marked, excluded from the report by default, and missing `min_amount_out` / `declared_quote` / `quote_timestamp` |
| Skip a solver's non-swap orders | The marking event's topic0 in the address book's `bridge_order_events` | Those orders decode as trades that never happened, with absurd rates |
| Add a venue | A `[venues.<name>]` section in the address book — entry points, fee collectors, optional `solver_aliases`. No code | The venue's trades still decode when a known solver's frame or log is inside; the venue label falls back to the raw entry address, and netted amounts keep the venue's fee inside |
| Attribute a new venue (owner / appData tag / fee wallet / integrator tag) | The matching address-book map (`[venue_owners]` / `[venue_appdata]` / `[venue_fees]` / `[venue_integrators]`) | The venue's trades are attributed to the underlying router or settler, not the venue |
| Reject decodes that are not real trades (an NFT purchase's payment leg, a mis-paired wrap) | A check in `veto.rs` | Records that are not trades enter the comparison |
| Support a new chain | A `registry/<chain>.toml` address book, an entry in `registry::BUILTIN_CHAINS` | The chain has no built-in book and must be passed via `--registry` |

### Re-solve monitor (`src/resolve/`)

```
 tycho stream ──▶ in-process Fynd solver, held at block N-1
                             │
      decode block N         │  re-solve every settled trade   →  top-of-block result
                             ▼
                     advance solver to N
                             │  re-solve every trade again     →  back-of-block result
                             ▼
                      RangeComparison ──▶ JSONL records + Prometheus metrics
```

Top-of-block (state N-1) is the optimistic comparison — Fynd sees the pools before the block's
own swaps moved them; back-of-block (state N) is the pessimistic one. The headline verdict is
top-of-block: `Win`, `Loss`, `CoverageMiss` (Fynd filled under half the settled size),
`Unsolvable`, or `Sandwiched` (the settled output was moved by MEV; excluded from savings
aggregates). Watchdogs rebuild the solver when the tycho feed dies or the monitor falls too
far behind chain head (`--max-lag-blocks`).

Each solved state also records which algorithm won the quote and the route it took, rendered as
`USDT -[uniswap_v2]-> DAI -[vm:balancer]-> WETH` (a split fans into ` + `-joined legs, each with its
share). Run `monitor` with a `--worker-pools-config` that defines a pool per algorithm to turn the
algorithm field into a comparison: with one pool configured, every trade is attributed to the same
algorithm.

### The address book (`src/decoder/registry/<chain>.toml`)

All chain- and protocol-specific data — solver routers, venue entry points and fee collectors,
batch settlers, bridge-order event signatures, infrastructure contracts, USD-anchor stablecoins,
display labels — lives in a
per-chain TOML loaded by `Registry`. One book is embedded at compile time per chain — ethereum,
base, unichain, arbitrum, bsc, polygon, robinhood — and `--chain <name>` picks one. Pass
`--registry <path>` to extend or replace a book without recompiling.

The books are not uniform, because the chains are not: CoW does not settle on Unichain and LiFi is
not deployed there, so that book has no batch settlers, no LiFi solver, and no CoW-appData or
integrator venue tier. Each book's header records what was checked and what was found absent, so
an omission reads as a finding rather than an oversight.

## Running

```bash
# Decode the latest block
RPC_URL=... cargo run -p hindsight --release -- decode

# Decode a range as JSON
RPC_URL=... cargo run -p hindsight --release -- decode --range 21000000-21000010 --json

# Verify a block against Allium
RPC_URL=... ALLIUM_API_KEY=... ALLIUM_QUERY_ID=... \
  cargo run -p hindsight --release -- verify --block 21000000

# Live monitor (requires a Tycho feed)
RPC_URL=... TYCHO_URL=... cargo run -p hindsight --release -- monitor --metrics-port 9898

# Report from a monitor run, declared records only (--include-netted adds the marked tier)
cargo run -p hindsight --release -- report --comparisons-dir ./comparisons
```

The RPC endpoint must support `debug_traceBlockByNumber`.
