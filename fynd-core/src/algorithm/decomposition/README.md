# Decomposition port notes

`decomposition` is a port of defibot's order solver
(`defibot/solver/order_solver/decomposition/`) into Fynd. This file records two things the code
itself cannot: **bugs found in the defibot source while porting**, and **places where the Rust
deliberately does not match it**.

All `path:line` references on the defibot side are from the state of that repo when the port was
made. Verify before acting on any of them.

## Structural change

defibot models a solution as a recursive `FractalRoute` tree — `SimpleRoute` (one pool),
`SequentialRoute` (hops in series), `ParallelRoute` (alternatives plus splits) — nestable without
limit. Production solutions were observed to collapse to one shape, so the port replaces the tree
with a fixed structure:

```
SolutionGraph     parallel branches + outer_splits    (was the top-level ParallelRoute)
  Branch            head Hop + tails + tail_splits    (was Sequential[first_hop, Parallel[tails]])
    SequentialRoute   token path, one Hop per leg     (was SequentialRoute)
      Hop               P pools + S splits, S == P    (was a per-leg ParallelRoute of SimpleRoutes)
```

A single direct pool is a one-branch graph whose branch has one pool in its head and no tails — no
special case. An empty split vector means "unsolved" at `Hop`, `Branch` (`tail_splits`) and
`SolutionGraph` level; the solver needs that state to force a re-solve after removing loops
(`order_solver.py:717`, `:893`). A branch with no tails has nothing to split and is solved as soon
as its head is.

`Branch` is the level `_group_by_neighbour_token` (`order_solver.py:517-554`) builds, and it is
**not** cosmetic. Without it the outer split is over token paths, and every path leaving the sell
token through the same pool holds a private `PoolRef` copy of it — so the optimizer, which scores
branches against untouched market state, allocates that pool's liquidity once per path. Measured on
the recorded fixture: an order of 3500 GNO → AAVE produced six branches, three starting
`GNO→USDT` through pool `0xf787…b91744` and three starting `GNO→WETH` through `0x3e84…1702c6`, and
each of the three was priced as though it had the pool to itself.

The level is a strict generalisation of a token path — `SequentialRoute[h0, h1, h2]` is
`head: h0, tails: [SequentialRoute[h1, h2]]` — and every composition rule collapses to
`SequentialRoute`'s under that mapping, exactly in real arithmetic and to within a last-place `f64`
rounding where the two group their multiplications differently. `Branch::from_route` performs the
mapping and `route_tests.rs` asserts the collapse.

## Bugs in defibot

Ordered by how much they matter. None of these are reproduced in the Rust; each divergence is
commented at its site with the defibot line.

### Worth chasing

**`_solve_without_splits` can sell more than the order** — `order_solver.py:833-849`.

The allocation loop `break`s once `amount_to_sell` reaches zero (`:833-834`), but the split vector
is then built from *every* branch (`:847-849`). Branches the loop never reached still hold the
`sell_amount` left by the probing `decrease_until_sell` at `:818-819`, so they contribute non-zero
splits: the vector sums above one and `:852` sells more than the order. Branches skipped by the
pool-reuse `continue` at `:838-840` are unaffected — that path calls `sell(0)`.

This is the fallback that runs when normal solving buys nothing, i.e. when liquidity is thin. It
has **no test in defibot**. Port zeroes the unreached branches
(`solve.rs`, `test_solve_without_splits_zeroes_branches_past_the_exhaustion_point`).

**The same function's final sell is rejected in exactly the case it exists for** —
`order_solver.py:852`. `route.sell(order.sell_amount)` checks the amount against the route's own
sell limit (`routes/parallel.py:182-194`), which is what fails when the market cannot absorb the
order. Port records the totals instead of re-selling.

**`cast_to_sell_token` indexes the token path by symbol** — `routes/sequential.py:189`
(`self.tokens.index(token.short())`, against the symbol list built at `:37-40`). A repeated ticker
or a cycle resolves to the *first* occurrence, so the amount is converted through the wrong prefix
of hops. It returns a wrong sell limit rather than raising. Port takes an explicit hop index.

### Latent

