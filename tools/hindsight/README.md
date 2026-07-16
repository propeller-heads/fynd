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
           ┌─────────────────┐    match_veto    ┌─────────────────┐
           │    matching     │ ───────────────▶ │ SolverKnowledge │
           └────────┬────────┘  skips non-swaps └─────────────────┘
                    │  entry point or a solver log
                    │  debug_traceTransaction
                    ▼
           ┌─────────────────┐
           │ gather evidence │   receipt and logs, trace, root calldata, transfer
           └────────┬────────┘   ledger — one DecodeContext for every strategy
                    │
                    ▼
     DecodeStrategy::decode — tried in order, first success wins
      ┌ 1 ────────────────────────────────┐   tx entered    ┌────────────────┐
      │ netting — nets the trader's       │ ──────────────▶ │ VenueKnowledge │
      │ value movements                   │  via a venue    └────────────────┘
      └───────────────────────────────────┘
      ┌ 2 ────────────────────────────────┐
      │ future: calldata decoding,        │
      │ log parsing                       │
      └───────────────────────────────────┘
                    │  TraderFlow
                    ▼
           ┌─────────────────┐  embedded_quote  ┌─────────────────┐
           │ post-processing │ ───────────────▶ │ SolverKnowledge │
           └────────┬────────┘                  └─────────────────┘
                    │  guards → attribution → gas → quote → sandwich scan
                    ▼
              DecodedTrade
```

Horizontal arrows are consultations: the pipeline stage calls the named trait method on the
protocol's knowledge implementation. The stages themselves are protocol-agnostic.

### The extension traits

The decode pipeline is extended by implementing a trait.

#### `DecodeStrategy` (`strategies/`)

One method of extracting the swap from a matched, traced transaction. The context carries all
the gathered evidence — receipt and logs, trace, transfer ledger, root calldata — and an
implementation reads the evidence its method trusts. Strategies are tried in a fixed order;
returning `None` hands the transaction to the next one.

```rust
trait DecodeStrategy<P> {
    /// Label recorded on the trades this strategy decoded.
    fn name(&self) -> &'static str;
    /// The trader's flow, or `None` when this method cannot decode the transaction.
    async fn decode(&self, ctx: &mut DecodeContext<P>) -> Option<TraderFlow>;
}
```

#### `VenueKnowledge` (`venues/`)

Everything Hindsight knows about one venue, today a single decode method that nets the
trader's flow and backs the venue's fee out. One implementation per venue, bound to its
`[venues.<name>]` address-book section — the registry fails at load time when a section has no
implementation. When venues grow a new kind of knowledge (say, calldata parsing for a calldata
strategy), it becomes a new trait method whose default means "this venue has nothing special
here", so only the venues with that behavior override it and every other impl stays unchanged.

```rust
trait VenueKnowledge {
    /// Decode a transaction entered through this venue's contract.
    fn decode(&self, ctx: &VenueContext) -> Option<TraderFlow>;
}

// venues/mod.rs — one arm per address-book section
fn from_name(name: &str) -> Option<&'static dyn VenueKnowledge> {
    match name {
        "relay" => Some(&relay::Relay),
        "metamask" => Some(&metamask::Metamask),
        _ => None,
    }
}
```

#### `SolverKnowledge` (`solvers/`)

What a solver's transactions reveal beyond its address. Both methods default to "nothing to
add", so most solvers are a single address-book line with no code; the solvers that do have
code are registered in `solvers::IMPLEMENTATIONS`.

```rust
trait SolverKnowledge {
    /// The solver's off-chain quote declared in its calldata, when it embeds one.
    fn embedded_quote(&self, input: &[u8], amount_in: U256) -> Option<SolverQuote> { None }

