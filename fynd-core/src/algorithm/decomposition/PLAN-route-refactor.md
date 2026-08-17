# Plan: one `Route` enum, and a `SolveCtx` for the token registry

Replace the five solution types plus the `HopPool` adapter with a single recursive `Route` enum, and
move the token registry out of the nodes into a solve-scoped context.

Separate from [`PLAN-topology-graph.md`](PLAN-topology-graph.md), which changes what the search walks
over. This one changes what the search produces. They do not overlap and either can land first.

## Vocabulary

Fixed for this document and the code that comes out of it.

- A **hop** is one step of a sequence. It is not a type; it is a member of `Route::Sequence`.
- A **pool** is one component traded in one direction.
- **Never "leg".** The word does not appear in the new code.

## What is wrong now

Five types — `DecompositionGraph`, `Branch`, `SequentialRoute`, `Hop`, `PoolRef` — plus the
`HopPool` adapter, implement three concepts:

| Today | Really |
| --- | --- |
| `DecompositionGraph` | a split over branches |
| `Branch(Head)` | a sequence: shared hop, then a split over tails |
| `Branch(Tail)` | a sequence: a split over tails, then the shared hop |
| `SequentialRoute` | a sequence of hops |
| `Hop` | a split over pools |
| `PoolRef` + `HopPool` | one pool |

The consequences:

* **Nine composed attributes are written four or five times each.** `route_price`,
  `marginal_price`, `new_marginal_price`, `fee`, `gas`, `minimum_gas`, `inertia`, `weight`,
  `executed_price` — around 36 methods, plus five near-identical private `combine*` helpers. Every
  one is a product, a sum, a minimum or a maximum over children. There are two rules, not five.
* **`BranchSide` is a position, not a kind.** `sell_hop_first` and `sell_sequences_first` are mirror
  images; three cast helpers (`cast_to_sell_token`, `cast_to_sequence_in`, `cast_from_hop_out`)
  differ only in which stretch of a chain they walk.
* **`HopPool` exists because `PoolRef` has no direction.** The tokens live on the enclosing `Hop`,
  so the optimizer re-marries them before it can split anything — cloning two `Token`s per pool per
  call. `optimizers/mod.rs` already carries a note saying the wrapper is unnecessary.
* **`Sellable` dedups the caller, not the implementations.** It is why the optimizers are written
  once, and it adds 10 methods × 3 impls of forwarding on top of the duplication it does not remove.

## Target shape

```rust
pub(crate) enum Route {
    /// Alternatives in parallel: a hop's pools, the solution's branches, a grouped branch's tails.
    Split { children: Vec<Route>, splits: Vec<Fraction> },
    /// Hops in series.
    Sequence { hops: Vec<Route> },
    /// One pool traded in one direction.
    Pool(PoolRef),
}
```

This is defibot's `ParallelRoute` / `SequentialRoute` / `SimpleRoute`, which keeps the README's
cross-references one-to-one — closer to the Python than the current five types are.

The port removed the recursive tree because it was *nestable without limit*
(`README.md`, "Structural change"). A recursive type is not the same thing as recursive
construction: `graph_build` is the only thing that builds routes and it produces exactly one shape.
The bound moves from the type system to the builder plus one `validate()` walk. What is given up is
"a `Hop` cannot contain a `Hop`" — the only bug that prevents is a builder bug. Every other
invariant (non-empty pools, tokens matching hops, branches sharing endpoints) is already a runtime
check in `Hop::new`, `SequentialRoute::new`, `Branch::head` and `DecompositionGraph::new`, so
nothing changes for those.

`Route` collides with `crate::types::Route`, the encoder's output. The overlap is four references in
`assemble.rs` and two in `mod.rs`; alias there (`use crate::types::Route as EncodedRoute`).
`SequentialRoute` disappears in the merge, so nothing else clashes.

### What each rule becomes