**Splits reassigned via a formatted display string** — `pair_comparison.py:47` and `:102-105` key a
dict on `route_info()` (`routes/interface.py:181-186`), a human-readable summary with 5-decimal
formatting. Two structurally distinct routes rendering identically would receive each other's
splits. Port uses indices.

**`ZeroDivisionError` reachable in the pair search** — `pair_comparison.py:154`, `:175`, `:180`
build `Fraction(sell_amount, next_sell_amount)`, and `get_next_route_to_solve` (`:214`) can return a
route whose `sell_amount` is zero.

**Single-route early return hides a shortfall** — `pair_comparison.py:39-42` returns `[ONE]` even
when the probing sell moved less than asked, contradicting the optimizer contract at
`optimizers/interface.py:26-31` that splits need not sum to one. The shortfall survives only in
`sold`.

**`ParallelRoute.pool_swaps` assumes solved** — `routes/parallel.py:211` zips against `self.splits`
unconditionally, raising `TypeError` when it is `None`, while `route_price`, `fee`, `inertia` and
`weight` all handle the unsolved case.

**`price_impact` divides by `route_price` unguarded** — `routes/interface.py:130-131`, immediately
below an `executed_price` that goes to some trouble to catch `DivisionByZero`.

**`_get_degree` caches under a boolean key** — `defibot/solver/market_graph/_market_graph.py:143-149`.
`if degree := self._degree_cache.get(token) is not None:` binds `degree` to the *comparison*, not
the lookup, and the next line writes `self._degree_cache[degree]`. Outside the decomposition
package, but it feeds `list_paths`' search-direction choice (`:192-193`) and therefore candidate
discovery.

**`iteration_strategy` is resolved at import time** — `optimizers/equal_start_v2.py:13-15`. The
module-level `config.get(...)` runs once when the module is first imported, so the setting cannot be
changed for a running process and neither branch of `:172-176` can be exercised against the other in
a test. Ported as a constructor parameter (`optimizers/equal_start_v2.rs`, `RankingMetric`).

**`_get_worse_route_indexes` ranks an on-chain amount against human-unit prices** —
`optimizers/equal_start_v2.py:276` appends `route.sell_amount - int(sell_amount * split)`, an
integer in on-chain units, into a list otherwise holding `Decimal` prices. It is sound only because
the value is negative and prices are not, so whatever the scale it sorts to the worst end. Ported
verbatim with that reasoning recorded at the site.

**`np.argsort` is not stable** — `optimizers/equal_start.py:425`. Its default quicksort leaves
equally ranked routes in whatever order the partition produced, so two solves of the same market can
move funds in different directions. The port sorts stably.

### Hygiene

- **`assert` used for correctness invariants** — `order_solver.py:794-796` (pool deduplication),
  `optimizers/equal_start.py:109`, `optimizers/equal_start_v2.py:149-151`. All vanish under
  `python -O`; all are a hard crash otherwise. Not ported.
- **Unreachable `None` guards** — `optimizers/equal_start_v2.py:192` and `:196` test whether an
  index returned by `_argsort` is `None`, but `_argsort` returns indices into a numpy array and
  never emits one. Not ported.
- **Dead remainder repair** — `optimizers/equal_start_v2.py:77-81` distributes `1 - sum(splits)`
  onto the first non-zero split, but the splits it repairs are `Fraction(1, n)`, whose sum is
  exactly one. Ported anyway (`initial_splits`): it costs nothing and stops being dead if the split
  denominator bound ever falls below the alternative count.
- **`print` to stdout inside a solver warning path** — `order_solver.py:234` calls
  `reference_route.scheme()`, which prints (`routes/interface.py:251-272`). Ported as `tracing::warn!`.
- **Dead branches** — `order_solver.py:454-456` and `:482-484` test `hop_route is None`, but
  `_create_one_hop_route` returns `None` only for empty input (`:570-571`), which the caller already
  rejects at `:476`.
- **Redundant construction** — `_create_one_hop_route` (`:563-569`) builds every `SimpleRoute` twice
  and the singleton branch returns the one built from `paths[0]` rather than the survivor of its own
  dedup set. Behaviourally equivalent.
