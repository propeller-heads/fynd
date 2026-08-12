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

All take `--chain` (selects the address book) and `--registry` /
`HINDSIGHT_REGISTRY` to load a custom address book. See `--help` per subcommand and
[CLAUDE.md](CLAUDE.md) for environment variables.

## Terminology: the three address tiers

- **Venue** — the platform the user entered through (`tx.to`): Relay, MetaMask. Owns the order
  flow, picks a solver, may take a fee.
- **Solver** — the router that computed and settled the route: 1inch, 0x, KyberSwap. These are
  Fynd's competitors.
- **Liquidity venue** — the pools a route executes against (Uniswap, Curve). Not modeled here;
  they only appear inside traces.

## Architecture

### Decode pipeline (`src/decoder/`)

```
    eth_getBlockReceipts (one call per block)
                    │
                    ▼
           ┌─────────────────┐   solver_veto    ┌─────────────────┐
           │    matching     │ ───────────────▶ │ SolverKnowledge │
           └────────┬────────┘  skips non-swaps └─────────────────┘
                    │  matched on tx.to or a solver log, then debug_traceTransaction
                    ▼
           ┌─────────────────┐
           │ gather evidence │   receipt + logs, trace, root calldata, transfer
           └────────┬────────┘   ledger → one DecodeContext
                    │
                    ▼
           ┌─────────────────┐   the matched entity holds an ordered list of prebuilt decoders,
           │     decode      │   each tried until one returns a TraderFlow (an entity may
           └────────┬────────┘   list several — a richer source first, a general one as fallback):
                    │
                    │   direct solver           →  [ SenderNetting ]
                    │   batch settler / solver  →  [ CowSettlement, IntentNetting ]
                    │   venue relay             →  [ RelayCalldata, RelayNetting ]
                    │   venue metamask          →  [ MetaMaskNetting ]
                    │  TraderFlow
                    ▼
           ┌─────────────────┐   swap_intent    ┌─────────────────┐
           │ post-processing │ ───────────────▶ │ SolverKnowledge │
           └────────┬────────┘                  └─────────────────┘
                    │  veto → venue attribution → solver attribution → gas → intent → sandwich scan
                    ▼
              DecodedTrade
```

Horizontal arrows are consultations: the stage calls the `SolverKnowledge` method on a resolved
handle — solver attribution resolves the name to its trait object once, and every later
consultation calls the trait on it. The stages themselves are protocol-agnostic.

### Decoders

A `TradeDecoder` turns one matched, traced transaction into the trader's flow. *How* it reads the
swap is open — the trait fixes only the input and the output, never the method. A decoder might
read the value movements, the calldata, the protocol's event logs, some combination of those, or
a source we have not needed yet; netting is simply the one that exists today.

```rust
trait TradeDecoder {
    fn name(&self) -> &'static str;
    /// The trader's flow, or `None` when this decoder cannot read the transaction.
    async fn decode(&self, ctx: &mut DecodeContext) -> Option<TraderFlow>;
}
```

Every decoder is constructed once, with the state it needs. A venue's decoders are built when
the registry loads — each holding its venue's addresses — and live on the venue's registry
entry; the sender and intent lists are built once per `Decoder`. The per-transaction path
resolves the entry point (an `Address`) to its entity and calls trait objects; no name is
converted to code per transaction.

```rust
// venues/mod.rs — the one name → code binding: a registration table, consulted once at load.
// An address-book venue with no row here fails the registry load.
const DECODERS: &[(&str, Constructor)] = &[
    ("relay", relay::decoders),        // [RelayCalldata, RelayNetting]
    ("metamask", metamask::decoders),  // [MetaMaskNetting]
    ...
];

// registry.rs — each loaded venue carries its constructed decoders
struct Venue {
    addresses: VenueAddresses,
    decoders: Vec<Box<dyn TradeDecoder>>,
}

// decode.rs — per transaction: the role selects a prebuilt list
match role {
    Sender       => &decoders.sender,   // [SenderNetting]
    Intent       => &decoders.intent,   // [CowSettlement, IntentNetting]
    Venue(venue) => &venue.decoders,    // resolved from tx.to by address
}
```

