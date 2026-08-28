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
                    │           everything else, never decoded.
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
                    │              └────────┬─────────┘  netting; a batch settler
                    │                       │            or solver log → find the trader in the
                    │                       │            transfers; solver → sender netting.
                    │                       │            Records are marked decode: "netted".
                    ▼                       ▼
           ┌─────────────────┐  veto (reject non-trades) → venue attribution (entry point →
           │ post-processing │  owner → appData → fee wallet → integrator tag) → solver
           └────────┬────────┘  attribution (entry point → trace frame → guess) →
                    │           sandwich scan
                    ▼
              DecodedTrade      carries decode: "declared" or "netted"
```

### The two decode tiers

**Declared** (`decoder/declared.rs`, `decoder/solvers/cow.rs`): the trade as the settlement's own
data states it. Calldata carries `token_in`/`token_out`/`amount_in`/`min_amount_out` (and
sometimes the solver's off-chain quote); the one field it can never carry — the settled
`amount_out` — is anchored as the gross amount the declared output recipient received in the
transfer ledger. No venue knowledge is needed to decode: a `[venue_fees]` wallet's cut is
corrected out afterwards, by the step that knows which decoder produced the amounts (see below).
These are the trusted records, and the report's default scope.

**Netted** (`decoder/netting.rs`): recover the swap from what moved — ERC-20 transfers plus
native flows from the trace. Works for any solver with no parser, but a fee the ledger does not
show (or whose collector is not in the address book) sits inside the amounts. Netted records are
marked (`decode: "netted"`) and excluded from the report unless `--include-netted`.

### Batch transactions are not supported

A transaction that swaps several times — an arbitrage contract routing legs through the same
router, or a settler filling several signed orders — has no single trade to record. One record
holds one swap, so recording a leg would compare Fynd against a fragment of what traded.

Both are declined rather than guessed. A transaction that enters a solver router more than once
declines its declared read (counted by `hindsight_several_legs_total`) and falls to netting, which
reads the trader's own balances and either finds one net swap or declines. A `[batch_settlers]`
settlement of several orders is declined by `CoW`'s decoder, and declined again by netting if no
single trader is found. Coverage the tool gives up, in both cases, rather than amounts it invents.

### SolverDecoder (`src/decoder/solvers/`)

One trait per solver — everything the solver's own calldata and logs can say:

```rust
trait SolverDecoder {
    /// What this solver's own data says about the transaction: nothing (Ok(None)), the trade its
    /// calldata or logs declare, or a veto — the transaction is not a same-chain swap and must
    /// not be decoded at all.
    fn declared(&self, input: &[u8], logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto>;

    /// The frontend tag this solver's data carries, for venues that share its router.
    fn venue_fingerprint(&self, input: &[u8], logs: &[Log]) -> Option<VenueTag>;
}

enum VenueTag {
    /// A frontend string the solver records in its own swap event (LiFi).
    Integrator(String),
    /// An order's committed appData hash (CoW).
    AppData(B256),
}
```

Two methods, one per question the decoder asks a solver. `declared` answers "what does this
solver say this transaction traded?", and a solver's parse fills the whole `DeclaredSwap` in one
pass, including the recipient. `venue_fingerprint` answers "does this solver's data name the
frontend that built the order?" — LiFi's integrator tag, CoW's `appData` hash, the app tag 0x
Settler carries in `zidAndAffiliate`. A solver's veto is
not a third method: it rides on `declared`'s `Err`, because a veto is a statement about the same
read.

Both default to "this solver's data does not carry that", so most solvers need no code at all —
one address-book line covers matching, attribution, and labels. An implementation is one row in
`solvers::IMPLEMENTATIONS`, joined onto the registry's `Solver` entry when the address book loads;
at trade time every lookup is `registry.solver(address)`, never a name search. That dispatch is
why no caller names a solver module: `decode_transaction` asks the settling solver's entry for a
fingerprint without knowing which solver carries one.

Today: Fly (packed calldata), KyberSwap (ABI `swap` params + `clientData` quote), 0x
(`AllowanceHolder.exec` / `Settler.execute`), ParaSwap (quote scan), 1inch (v6 `swap`
calldata), okx (`OrderRecord` log), LiFi (bridge veto + integrator tag), and CoW's `Trade`-log
read plus its `appData` tag.

### Why decoding is per solver, not per venue

One solver serves many venues: the same KyberSwap call settles a direct trade, a Relay trade,
and a MetaMask trade. Decoding by venue would re-implement the same read per venue — and could
give the same solver call different results depending on the wrapper. Decoding by solver reads
the call once, identically everywhere; the venue is looked up afterwards from the entry point
and the registry fingerprints.

### Venue fees are not modelled

A venue's fee is charged whichever solver fills the order, so it cancels out of the Fynd
comparison and hindsight does not track it.

One correction survives, and it is not about the fee itself. Fynd quotes the swap alone, so when a
known fee wallet (`[venue_fees]`) is paid out of the trade the recorded amounts have to be put back
on the swap's own basis, on whichever side the wallet was paid:

- **paid in the buy token** — the trader's receipt is short of the swap's gross output by the fee,
  so the fee is added back into `amount_out`. Without it the comparison hands Fynd the venue's cut
  as savings. Skipped when the recorded figure already contains the cut: a solver's own event
  states the gross output outright, and a receipt measured at the router that then paid the fee
  wallet is gross too. Adding it there would count the cut twice.
- **paid in the sell token** — the pools saw less than `amount_in` states, so the fee is
  subtracted. Without it Fynd is re-solved on more input than reached the pools and its larger
  output reads as savings.

Both corrections apply on either tier, declared or netted. `decode_transaction` applies them
(`apply_venue_fee`), not the venue label search: it is the one step that knows which decoder
produced the amounts, and running it there means the correction does not depend on which
fingerprint happened to name the venue. A fee paid to any other address stays inside the amounts,
which is part of what the netted marker warns about.

### Venue attribution (`attribution.rs`)

The venue is normally the contract the trader entered through (`tx.to`). Some order-flow venues
own the flow without being that contract, so after a flow is decoded one step can override the
venue from a registry fingerprint. Nothing in `attribution.rs` names a specific venue — it reads
four maps from the address book:

- **owning trader** (`[venue_owners]`) — the flow was read from a known venue address (kpk's
  Safes, surfaced from the CoW decode's owner).
- **CoW appData tag** (`[venue_appdata]`) — the settled order committed a frontend tag
  (`appCode`) whose appData hash maps to a venue (LlamaSwap).
- **fee wallet** (`[venue_fees]`) — a known venue fee wallet took a cut (Phantom, Robinhood,
  Coinbase's Base App). `venue_fee` reports the cut and the caller corrects it out of the amounts
  (see above); every function in `attribution.rs` only reads. Wallets are checked in address
  order, so a trade cut by two venues' wallets resolves the same way on every run.
- **provider integrator tag** (`[venue_integrators]`) — the provider's own data carried a tag
  mapped to a venue, read through `SolverDecoder::venue_fingerprint`: an integrator string in a
  LiFi event, or the hex app tag a frontend writes into 0x Settler's `zidAndAffiliate` word
  (Matcha Meta).

The solver label comes from its own evidence tiers, most- to least-trusted: the entry point
itself, the outermost solver frame in the trace, the largest external call (a guess, for unknown
routers), and the entry-point label as the honest "don't know". The tier is recorded on the record
(`solver_source`). A venue's own claim about which solver it routed to is not consulted — the
router that settled the trade is in the trace, which is the harder fact, and the `solver_aliases`
tables the claim was read from are gone from the address books.

### Per protocol, not per chain

A venue or solver deployed on several chains behaves the same everywhere, so one `SolverDecoder`
serves all of them; what differs per chain — entry points, router addresses, stablecoins — lives
in the per-chain address book.

A wrong sameness assumption surfaces as trades failing to decode, or as `verify` reporting gaps
against Allium.

### Where does new code go?

| You want to… | Touch | Without it |
|---|---|---|
| Track a new solver | One line in the address book's `[solvers]` section. No code — its trades match and net like any other | Trades sent directly to the solver's router never match; trades a known venue routed through it decode, but the solver is recorded as "unknown" |
| Make a solver's trades declared (trusted) instead of netted | A `SolverDecoder` impl in `solvers/` with `declared`, one row in `solvers::IMPLEMENTATIONS` | The solver's trades stay netted: marked, excluded from the report by default, and missing `min_amount_out` / `declared_quote` / `quote_timestamp` |
| Skip a solver's non-swap orders | An `Err(Veto)` from its `declared`, off a log its own module reads | Those orders decode as trades that never happened, with absurd rates |
| Add a venue | A `[venues.<name>]` section in the address book — its entry points. No code | The venue's trades still decode when a known solver's frame or log is inside, but the venue label falls back to the raw entry address |
| Attribute a new venue (owner / appData tag / fee wallet / integrator tag) | The matching address-book map (`[venue_owners]` / `[venue_appdata]` / `[venue_fees]` / `[venue_integrators]`) | The venue's trades are attributed to the underlying router or settler, not the venue |
| Read a tag from a *new* solver's own data | A `venue_fingerprint` on its `SolverDecoder`, returning the `VenueTag` variant its data carries | The tag is never read, so venues sharing that solver's router fall back to its label |
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

All chain- and protocol-specific data — solver routers, venue entry points, batch settlers,
infrastructure contracts, USD-anchor stablecoins, display labels — lives in a per-chain TOML
loaded by `Registry`. One book is embedded at compile time per chain — ethereum,
base, unichain, arbitrum, bsc, polygon, robinhood — and `--chain <name>` picks one. Pass `--registry <path>`
to extend or replace a book without recompiling.

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