- **`NotImplementedError` for an unreasoned case** — `optimizers/equal_start.py:243-250`: *"I can't
  think of a scenario where we would end up with 0 indices here."*
- **Stale config comments** — `propeller-solver-core/core/defibot.yaml:623-626` claims `max_depth`
  "supports 2 or 3" (code accepts 1–3, `order_solver.py:71`) and that candidates rank by inertia
  (they rank by weight, `order_solver.py:505`). Mirrored in `solver-config.yaml:193-194`.

## Deliberate divergences

Behaviour that differs from defibot **by choice**, not by oversight.

| Area | defibot | Here | Why |
| --- | --- | --- | --- |
| Solution shape | recursive `FractalRoute` | fixed three levels | see above |
| `inertia` | precomputed `Inertia` (`swaps/interfaces.py:80-95`) — trade size that moves price past a depth threshold | Fynd's derived `ComponentDepths` | same quantity, already computed (`derived/computations/component_depth.rs:1-6`). Explicitly **not** `get_limits`, which is pool exhaustion and ranks concentrated liquidity differently |
| One-hop branch weight | bare `ParallelRoute`, max over pools (`order_solver.py:450-456`) | one-hop route delegates to its hop | the composed formula pairs the *mean* pool price with the *max* pool inertia and can score above every pool, inflating single-hop branches against multi-hop ones |
| Restart bound | unbounded recursion | `MAX_SOLVE_RESTARTS`, then geometric back-off | defibot's implicit bound is Python's recursion limit; the `limit - 1` path can decrement by one for an unbounded number of rounds |
| Back-off from a cast-back limit | `f(e.limit)` (`utils.py:94-95`) | clamped to below the requested amount | a multi-hop limit cast back through spot prices can exceed the request, and the Python loop then makes no progress. defibot guards this in the solver (`order_solver.py:626-647`) but not in the helper |
| Missing gas price | `DEFAULT_GAS_PRICE = 1e-6` (`models.py:29`) | charge zero, rank on gross | the constant is in human units of whichever token, so it means something different per token |
| Buy-token group drop | keeps only the first member (`order_solver.py:535-537`) | keeps all members | no comment or test explains the drop. In this pipeline the two agree anyway: token-sequence grouping runs first, so the buy-token group always has exactly one member |
| Branch ordering | groups in dict insertion order over the weight-sorted path list | the same | group order is therefore by best member's weight. `Branch::weight` exists and is tested but nothing ranks with it, because ranking happens on the token paths before grouping |
| Pruning gas | `minimum_gas` | realised gas of *activated* pools only | no `minimum_swap_gas` equivalent in Fynd. The activated-pools filter is the half that matters; the remainder errs upward, which is the safe direction — see `Sellable::minimum_gas` |
| `_remove_duplicated_routes` | called from the depth-3 grouping (`order_solver.py:551`) | ported, same call site | see below |
| EqualStartV2 iteration budget | unbounded by default (`equal_start_v2.py:24`, `:116`) | finite default | the walk's lattice is exponential in the alternative count and the optimizer has no deadline of its own. defibot's only bound is the `visited` set |
| `_sell_by_splits`, `_round_splits` | v1 helpers (`equal_start.py:327-372`, `:289-302`) | not ported | v2 imports neither (`equal_start_v2.py:10`) and sizes its sells inline at `:133-137`. Their jobs are covered by the running `sell_amount - total_sold` clamp and by exact rationals |
| Path discovery bound | topology filter on `list_paths` (`_market_graph.py:229-236`) | wall-clock deadline plus a path cap | see below |
| Shortfall solutions | returned as-is as a partial fill | refused at assembly | see below |
| Reference connector | hard-coded `WRAPPED_TOKEN` (`order_solver.py:344`) | explicit config, else highest-degree token of the connector allowlist, else highest-degree token in the graph | no address is right on every chain, and the wrapped native token is not always the deepest hub. Pool-edge degree is the signal `fynd derive-connector-tokens` ranks by, so the derived default needs no chain-specific list |
| Reported amounts | the solver's own `buy_amount` | re-derived from the assembled route | see `assemble.rs` |