```
                          matched transaction
                          (tx.to = entry_point)
                                 │
                                 ▼
               ┌─────────────────────────────────────┐
               │ Is entry_point a known VENUE?        │  registry.venue_for(tx.to)
               │   (Relay router, MetaMask router …)  │
               └─────────────────────────────────────┘
                      │ yes                    │ no
                      ▼                        ▼
            TraderRole::Venue(&Venue)  ┌──────────────────────────────┐
                      │               │ Is entry_point a BATCH SETTLER│  is_batch_settler(tx.to)
                      │               │   (CoW settlement contract)?  │
                      │               └──────────────────────────────┘
                      │                    │ yes            │ no
                      │                    ▼                ▼
                      │            TraderRole::Intent  ┌───────────────────────┐
                      │                    │           │ Is entry_point KNOWN? │  is_known(tx.to)
                      │                    │           │  (a registered router) │
                      │                    │           └───────────────────────┘
                      │                    │             │ yes            │ no
                      │                    │             ▼                ▼
                      │                    │      TraderRole::Sender   TraderRole::Intent
                      │                    │             │                │
                      ▼                    ▼             ▼                ▼
              venue.decoders         intent decoders   SenderNetting   intent decoders
                      │               └── intents/ ──┘   netting.rs     └── intents/ ──┘
           ┌──────────┴─────────────────┐
           ▼                            ▼
     [RelayCalldata, RelayNetting]  [MetaMaskNetting]
     (venues/relay.rs)              (venues/metamask.rs)

   direct call vs a solver-settled intent order — SAME solver, DIFFERENT decoder:
     0x called directly             → Sender → [ SenderNetting ]     (your own tx, your gas)
     0x settling your intent order  → Intent → the intent decoders   (a solver settles for you)
   an intent source with a richer signal gets its own decoder ahead of the netting fallback —
   CoW reads its Trade event (intents/cow.rs), then IntentNetting catches the rest. Relay is the
   same shape: RelayCalldata reads the settling solver's own calldata (SwapIntent) plus a
   recipient-anchored ledger query for the settled output, ahead of RelayNetting.

   implement a new decoder where the entity that carries the flow lives:
   ├─ new venue                       → venues/<name>.rs (a decoders constructor) + a DECODERS row
   ├─ new read for an existing venue  → another TradeDecoder in that venue's constructor (first-wins)
   └─ new intent source (a settler)   → intents/<name>.rs  +  entry in intents::decoders()

   the Intent list lives in intents::decoders() (first-wins order):
     [ MyDecoder, IntentNetting ]   # prepend: self-guard; netting stays the fallback
     [ MyDecoder ]                  # full swap: no fallback — must cover every intent trade
```

One transaction goes to one entity — a direct sender, an intent order, or a specific venue — and
that entity's decoders are tried in order, first hit wins. Every kind of evidence is gathered
once into the `DecodeContext`, so a decoder takes only what it needs and a decoder that declines
costs the next one nothing. What the common sources tell you — examples, not a fixed menu:

| Evidence | What it tells you |
|---|---|
| Value movements: ERC-20 `Transfer` events + traced native transfers | What actually moved |
| Calldata: the transaction's input | What the transaction requested |
| Protocol event logs: a `Swap`/fill event | What the contract declared |

A decoder may read one of these, several at once, or something else.

Example of a venue correction: on a MetaMask ETH→token swap, netting alone recovers "1000 ETH →
2000 TOKEN" — well-formed but wrong, because 9 of the 1000 went to MetaMask's fee wallet before
the swap. `MetaMaskNetting` backs the fee out to 991.

### Gas is derived, not declared

Decoders do not decide gas. One rule (`decode::gas_scope`) derives how the settled route's gas is
charged, from facts the role and the flow already establish:

| The flow says | Gas charged |
|---|---|
| The trader sent the transaction and funded the swap; entry point is a solver | The whole transaction |
| The trader sent the transaction and funded the swap; entry point is a venue | The solver call's trace frame (the venue's own overhead is charged whichever router it picks) |
| The sender never funded the swap (a solver-initiated rebalance), or someone else's flow is tracked (intent fills) | Nothing |

### Solver knowledge (`solvers/`)

What a solver's transactions reveal beyond its address — a calldata-recovered swap intent
(KyberSwap's ABI decode plus its `clientData` quote, ParaSwap's word layout, Fly's packed
layout), a match-time veto (LiFi's bridge orders), or the integrator tag a frontend records in
the solver's event (LiFi's Diamond). Every method defaults to "nothing to add", so most solvers
are a single address-book line with no code; those with code are registered in
`solvers::IMPLEMENTATIONS`.

That table is consulted once, when a solver name is resolved to its handle
(`solvers::knowledge(name)` → `&dyn SolverKnowledge`): attribution carries the handle on its
result, and every consultation after that calls the trait on it. A book-only solver resolves to
a shared no-op implementation, so call sites never branch on whether a solver has code. Adding a
`SolverKnowledge` method is just the trait method — no per-method dispatch function.

`solvers::settled_intent` is the venue-agnostic half of a calldata-primary decode, in one step:
find the solver frame in the trace, resolve its handle, recover its `swap_intent` and
`output_recipient`. A venue's calldata decoder (`RelayCalldata`) is that call plus the venue's
own guards, fee basis, and corrections — the next venue's calldata decoder is the same thin
shape.

```rust
trait SolverKnowledge {
    /// The trader's swap terms (token in/out, amounts, the on-chain min_amount_out floor, and —
    /// when the calldata declares one — the solver's off-chain quote), when the solver frame's
    /// own calldata carries them — how a reverted trade's floor is recovered (a revert emits no
    /// logs to net a settled amount from). `amount_in_hint` is the decoded flow's input amount,
    /// when known (absent for a reverted trade); scan-based extractors (ParaSwap) need it to
    /// locate fields by value rather than by ABI offset.
    fn swap_intent(&self, input: &[u8], amount_in_hint: Option<U256>) -> Option<SwapIntent> { None }

    /// The address this solver's calldata declares as the output recipient — how a
    /// calldata-primary decode (RelayCalldata) learns whose receipt to read the settled amount
    /// from, since calldata alone never carries a settled amount.
    fn output_recipient(&self, input: &[u8]) -> Option<Address> { None }

    /// The veto this solver's logs place on a matched transaction that is not a swap.
    fn solver_veto(&self, logs: &[Log]) -> Option<Veto> { None }

    /// The order-flow integrator tag this solver records in its logs, when it exposes one.
    fn integrator(&self, logs: &[Log]) -> Option<String> { None }
}
```

### Venue attribution (`venue_attribution.rs`)

The venue is normally the contract the trader entered through (`tx.to`). Some order-flow venues own
the flow without being that contract, so after a flow is decoded one step can override the venue from
a registry fingerprint. Nothing in `venue_attribution.rs` names a specific venue — it reads four maps
from the address book:

- **owning trader** (`[venue_owners]`) — the flow was read from a known venue address (kpk's Safes,
  surfaced from the CoW decoder's owner).
- **CoW appData tag** (`[venue_appdata]`) — the settled order committed a frontend tag (`appCode`)
  whose appData hash maps to a venue (LlamaSwap). The hash is read from the settle calldata by
  `intents::venue_tag`, so `venue_attribution.rs` stays protocol-agnostic.
- **fee wallet** (`[venue_fees]`) — a known venue fee wallet took the output-token fee (Phantom,
  Robinhood); the fee is grossed back. Only inside an already-matched trade, so a dust spray to a
  fee wallet is not mistaken for flow.
- **provider integrator tag** (`[venue_integrators]`) — a provider's event carried an integrator
  string mapped to a venue (LiFi frontends). The tag is read by that provider's
  `SolverKnowledge::integrator`, so `venue_attribution.rs` stays provider-agnostic.

### Why matching stays venue-keyed

Decoding centers on the solver call, but matching does not. Here's the catch. We recognize a
Relay transaction by its address in the receipt — before doing anything expensive. But we can't
see the solver call without tracing first. So "find all solver trades first" really means "trace
every transaction in the block", because we wouldn't know which ones to skip. That switch is out
of scope for now; if it ever happens, decode is already organized around the solver, so matching
by solver frame slots in without a redesign.

### Per protocol, not per chain

A venue or solver deployed on several chains behaves the same everywhere, so one decoder serves
all of them; what differs per chain — entry points, router addresses, fee collectors,
stablecoins — lives in the per-chain address book. A venue that genuinely diverges on one chain
(a different router, a different ABI) is registered under its own section name (Relay on Base →
`[venues.relay_base]`) with its own decoder.

A wrong sameness assumption mostly surfaces as trades failing to decode or `verify` — but not
always: a diverged fee scheme, with the fee collector missing from that chain's book, decodes
trades with the fee still inside the amounts. Those are wrong records, not misses, so fee
collectors are re-verified on every chain a venue is added on.

### Where does new code go?

| You want to… | Touch | Without it |
|---|---|---|
| Track a new solver | One line in the address book's `[solvers]` section. No code — trades sent straight to the router then match on the entry point and decode like any other; the intent and veto rows below are optional extras | Trades sent directly to the solver's router never match, so they never appear in the output; trades a known venue routed through it still decode, but the solver is recorded as "unknown" |
| Recover a solver's swap terms (tokens, amounts, on-chain floor, and — when its calldata declares one — its off-chain quote) | A `swap_intent` method on its `SolverKnowledge` impl, called with the settling solver frame's own input | A trade's record carries no `min_amount_out` / `declared_quote` / `quote_timestamp` |
| Skip a solver's non-swap orders | A `solver_veto` method on its `SolverKnowledge` impl | Those orders decode as trades that never happened, with absurd rates |
| Add a venue | A `[venues.<name>]` section in the address book, a `TradeDecoder` in `venues/` (constructed with the venue's addresses by its `decoders` function), one row in `venues::DECODERS` | The venue's trades are missed: with no entry-point match they only surface when a known solver logs inside them, and intent decoding then excludes the trader |
| Extend what Hindsight knows about a venue | That venue's module in `venues/` — never anywhere else | Decoding degrades silently |
| Decode an intent settler (CoW-style) | A `TradeDecoder` in `intents/`, listed in `intents::decoders` ahead of the netting fallback | The settler's trades decode by net flow, losing exact amounts and (for contract owners) the venue |
| Attribute a new venue (owner / appData tag / fee wallet / integrator tag) | The matching address-book map (`[venue_owners]` / `[venue_appdata]` / `[venue_fees]` / `[venue_integrators]`); a provider's integrator tag also needs `SolverKnowledge::integrator` | The venue's trades are attributed to the underlying router or settler, not the venue |
| Add a new decode method | A `TradeDecoder` (the `netting` engine or `solvers::settled_intent` behind it), listed for the entities that use it | Transactions the existing decoders cannot read stay undecoded |
| Reject decodes that are not real trades (an NFT purchase's payment leg, a mis-paired wrap) | A check in `veto.rs` | Records that are not trades enter the comparison |
| Support a new chain | A `registry/<chain>.toml` address book, an entry in `registry::BUILTIN_CHAINS`, plus decoders for its venues and `SolverKnowledge` for its solvers that have none yet | The chain has no built-in book and must be passed via `--registry` |

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
batch settlers, infrastructure contracts, USD-anchor stablecoins, display labels — lives in a
per-chain TOML loaded by `Registry`. One book is embedded at compile time per chain — ethereum,
base, unichain, arbitrum, bsc, polygon — and `--chain <name>` picks one. Pass `--registry <path>`
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
```

The RPC endpoint must support `debug_traceTransaction`.