```rust
impl Route {
    pub(crate) fn marginal_price(&self, ctx: &SolveCtx<'_>) -> Result<Price, DecompositionError> {
        match self {
            Route::Pool(pool) => pool.marginal_price(ctx),
            Route::Sequence { hops } => {
                let mut price = Price::UNIT;
                for hop in hops {
                    price = price * hop.marginal_price(ctx)?;
                }
                Ok(price)
            }
            Route::Split { children, splits } => mix(children, splits, |child| child.marginal_price(ctx)),
        }
    }
}
```

One function replaces six. `sell` replaces seven, and the head/tail fork goes with it: "hop first"
and "hop last" are the order of `hops` in a `Sequence`.

### What the optimizers become

```rust
// before
pub(crate) fn split_by_pair_comparison<S: Sellable>(routes: &mut [S], ..)
// after
pub(crate) fn split_by_pair_comparison(routes: &mut [Route], ..)
```

The bodies do not change. They call nine methods and all nine are on `Route`. Consequences:

* `SplitOptimizer::split` becomes object-safe, so `solve_graph` loses its type parameters and the
  per-optimizer `match` in `mod.rs` collapses.
* `HopPool::bind_all(hop)` becomes `route.children_mut()`.
* `Sellable` and its three impls delete.

## `SolveCtx`

Starts as **only** the token registry. Nothing else moves in this pass.

```rust
pub(crate) struct SolveCtx<'a> {
    market: &'a MarketState,
}

impl SolveCtx<'_> {
    /// The full token, for the tycho calls that demand one.
    fn token(&self, address: &Address) -> Option<&Token>;
    /// Just the exponent, for turning an on-chain amount into a human one.
    fn decimals(&self, address: &Address) -> Option<u32>;
}
```

`PoolRef` then holds addresses instead of tokens:

```rust
pub(crate) struct PoolRef {
    component_id: ComponentId,
    limit_kind: SellLimitKind,
    state: Box<dyn ProtocolSim>,
    depth: Option<BigUint>,
    token_in: Address,
    token_out: Address,
}
```

`tycho_common::models::Token` is a fat struct — `Bytes` address, `String` symbol, `u32` decimals,
`TransferTax`, `Vec<Option<TransferCost>>` gas, `Chain`, `u32` quality — two heap allocations per
clone, for the two fields anything here reads. Today it is cloned four times over on the way in:
`resolve_tokens` clones every token of a sequence per group, `Hop::new` clones the pair again,
`SequentialRoute::new` clones the vector again, and `HopPool::bind_all` clones the pair **per pool,
per optimizer call** — and PairComparison makes five refinement passes over a hop.

The market snapshot already is the registry; `market.get_token(address)` is what `resolve_tokens`
calls today. The change is to stop cloning out of it and start borrowing from it.

**Deferred to a later pass, deliberately:** the swap cache, the limit caches, the `prices` field,
and pure quoting. They all want the same `&SolveCtx` parameter this pass threads, which is why the
threading is worth doing first and on its own.

## Decimals

On-chain amounts are integers in the token's smallest unit: 1 USDC is `1_000_000` (6 decimals), 1
WETH is `10^18`. Every price in this module is in **human** units — buy-token per sell-token, as a
person would say it. Converting between the two is the only thing decimals are for: divide the
integer by `10^decimals`.

So decimals are needed exactly where an on-chain integer becomes a float. There are three such
places and no others:

| Site | Needs |
| --- | --- |
| `executed_price` | both tokens' decimals — `(buy/sell) × 10^(sell_dec − buy_dec)` |
| `PoolRef::inertia` | `token_in`'s decimals; `depth` is on-chain units, inertia is human |
| `spot_price` / `get_amount_out` | tycho converts internally — which is *why* they take `&Token` and not an address |

Everything else stays integer end to end. `convert_through_numeraire` is the counter-example:
`TokenGasPrices` are ratios already expressed in on-chain units, so it multiplies and divides
`BigUint`s and never touches decimals.