    /// The reason this solver's logs mark a matched transaction as not a same-chain swap.
    fn match_veto(&self, logs: &[Log]) -> Option<&'static str> { None }
}
```

The implementations are per protocol, not per chain: a venue or solver deployed on several
chains behaves the same everywhere, so one implementation serves all of them. Everything that
does differ per chain — entry points, router addresses, fee collectors, stablecoins — lives in
the per-chain address book, which is handed to the implementations at run time. A venue or
solver that genuinely diverges on one chain (a different router contract, a different ABI) is
registered in that chain's book under its own section name — Relay diverging on Base would
become `[venues.relay_base]` — and that name is bound to its own `VenueKnowledge` or
`SolverKnowledge` impl instead of the shared one. A wrong sameness assumption mostly surfaces
as trades failing to decode or failing `verify` — but not always: a fee scheme that diverged,
with the fee collector missing from that chain's book, decodes trades with the venue's fee
still inside the amounts. Those are wrong records, not misses, so a venue's fee collectors
must be re-verified on every chain it is added on.

### Where does new code go?

| You want to… | Touch | Without it |
|---|---|---|
| Track a new solver | One line in the address book's `[solvers]` section. No code — trades sent straight to the router then match on the entry point and decode like any other; the quote and veto rows below are optional extras | Trades sent directly to the solver's router never match, so they never appear in the output; trades a known venue routed through it still decode, but the solver is recorded as "unknown" |
| Read a solver's quote from its calldata | A `SolverKnowledge` impl in `solvers/`, registered in `solvers::IMPLEMENTATIONS` | Records for that solver carry no quote |
| Skip a solver's non-swap orders | A `match_veto` method on its `SolverKnowledge` impl | Those orders decode as trades that never happened, with absurd rates |
| Add a venue | A `[venues.<name>]` section in the address book, a `VenueKnowledge` impl in `venues/`, one arm in `venues::from_name` | The venue's trades are missed, or decoded with its fee still inside the amounts — see below |
| Extend what Hindsight knows about a venue | That venue's module in `venues/` — never anywhere else | Decoding degrades silently — see below |
| Add a new way to extract swaps | A module in `strategies/` plus one entry in `default_strategies` | Transactions the existing methods cannot decode stay undecoded |
| Reject decodes that are not real trades (an NFT purchase's payment leg, a mis-paired wrap) | A guard in `guards.rs` | Records that are not trades enter the comparison |
| Support a new chain | A `registry/<chain>.toml` address book (all sections required), plus `VenueKnowledge`/`SolverKnowledge` impls for its venues and solvers that have none yet | Only `ethereum` is built in |

### Strategies vs venue knowledge vs solver knowledge

**A strategy is a method** for extracting the swap, and what defines a method is the on-chain
evidence it reads. Choosing the `DecodeStrategy` implementation is choosing which evidence to
trust:

| Evidence | What it tells you | Strategy |
|---|---|---|
| Value movements: ERC-20 `Transfer` events + traced native transfers | What actually moved | `netting` (today) |
| Protocol event logs: a venue's or solver's own `Swap`/fill events | What the contract declared happened | future |
| Calldata: the transaction's top-level input | What the transaction requested | future |

All the evidence is extracted for every matched transaction regardless of strategy: the receipt
and its logs, the trace, the flattened transfer ledger, and the root calldata are all in the
context handed to each strategy. Gathering once is deliberate — the context doubles as a
per-transaction cache, so a strategy that declines costs the next one nothing, and a hybrid
strategy that combines kinds of evidence pays no second extraction. It is also nearly free: the
trace is fetched anyway because post-processing (gas isolation, attribution) needs it whichever
strategy wins, and the rest is a single pass over the transaction's own logs.

Strategies form a short ordered list; a later one is the fallback when an earlier one cannot
decode a transaction. A strategy is defined by *how* it decodes, not whose transactions it
decodes.

**Venue knowledge** is venue-specific information a strategy consults while decoding. It comes
in layers, and all of a venue's layers live in its one module under `venues/`:

- *Address facts* (entry points, fee collectors) — pure data, in the address book.
- *Transfer-based knowledge* — how to correct a netted flow: back the venue's fee out,
  recognize Relay's solver-rebalance fills.
- *Calldata-based knowledge* — how to read the venue's own contract input: MetaMask's router
  ABI and the solver id it declares.

Example: on a MetaMask ETH→token swap, netting alone recovers "1000 ETH → 2000 TOKEN" — a
well-formed swap, but wrong, because 9 of the 1000 went to MetaMask's fee wallet before the
swap. The venue knowledge corrects it to 991: the strategy extracts the swap, the venue module
corrects it.

Missing venue knowledge degrades decoding silently. A venue absent from the address book
mostly goes undecoded — its trades only match when a known solver logs inside them, a coverage
gap visible in `verify`. A registered venue with an unlisted fee collector is worse: its trades
decode with the fee still inside the amounts, and every comparison credits Fynd with the
venue's own fee, silently inflating wins.

**Solver knowledge lives on the solver's `SolverKnowledge` impl.** Each piece — a calldata
quote format (KyberSwap's `clientData`, ParaSwap's word layout), a match-time veto (LiFi's
bridge orders) — is a trait method with a default, so a solver implements only what it has and
most solvers are address-book lines with no code at all.

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
