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

All take `--chain` (selects the address book; only `ethereum` is built in) and `--registry` /
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
           ┌─────────────────┐   the matched entity maps to an ordered list of decoders,
           │     decode      │   each tried until one returns a TraderFlow (an entity may
           └────────┬────────┘   list several — a richer source first, a general one as fallback):
                    │
                    │   direct solver           →  [ SenderNetting ]
                    │   batch settler / solver  →  [ IntentNetting ]
                    │   venue relay             →  [ RelayNetting ]
                    │   venue metamask          →  [ MetaMaskNetting ]
                    │  TraderFlow
                    ▼
           ┌─────────────────┐  embedded_quote  ┌─────────────────┐
           │ post-processing │ ───────────────▶ │ SolverKnowledge │
           └────────┬────────┘                  └─────────────────┘
                    │  veto → attribution → gas → quote → sandwich scan
                    ▼
              DecodedTrade
```

Horizontal arrows are consultations: the stage calls the named `SolverKnowledge` method on the
solver's implementation. The stages themselves are protocol-agnostic.

### Decoders

A `TradeDecoder` turns one matched, traced transaction into the trader's flow. *How* it reads the
swap is open — the trait fixes only the input and the output, never the method. A decoder might
read the value movements, the calldata, the protocol's event logs, some combination of those, or
a source we have not needed yet; netting is simply the one that exists today.

```rust
trait TradeDecoder<P> {
    fn name(&self) -> &'static str;
    /// The trader's flow, or `None` when this decoder cannot read the transaction.
    async fn decode(&self, ctx: &mut DecodeContext<P>) -> Option<TraderFlow>;
}

// decode.rs — the matched entity selects its decoders
match role {
    Sender      => vec![Box::new(SenderNetting)],
    Intent       => vec![Box::new(IntentNetting)],
    Venue(name) => venues::decoders_for(name),   // e.g. "relay" → [RelayNetting]
}
```

```
                          matched transaction
                          (tx.to = entry_point)
                                 │
                                 ▼
               ┌─────────────────────────────────────┐
               │ Is entry_point a known VENUE?        │  registry.venue_name(tx.to)
               │   (Relay router, MetaMask router …)  │
               └─────────────────────────────────────┘
                      │ yes                    │ no
                      ▼                        ▼
            TraderRole::Venue(name)   ┌──────────────────────────────┐
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
           venues::decoders_for(name)  IntentNetting  SenderNetting   IntentNetting
                      │                 └──────── netting_decoders.rs ─────────┘
           ┌──────────┴───────────┐
           ▼                      ▼
     [RelayNetting]        [MetaMaskNetting]
     (venues/relay.rs)     (venues/metamask.rs)

   direct call vs a solver-settled intent order — SAME solver, DIFFERENT decoder:
     0x called directly             → Sender → [ SenderNetting ]  (your own tx, your gas)
     0x settling your intent order  → Intent → [ IntentNetting ]  (a solver settles for you)
   when a solver settles an intent order, its own decoder can replace IntentNetting.

   implement a new decoder where the entity that carries the flow lives:
   ├─ new venue                       → venues/<name>.rs  +  arm in venues::decoders_for("<name>")
   └─ new read for an existing venue  → another TradeDecoder in that venue's list (first-wins order)

   replace a Sender/Intent leaf in decode.rs decoders_for — the arm is global, so:
     Intent => [ MyDecoder, IntentNetting ]   # prepend: self-guard; netting stays the fallback
     Intent => [ MyDecoder ]                  # full swap: no fallback — must cover every intent trade
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

### Solver knowledge (`solvers/`)

What a solver's transactions reveal beyond its address — a calldata quote (KyberSwap's
`clientData`, ParaSwap's word layout), a match-time veto (LiFi's bridge orders). Both methods
default to "nothing to add", so most solvers are a single address-book line with no code; those
with code are registered in `solvers::IMPLEMENTATIONS`.

```rust
trait SolverKnowledge {
    /// The solver's off-chain quote declared in its calldata, when it embeds one.
    fn embedded_quote(&self, input: &[u8], amount_in: U256) -> Option<SolverQuote> { None }

    /// The veto this solver's logs place on a matched transaction that is not a swap.
    fn solver_veto(&self, logs: &[Log]) -> Option<Veto> { None }
}
```

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
| Track a new solver | One line in the address book's `[solvers]` section. No code — trades sent straight to the router then match on the entry point and decode like any other; the quote and veto rows below are optional extras | Trades sent directly to the solver's router never match, so they never appear in the output; trades a known venue routed through it still decode, but the solver is recorded as "unknown" |
| Read a solver's quote from its calldata | A `SolverKnowledge` impl in `solvers/`, registered in `solvers::IMPLEMENTATIONS` | Records for that solver carry no quote |
| Skip a solver's non-swap orders | A `solver_veto` method on its `SolverKnowledge` impl | Those orders decode as trades that never happened, with absurd rates |
| Add a venue | A `[venues.<name>]` section in the address book, a `TradeDecoder` in `venues/`, one arm in `venues::decoders_for` | The venue's trades are missed: with no entry-point match they only surface when a known solver logs inside them, and intent decoding then excludes the trader |
| Extend what Hindsight knows about a venue | That venue's module in `venues/` — never anywhere else | Decoding degrades silently |
| Add a new decode method | A `TradeDecoder` (a `netting`/`calldata` toolkit function behind it), listed for the entities that use it | Transactions the existing decoders cannot read stay undecoded |
| Reject decodes that are not real trades (an NFT purchase's payment leg, a mis-paired wrap) | A check in `veto.rs` | Records that are not trades enter the comparison |
| Support a new chain | A `registry/<chain>.toml` address book (all sections required), plus decoders for its venues and `SolverKnowledge` for its solvers that have none yet | Only `ethereum` is built in |

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

### The address book (`src/decoder/registry/<chain>.toml`)

All chain- and protocol-specific data — solver routers, venue entry points and fee collectors,
batch settlers, infrastructure contracts, USD-anchor stablecoins, display labels — lives in a
per-chain TOML loaded by `Registry`. The Ethereum book is embedded at compile time; pass
`--registry <path>` to extend or replace it without recompiling.

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