That is the whole surface, and it is why `SolveCtx` needs only the two accessors above.

## Float attributes

Undocumented today, and the source of most of the confusion:

| Attribute | Units | Kind |
| --- | --- | --- |
| `route_price` | buy per sell, human | rate, pre-trade, gross |
| `marginal_price` | buy per sell, human | rate, pre-trade, net of fee |
| `new_marginal_price` | buy per sell, human | rate, at the post-trade state |
| `executed_price` | buy per sell, human | rate, realised |
| `fee` | none | fraction |
| `inertia` | human units of the **sell** token | size |
| `weight` | human units of the **buy** token | size |

`weight = inertia × (1 − fee) × route_price`, so it is a size — what comes out if the depth-sized
amount trades at spot — not a rate. All seven are bare `f64`, so comparing a weight against a price
compiles and means nothing.

The comparisons in the code today are sound: `Hop::weight` ranks pools sharing a pair, and the three
`rank_subgraph` calls rank sequences sharing endpoints. Nothing states that, and nothing enforces it.

Two properties worth keeping:

* **Floats never become amounts.** `Fraction::from_f64` has no library caller — its doc says it is
  there so fixtures can write defibot's exact ratios. Splits stay `BigRational`, amounts stay
  `BigUint`. f64 is confined to ranking and pruning, where approximate is correct.
* **One exception.** `assemble.rs` calls `split.to_f64()` four times to build
  `PathAllocation.fraction`, which is the only float that reaches the transaction.
  `build_split_route` renormalises so the error is bounded, but the exact `Fraction` is available
  and could stay exact.

Handled but silent: `BigUint::to_f64()` returns `None` on overflow — `inertia` falls back to
`MISSING_DEPTH_INERTIA`, `executed_price` does `unwrap_or(0.0)` and the route then ranks last
without saying why. NaN is mapped to `NEG_INFINITY` in one place (`rank_subgraph`), and
`NEG_INFINITY` doubles as the "no children yet" seed in the maximum folds.

### Newtypes, folded into the merge

```rust
/// Buy-token per sell-token, in human units.
struct Price(f64);
/// An amount in human units of a named token.
struct Size(f64);
```

`marginal_price() -> Price`, `weight() -> Size`, `inertia() -> Size`. Mixing them stops compiling.

Done as its own pass this is churn for a payoff that is mostly documentation. Done **inside** the
enum merge it is nearly free, because all 36 methods are being rewritten into 9 anyway.

## Order of work

1. **Snapshot test.** Nothing else starts until this is green. See below.
2. **Fix the test suite.** 66 errors across 8 files from the in-flight refactor, mostly struct
   variants and removed methods.
3. **`SolveCtx` + addresses on `PoolRef`.** Threads one parameter, deletes the token cloning and
   `HopPool::bind_all`'s per-pool clones. `HopPool` itself can go here or with step 4.
4. **The `Route` enum**, with `Price`/`Size` folded in. Delete `Sellable`, `BranchSide`,
   `sell_hop_first`, `sell_sequences_first`, two of the three cast helpers, and the five `combine*`
   helpers.
5. **Rewrite the README's type mapping.** `Split`/`Sequence`/`Pool` against defibot's
   `ParallelRoute`/`SequentialRoute`/`SimpleRoute`.

## The snapshot test

Decomposition has **no** snapshot coverage today. `fynd-core/tests/integration/worker_pools.toml`
configures one pool, `bellman_ford`, so `expected_outputs.json` is that algorithm's output and
decomposition never runs. `tests/decomposition_gno.rs` is a printing diagnostic behind `#[ignore]`,
not an assertion. The unit tests under `decomposition/tests/` build small synthetic markets and will
not catch "the real fixture now picks a different pool on hop two".