### `_remove_duplicated_routes` and what it does *not* cover

Ported in `graph_build.rs` and called where defibot calls it, from the grouping (`:551`). It drops a
pool held by two **tails of the same branch**: the tails are parallel, so leaving the duplicate lets
the tail split spend that pool's liquidity twice at once — the same error grouping fixes one level
up. The lowest-weight tail gives the pool up, and a tail whose leg the removal would empty is
dropped whole (`:762-772`). Its trailing `assert` (`:794-796`) is not ported; see *Hygiene*.

Two kinds of sharing it deliberately does not address, both matching defibot:

* **Across branches.** Handled after the splits are frozen: `sell_with_coupled_paths` (`solve.rs`)
  re-sells the branches against each other's post-trade liquidity, and
  `split_primitives::build_split_route` merges a hop shared across branches into a single on-chain
  swap so it is not paid for twice in gas.
* **Between a branch's head and one of its tails.** A pool with more than two tokens can serve both.
  `build_branch`'s `seen_pools` stops it *within* one token path, but the head comes from the
  group's first member only, so a pool absent from that member's first hop can reappear in a later
  member's tail. defibot has the same hole and dedups only within the tail set. Sequential reuse
  under-counts rather than over-counts liquidity, so it errs in the safe direction.

### The topology filter is not ported either

`list_paths` submits candidate paths to `TopologyFilter` whenever a sell amount is given
(`_market_graph.py:229-236`), which needs the encoded price functions served by
`price_function_gw`. Fynd has no equivalent, and defibot's base configuration ships the filter off
(`propeller-solver-core/core/defibot.yaml:620`, `enable_topology_filter: false`) — porting it
faithfully would have produced a disabled feature. Enumeration is bounded directly instead, by a
deadline and a path cap; `graph_build.rs`'s module docs state what truncation costs.

### A shortfall solution is not an exact-in route

**This is a semantic divergence between the two systems, not an implementation detail.** It is the
reason `solve_without_splits` can look, from the inside, like unreachable code. Do not delete it on
that basis; read this first.

**What defibot does.** Its optimizers may return splits summing below one, which is how they say the
market could not absorb the whole order (`optimizers/interface.py:26-31`). `_solve_without_splits`
(`order_solver.py:810-853`) exists precisely to produce such a solution: it is the fallback when
normal solving buys nothing, and it hands the order out greedily over whatever each branch can
individually take, deliberately leaving a shortfall rather than normalising it away.
`solve_order` returns that solution, and the surrounding batch solver treats it as a **partial
fill** — defibot's orders come from batch auctions where filling part of an order is a legitimate,
priceable outcome.

**What we do.** `assemble.rs` refuses any solution routing less than `MIN_ROUTED_FLOW` of the order
and the caller falls through to the other candidate; if neither is assemblable the solve returns
`InsufficientLiquidity`.

**Why exact-in orders make the difference.** Fynd's `Order` is exact-in and its `Route` has no way
to say "this spends less than `amount`". The encoded transaction spends the whole input balance
regardless — `build_split_route` renormalises whatever fractions it is handed so that it does.
Returning a shortfall solution would therefore not produce a partial fill; it would produce a
*full-size* transaction whose per-pool amounts were extrapolated from a solve that sized them for a
fraction of the order. In one port fixture that pushed a pool past its own `reserve_in`. A quote
that misprices its own transaction is worse than no quote.

The practical consequence: `solve_without_splits` can only contribute when its greedy allocation
happens to exhaust the order (splits landing within tolerance of one). Where defibot returns a
partial fill, this returns nothing.

**What would have to change for a partial fill to be returnable.** Three things, in order:

1. `Route` (or `RouteResult`) would need to carry the input it actually spends, distinct from
   `order.amount()` — today the encoder infers the input from the order.
2. `build_split_route` would need a mode that does *not* renormalise, so the root swaps sum to the
   solver's routed amount rather than to the full balance, and the last leg's `split = 0.0`
   remainder convention would have to be reconciled with that.
3. `WorkerPoolRouter` ranks candidates on `amount_out_net_gas` alone; a partial fill would have to
   be comparable against a full one, which is a price-per-unit comparison plus a policy on whether
   an unfilled remainder is acceptable at all.

Until all three exist, refusing is the only answer that keeps the quote and the transaction the same
object.

## Known-faithful weaknesses

Ported as-is because matching defibot comes before improving on it. Revisit only once the port is
at parity.

- **Splits are chosen against an over-optimistic model.** The optimizer scores branches against
  untouched market state, so branches sharing a pool double-count its liquidity;
  `sell_with_coupled_paths` (`utils.py:18-47`) corrects the result *after* the splits are frozen.
  Fynd's `execute_split_plan` could score against a shared overlay directly, which would be more
  accurate but would no longer match defibot's numbers. Grouping removed the *worst* case of this —
  a pool shared by paths leaving the sell token together — but not the general one.
- **Grouping concentrates shortfalls.** Fewer, larger outer splits mean a branch that cannot absorb
  its allocation leaves a bigger hole. On the recorded fixture, grouping improved 20 pairs and cost
  6; every large regression is a solution whose splits summed well below one and was then stretched
  to the full order by `build_split_route`, which either loses quality (`AAVE_to_LINK`,
  `routed_flow=0.147`) or fails outright on a pool's tick range and falls through to the reference
  (`GNO_to_AAVE`, `routed_flow=0.605`, `Ticks exceeded` on `0xf787…b91744`). The shortfall policy,
  not the grouping, is the next thing to change — see *A shortfall solution is not an exact-in
  route*.
- **Loop removal inspects a stale activation set.** `remove_loops` runs between the first solve and
  the per-branch re-solve (`order_solver.py:270-298`), and skips hops that carried nothing. A tail
  the first solve left on a zero split is therefore invisible to it, and the re-solve that follows
  can activate that tail — putting a token pair back in both directions. The encoder then rejects
  the route with `dependency cycle` and the solve falls through to the reference. Observed on
  `UNI_to_LINK`. Safe (a bad route is refused, never quoted) but it costs the candidate. Running
  loop removal on the final activated set would fix it and would depart from defibot's ordering.
- **Loop removal drops a whole branch.** `_remove_loops` (`order_solver.py:884-891`) discards an
  entire top-level split over one bidirectional token pair, with a comment acknowledging the
  trade-off.
- **`seen_pools` is first-fit.** `order_solver.py:467-474` lets the earlier leg claim a shared pool;
  if it was the later leg's only source the whole token sequence is discarded, even when a valid
  assignment exists. A matching problem solved greedily.
- **Rounding remainder.** Each pool receives `floor(amount * split)`, so `P` pools can leave up to
  `P-1` wei unrouted even when the splits sum to one. defibot has the same exposure via its
  `Decimal` → `as_int` conversion. It never reaches a quote: `assemble.rs` re-derives the returned
  amounts from the assembled route, which spends the whole order under the encoder's
  remainder-split convention. See that module's docs for the bound and its direction.

## New notes on defibot found in task 6

- **Production and base configurations disagree on the optimizer.** `solver-config.yaml:198` selects
  `EqualStartV2`; `propeller-solver-core/core/defibot.yaml:629` selects `PairComparison`. Both are
  ported now (`DecompositionConfig::optimizer`) and the default follows the base configuration.
  Moving the default needs a benchmark, not a preference.
- **`enable_topology_filter` is off in the base configuration** (`defibot.yaml:620`) and absent from
  `solver-config.yaml`, so the path filter `list_paths` applies is not obviously live in production
  either.
- **`solve_order` runs a branch-and-bound fine-tune after the decomposition**
  (`order_solver.py:154-166`): `BnBOrderSolver::fine_tune_solution_splits` re-optimises the split
  vector and replaces the solution if it buys more. That is a separate solver, not part of the
  decomposition, and is not ported — the decomposition's output is returned directly here.
- **`solve_order` also caches solutions per `(sell_token, buy_token, sell_amount, min_buy_amount)`**
  (`_get_order_id`, `GeneralPurposeSolutionCache`) and takes protocol fees (`take_fee`). Both are
  concerns Fynd handles outside the algorithm — the worker pool router and the encoder respectively.