`ExpectedOutput` pins `status`, `amount_out_net_gas`, `gas_estimate`, `num_swaps` and
`solve_time_ms`. None of those identify a route: two different routes with the same net output and
hop count both pass.

What the new snapshot must pin, per order:

* every swap's component id, token in and token out, **in order**;
* the split on each swap, as the exact rational rather than an `f64`;
* `amount_out` and `amount_out_net_gas`.

Ordering matters more than usual here. The enum merge changes child iteration order and the topology
work changes which pools land in a hop and in what order, so several tie-breaks will shuffle. A
snapshot comparing sets rather than sequences would hide exactly the regressions worth catching.

Orders must be **large enough to split**. A small order takes one pool and pins nothing about the
split search, which is the part being refactored.

One caveat to record in the fixture itself: decomposition currently returns its *reference* route on
`GNO_to_AAVE` because the candidate cannot assemble (see `tests/decomposition_gno.rs`). Pinning
today's behaviour pins that defect too. That is correct for a refactor snapshot — the point is to
notice any change — but whoever fixes `assemble`'s stretching later must regenerate deliberately
rather than read the diff as a break.

---

# State of the work

## Landed and verified

* **`Arc<Token>` on the route components.** `PoolRef` carries the pair it trades; `SplitRoute` and
  `SequenceRoute` derive their endpoints from it. `graph_build::resolve_tokens` shares out of
  `MarketState` (`get_token_shared`) instead of deep-cloning `Token` four times over. `HopPool` and
  `bind_all` are gone — a pool implements `Sellable` directly.
* **Snapshot test** at `tests/snapshots/decomposition_routes.json`, 44 orders at `max_hops = 3`,
  8 workers, ~3.5s. Pins each swap's component, pair, split (6dp) and order, plus the amounts.
  `[profile.dev.package."*"] opt-level = 3` in the workspace `Cargo.toml` is what took it from 82s.
* **Connector filtering removed from the candidate search** (reference legs keep theirs). Measured
  over the 44 orders: 27 identical, 14 improved, 3 regressed. Best: `SHIB_to_WETH` +518bps and
  `WETH_to_SHIB` +513bps, both going from *one* swap to five or six. Snapshot regenerated on this.
* **`NoPath` reporting restored** in `find_best_route`; it had been flattening everything to
  `AlgorithmError::Other`, which no other algorithm does.

## Landed, not yet verified

The `Route` enum. **The snapshot has not run against any of it.**

```rust
enum Route { Split(SplitRoute), Sequence(SequenceRoute), Pool(PoolRef) }

// split.rs     SplitRoute    { children: Vec<Route>,      splits, amounts, limit_cache }
// sequence.rs  SequenceRoute { hops:     Vec<SplitRoute>, amounts, limit_cache, prices }
// pool.rs      PoolRef       { component, Arc<Token> pair, state, depth, caches }
// branch.rs    Branch        { hop: SplitRoute, side, sequences: Vec<SequenceRoute> }
```

`route.rs` is the enum plus dispatch for what all three share, nothing else. Each shape owns its own
state and its own logic in its own file. Validation is in the constructors, so an invalid level
cannot be built; `Route::validate` is deleted. The `as_split`/`as_sequence`/`as_pool` accessors are
deleted — wherever the shape is known statically the caller holds the struct, which is why the only
`match` on `Route` outside `route.rs` is the no-split-children check in `SplitRoute::new`.

`SplitRoute::pools()` is the direct children (one per entry in `splits`, so `assemble.rs`'s zip is
correct by construction). `Route::all_pools()` is the recursive walk.

**Target: zero snapshot movement.** The arithmetic groups the way it did, including both `weight`
single-child delegations, which are load-bearing for branch ranking. Any moved row is a port bug.
Most likely culprits, in order: `SplitRoute::use_estimate` (the `splits_sum < 1` fallback), the
`weight` delegations, `SplitRoute::pools()` vs the old recursive flatten.

## Left to do

79 library errors, then six test files.

| File | Errors | Work |
| --- | --- | --- |
| `solve.rs` | 32 | in progress |
| `graph_build.rs` | 21 | see *Build once* below |
| `assemble.rs` | 11 | walking hops to build `HopDescriptor`s |
| `optimizers/*` | 15 | the `Sellable` impl set |

### `solve.rs` cleanups agreed

1. **`solve_head_branch` / `solve_tail_branch` share one error shape** written four times. Extract:

   ```rust
   fn restart_after(
       error: DecompositionError,
       amount: &BigUint,
       cast: impl FnOnce(&BigUint) -> Result<BigUint, DecompositionError>,
   ) -> Result<Option<BigUint>, DecompositionError>
   ```

   `cast` is identity where the failing leg already trades the branch's sell token, and
   `cast_from_hop_out` / `cast_to_sequence_in` where it does not. All four sites apply `minus_one`
   then `shrink_below`, so the unification is faithful — diff them before trusting that.

2. **`split_branch_sequences` should take `&mut [SequenceRoute]` and return `Vec<Fraction>`.** It
   touches the branch only for `sequences_mut()` and `set_splits`. Returning the splits puts all
   three answers (empty, all-zero, optimised) through one return type.

3. **`decrease_until_sell` takes a lambda**, not a `Sellable`:

   ```rust
   fn decrease_until_sell(
       sell_amount: &BigUint,
       sell: impl FnMut(&BigUint) -> Result<(BigUint, BigUint), DecompositionError>,
   ) -> Result<(BigUint, BigUint), DecompositionError>
   ```

   It only ever calls `sell`. This fixes `solve_sequence_route`'s last line, which currently tries
   `decrease_until_sell(Route::Sequence(route), ..)` — a move out of a `&mut`, into a temporary whose
   mutations would be dropped.

   **`Sellable` does not disappear.** The optimizers need a trait over whatever they split, and that
   is `Route` (a hop's children), `SequenceRoute` (a branch's tails) and `Branch` (the graph's
   branches). Three impls instead of five. It shrinks to one only if `Branch` and
   `DecompositionGraph` become `Route`s too — the open Step B question.

4. `let amount = amount;` in `solve_head_branch` and its bare `{ }` block are dead residue from when
   both halves were inline in the restart loop. Same pair likely in `solve_tail_branch`.

### `graph_build.rs` — build once

The messiness is real: it builds `Route`s per token sequence, ranks them, **takes them apart**
(`into_hops`), regroups by head or tail token, and rebuilds. The structure is constructed, dismantled
and reconstructed because grouping happens after construction.

Hold plain data until the shape is decided — `TokenSequenceGroup` is already nearly it: a token
sequence plus one component vector per surviving pool path. Rank on that, group on that, and build
`Route`s exactly once, at the end, when head-vs-tail is settled. `SequenceRoute::into_hops` exists
only to serve the dismantling and goes with it.

**This will move the snapshot**, because ranking currently happens on `SequenceRoute::weight` over
built routes and a pre-construction score is a different ordering. So: land the enum switch first,
get the snapshot green at zero movement, then do this as its own step with its own diff.

## Open questions, not blocking

* Two snapshot regressions from the connector change: `WETH_to_USDC` −256bps against `USDC_to_WETH`
  +284bps. Same pair, opposite directions, similar size — suspect the `max_parallel_routes` cap now
  that more candidates compete for 50 slots. Unverified.
* `test_candidate_beats_the_reference_on_a_path_the_reference_cannot_see` fails. It tested a
  distinction the refactor removed (reference and candidate drew from the same connector set); with
  candidate filtering now off it may be meaningful again. Check before deleting.
* `split.rs` holds `SplitRoute` and `Fraction` moved to `models.rs`. `sequence.rs` holds
  `SequenceRoute`. Whether `Branch` and `DecompositionGraph` collapse into `Route` variants is Step B
  and deliberately unanswered.
