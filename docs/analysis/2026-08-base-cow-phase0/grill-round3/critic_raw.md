# Critic provenance

agent_id: ad426b147816c5d34
subagent_type: general-purpose
model: opus
tool_uses: 57
total_tokens: 185829
duration_ms: 826110
plan_input: digest (~5 KB — complete PHASE 1 v3 section verbatim; full PLAN.md path provided)
note: first launch of this round died at startup on an API usage limit (0 tool uses, no output);
this is the successful relaunch with the identical prompt.

# Critic output

# Findings

## Finding 1
SEVERITY: critical
AXIS: numeric correctness / fail-fast (item 6 "min price ≥ 1e6 with overflow headroom asserted")
ISSUE: The headroom assertion as stated cannot hold, and its violation is **silent**, not a panic.
- Every arithmetic op in apex wraps: `impl Mul for Signed256 { … self.0.wrapping_mul(rhs.0) }` (`apex-solver/src/signed.rs:123-128`), and ruint's `Uint` operators are generated from `wrapping_*` too (`ruint-1.20.0/src/macros.rs:11`, `src/mul.rs:190`, `src/add.rs:234`). There is no debug/release asymmetry and no panic — an overflow produces a wrong objective and wrong prices with no counter, no error, no `deadline_fired`-style flag.
- The dominant term is squared: `objective_value` does `value += (delta * delta).into_raw()` (`src/algorithm/price_search.rs:369-377`) where `delta` is a *value* = amount(18-dec) × price (`src/orderbook/manager.rs:114-121`, `src/orderbook/pair.rs:60-78`, `src/orderbook/clearing.rs:79`). For `delta²` to stay inside `I256`, `|delta| < 2^127.5 ≈ 1.5e38`.
- Working the arithmetic: let `S` = apex price units per USD, so `price = S · usd_per_token`. Then for an order of USD notional `N`, `value = amount_18 × price = (N/usd_per_token × 1e18)(S · usd_per_token) = N · 1e18 · S` — token-independent. `min price ≥ 1e6` forces `S ≥ 1e6 / min_usd_per_token` over the batch.
  - Benign batch (cheapest token $0.01) → `S ≥ 1e8` → the batch is safe up to ~$1.5e12 notional. Fine.
  - The killer is `increase_precision`, which multiplies **every** price by 10 (`price_search.rs:24-35`, `:331-350`) up to `max_precision_increases`, whose default is **10** (`src/configuration.rs:120`). That is a ×1e10 factor on `value`, i.e. `N·S·1e10 < 1.5e20`. At `S = 1e8` the batch notional ceiling collapses to **$150**. With a $1e-9 memecoin in the batch (`S ≥ 1e15`) it collapses below one cent.
- Precision increases are not hypothetical: the trigger is *step* granularity (`smallest_delta ≤ min_absolute_price_change = 1` for `max_it_at_min_step = 30` rounds, `price_search.rs:204-226`), and `decrease_step` divides by 4 per non-improving round (`iterator.rs:132-134`), so step 1 is reached fast. It is orthogonal to how many significant digits the starting price has.
- Aggravating: with `two_hops=ON` (item 12) `all_tokens` is the *whole* market-token set (`algorithm/mod.rs:290-304`), and tokens with zero delta are stepped **downward** every round (`iterator.rs:175-190`, else-branch on `new_demand == 0`), so a large priced token set makes the min-step/precision-increase path fire sooner, not later.
WHY IT MATTERS: The plan's only guard against silent wraparound is an assertion whose two halves (floor 1e6, headroom) are mutually unsatisfiable at the pinned config unless `max_precision_increases` is also pinned low and the assertion is derived as `total_batch_value × 10^P < 1.5e38`. As written, a realistic Base batch silently produces garbage prices, and every downstream number (apex_vs_fynd bps, internalization share) is fiction with no counter to catch it. The v2 "ApexConfig fully pinned" item never states a value for `max_precision_increases`, and v3 item 6 does not connect the two.

## Finding 2
SEVERITY: critical
AXIS: verify-APIs-before-using (item 6 price SOURCE)
ISSUE: The price transform from fynd's derived data to apex's `initial_prices` is under-specified in exactly the two ways that fail silently.
- Type/direction: `TokenGasPrices = HashMap<Address, Price>` (`fynd-core/src/derived/types.rs:45-48`) with `Price { numerator: BigUint, denominator: BigUint }`, documented as "amount of token_out (what you receive)" / "amount of token_in (what you pay)", **including token decimals** (`tycho-common-0.345.1/src/simulation/protocol_sim.rs:87-92`). The computation makes it *tokens received per gas token spent* (`fynd-core/src/derived/computations/token_gas_price.rs:281-296`; the test asserts USDC ratio ≈ 2000 with ETH at 2000, `:1005-1013`).
- Apex prices are the **inverse** orientation: `clearing_price = Fraction::new(prices[sell], prices[buy])` is buy-per-sell (`algorithm/mod.rs:418`, `orderbook/clearing.rs:61-67`), and a limit order clears only when `limit_price ≤ clearing_price` with limit = min-buy-per-sell (`orderbook/limit_order.rs:19-21`) — so a *more valuable* token must have a *larger* apex price, while tycho's `Price` gives a *smaller* numerator/denominator ratio for a more valuable token.
- Decimals: because tycho's rational is in atomic units and apex amounts are lifted to 18 decimals (`apex-solver/src/token.rs:3, 17-31`), the correct transform is `apex_price(t) = S · 10^(dec_t − 18) · den/num`. Item 7 pins the 18-dec contract for *limits* but item 6 says only "normalized to the apex U256 scale per batch" — no inversion, no decimals factor.
WHY IT MATTERS: Getting either wrong is undetectable at runtime — no panic, no exclusion counter; the tâtonnement simply starts from reciprocal or 1e12-off prices and converges to plausible-looking garbage. USDC (6 dec) vs WETH (18 dec) is off by 1e12 from the missing factor alone. Item 6 must state the transform explicitly and anchor it with a property test on a known mixed-decimal pair, the same way item 7 does for limits.

## Finding 3
SEVERITY: high
AXIS: does the resolution actually resolve it (item 11 per-component isolation)
ISSUE: APEX clusters are already the connected components of the **orders + pool-pairs** graph, and `PoolLiquidity` registers both directions of every pool pair (`market/pool_liquidity.rs:28-34`), which `build_graph` folds into `demands`/`supplies` (`algorithm/token_cluster.rs:117-124`) and `split_into_clusters` BFS-walks (`:173-193`). So two orders on completely disjoint pairs land in **one** cluster whenever pools link them. Item 12 pins the subset as "pools adjacent to order tokens ∪ pools linking two order-adjacent tokens (2-hop closure), TVL-capped at K" — on Base, top-TVL WETH/USDC pools survive any cap and hub-connect essentially every order token. Expected component count per block ≈ 1. In that case `run_apex_solver`'s `?` at `algorithm/mod.rs:262` still kills every order in the block on a single `ClearingUnderLimitPrice` / `PostTruncateImbalance` — the failure mode item 11 exists to contain is untouched, and `component_error{kind}` will simply report "1 component, all orders lost".
WHY IT MATTERS: Item 11 is presented as the resolution to a critical round-2 finding, and the plan explicitly declines the alternative ("limits are NOT epsilon-fudged; … goes upstream"). If components are ~always 1, the study has no mitigation at all for a whole-batch abort, and the loss is correlated with exactly the batches that matter (many orders, tight limits). This is measurable *before* building: the connectivity pre-check data plus a candidate K can produce the component-count distribution under the pinned subset filter. Do that first; if it is 1, the mitigation has to be something else (per-order drop-and-retry, or the upstream fix on the critical path).

## Finding 4
SEVERITY: high
AXIS: semantics preservation (item 11, the other horn)
ISSUE: Where per-component isolation *does* bite (≥2 components), it is not semantics-neutral in the pinned `two_hops=ON` config, because three apex code paths are scoped to the whole `MarketRouter`, not to the cluster:
- `all_tokens = market_router.get_market_tokens() ∪ order assets` (`algorithm/mod.rs:290-297`; `market/router.rs:638-645`) — in one call, *every* cluster prices the union of all pool tokens.
- `single_thread_register_supply` iterates **all** `pairs_with_liquidity` on every price-search iteration (`market/router.rs:366-383`).
- `set_upper_bounds` loops over all `pairs_with_liquidity` again in the clearing phase (`algorithm/mod.rs:533-553`), and `select_tokens_to_clear` only prunes tokens that appear in `snd.intents` — priced tokens with no intent at all are never flagged disconnected (`algorithm/mod.rs:450-495`, `snd.rs:51-58`), so they stay in `tokens_to_clear` and inflate the simplex.
So N per-component calls with per-component pools solve a strictly different (smaller-dimension) price-search and simplex problem than one call would, and — via Finding 1's mechanism — a different precision-increase trajectory. Alternatively, passing the *full* pool set to each component call preserves semantics but multiplies the per-iteration cost by N.
WHY IT MATTERS: The plan asserts per-component calling as a pure blast-radius change. It is also a pricing/dimensionality change, so `apex(batch)` results are not comparable across blocks with different component counts, and the offline `two_hops=OFF` secondary cell is not the only axis that moves. State which pool set each component call receives, and note that per-component ≠ single-call results.

## Finding 5
SEVERITY: high
AXIS: pacing arithmetic (items 1 + 11 vs item 4)
ISSUE: Item 4's load model ("≈1 core-s average per 2 s block for both brackets") counts **one** batch solve per bracket. Items 1 and 11 both multiply that count and neither is in the model:
- Item 1's singles control is "every order ALSO solved through APEX as a single-order batch (same config/subset/budget)". With the same subset and `two_hops=ON`, a single-order solve costs about what a batch solve costs (same token universe, same all-pairs supply pass, same all-pairs `set_upper_bounds`) — so per block the work is `2 brackets × (1 + n) × 1 s`, i.e. ≥ 6 core-s per 2 s block at n=2, against a 2-thread owned pool. The queue is bounded, so this manifests as `apex_skipped{queue_full}` on most blocks, not as lag — silently gutting the headline's sample.
- Item 11 adds a further ×(components) and leaves "1 s per batch solve" ambiguous: 1 s per component or 1 s split across components? At 1 s each, the item 3 gate (total wall p99 < 2 s) is violated by construction on any multi-component block.
- Minor but related: item 4 says eligible blocks ≈50%/day while the same section's footer reports the pre-check at 32%; and the singles cell makes *1-trade* blocks eligible too, which the "skip <2 trades" rule was sized against.
WHY IT MATTERS: The whole live-vs-offline split rests on this budget. If singles must run live for state parity with the batch cell, the pool must be sized for `2(1+n)` solves; if singles can run offline from the recording, say so — but then the headline (`apex(batch)` live vs `apex(singles)` offline) inherits the item 19 live/replay divergence bound, which is precisely what item 19 says is an upper bound of unknown size.

## Finding 6
SEVERITY: high
AXIS: replay design (item 15 ring buffer)
ISSUE: The retained object is mis-specified for the job it is meant to do.
- **Wrong subset.** A `w`-block window's batch is the union of orders across blocks `i..i+w`. Solving it at the window-*start* state needs the pool subset for that **union** of order tokens, evaluated at state `i−1`. The ring buffer retains, per eligible block, the subset filtered for *that block's own* orders. Pools required by orders from blocks `i+1..i+w−1` are simply absent from the retained clone, so the w∈{6,30,150} window-start cells cannot be built from it. (Window-*end* is fine — all orders are known by then.)
- **Wrong index.** "eligible blocks only, ≤150 deep" does not address chain blocks: at ~32–50% eligibility, 150 retained eligible blocks reach back ~300–470 chain blocks, and the block 150 chain-blocks back — the actual window start — is frequently *ineligible* and therefore has no retained clone at all.
- Consequently the "bounded ring buffer" memory claim is unverifiable as stated: the object that would actually work is either the full-market state clone (orders of magnitude larger, ×150) or a re-fold from retained raw updates. The plan gives no per-clone byte figure and no measured Base pool count to bound either.
WHY IT MATTERS: Multi-block batching is confirmed required scope, and the whole offline sweep (the only place w>1 lives after v2 item 11 cut live multi-block) depends on this buffer. Item 15 replaced `run_matrix`'s per-block contract (`tools/apex-batch/src/runner.rs:188-201`) with a design that does not produce the window-start cells the study needs.

## Finding 7
SEVERITY: high
AXIS: fail-fast / does the guard guard (item 3 watchdog)
ISSUE: The deadline-checkpoint inventory in item 3 is **correct** (verified: between clusters `algorithm/mod.rs:246-254`, in the price-search loop `price_search.rs:144-148`, once post-search `mod.rs:321-328`; clearing is unguarded). But the proposed mitigation does not do what it is asked to do:
- "Outer watchdog discards results older than 3× budget" bounds *result freshness*, not *occupancy*. There is no cancellation path into `run_apex_solver`; the worker thread stays busy for the full overrun. With a 2-thread owned pool, one overrunning solve removes half the capacity for its whole duration, and the next blocks fall to `queue_full`.
- The unbounded tail scales with the wrong knob. `set_upper_bounds` (`mod.rs:533-553`) and `select_tokens_to_clear` (`mod.rs:450-484`, each pass calling `query_supply_and_demand` over all pairs) are O(subset pools), not O(orders). So shrinking the pool subset K — item 3's stated remedy when the p99 gate fails — is also the only lever on the unbounded phase, and the search-budget knob (1 s) has no effect on it at all. The shadow run must therefore measure clearing-phase wall time as a separate series from search time, and the gate must be on the clearing tail, not just total p99.
WHY IT MATTERS: "shadow run gates on measured TOTAL wall p99 < 2 s, else shrink the pool subset" is the only pacing safety net, and it is stated as if the deadline plus watchdog bound the system. They bound neither thread occupancy nor the clearing phase.

## Finding 8
SEVERITY: medium
AXIS: study design hygiene (item 1)
ISSUE: The batch-vs-singles headline has no stated pairing rule, and both cells can degrade independently and asymmetrically: `deadline_fired` truncates a batch solve at a cluster boundary and drops whole clusters (`ApexResult` doc, `algorithm/mod.rs:45-58`; empty-on-expired-deadline verified at `:246-254`), while a singles solve for the same order may complete; conversely `component_errored` (item 11) can void the batch cell for orders whose singles cell succeeded. Item 2 records `deadline_fired` per solve but nothing says the comparison is restricted to orders where both cells produced a validated (non-`deadline_fired`, non-errored) result.
WHY IT MATTERS: Without an explicit intersection rule, the batching-isolated headline is a selection-biased statistic in the direction that flatters batching (batches that failed to converge drop out; their singles counterparts stay). Since item 1 is flagged "[NEEDS USER/ALAN SIGN-OFF]", the pairing rule should ship with the reframing, not after.

## Finding 9
SEVERITY: medium
AXIS: integration seam (item 13)
ISSUE: The seam itself checks out — `Solver::market_data()` exists and is already used by the monitor (`fynd-core/src/solver.rs:1148-1150`; `tools/hindsight/src/resolve/monitor.rs:137-153`), `advance()` really is inside `resolve_block_range` (`tools/hindsight/src/resolve/mod.rs:344-374`), and the `prices_top`/`prices_back` bracket at `monitor.rs:661/668` is unaffected by a tops/advance/backs split. The problem is the last clause: "keeping `SteppingSolver` mock tests on the composed function". After the split, production runs the three phases from the monitor and *nothing* runs the composed function except the tests. The mock tests then certify a code path no longer used, and any drift between the composed helper's ordering and the monitor's sequencing (e.g. where the N−1 clone and the `advance()` sit relative to `tops`) is invisible.
WHY IT MATTERS: This is a deprecated shim kept alive for its tests, against the project's replace-don't-deprecate and test-behavior-not-implementation rules. Either the monitor keeps calling a composed function that internally takes an "after-tops / after-advance" hook, or the mock tests move onto the three phase functions plus one test of the monitor's sequencing.

## Finding 10
SEVERITY: medium
AXIS: coverage gates (item 5 precondition × price source)
ISSUE: Item 5 drops a whole pool if any of its tokens is unpriced in the N−1 map, and item 6 pins that map to `TokenGasPriceComputation`, whose path search is bounded (`max_hops: 2` by default, `fynd-core/src/derived/computations/token_gas_price.rs:84-91`) and whose entries are dropped when a path is non-viable (`:274-279` returns `SimulationFailed` when gas cost exceeds output). Long-tail Base tokens more than two hops from the gas token, or with thin paths, will have no derived price. Under item 12's 2-hop closure, a single such token in a linking pool removes that pool from the subset. There is a `pool_unpriced` counter but no gate: item 18 sets an explicit <50% escalation threshold for extractor coverage, and nothing equivalent exists for price coverage even though it can silently hollow out the routing graph that the batching result depends on.
WHY IT MATTERS: If price coverage on the 2-hop closure is poor, `apex(batch)` and `apex(singles)` both degrade — but not equally, since the batch's closure spans more tokens. Measure `pool_unpriced` share on a sample block set at the shadow-run step and put a threshold on it, alongside item 18's.

## Finding 11
SEVERITY: low
AXIS: replace-don't-deprecate (item 9)
ISSUE: Item 9's claim is **correct** — `validate_result` no longer unwraps a clearing price; the unpriced case returns `ApexError::UnpricedTokenImbalance` (`apex-solver/src/algorithm/mod.rs:619-625`), so `panic-validate-result.md` is stale. But the scaffold still encodes the stale doc as a hard requirement: `tools/apex-batch/src/runner.rs:170-186` says "**Must wrap the APEX call in `std::panic::catch_unwind`** … see `apex-solver/panic-validate-result.md`", which now contradicts item 9's "catch_unwind only as last resort". The remaining genuine panic sites are different ones (`tokens[&addr]` indexing at `mod.rs:96-108, 312, 315, 331, 417`, and `OrderbookManager::get_order`'s `.unwrap().expect("Order not found")` at `orderbook/manager.rs:141`).
WHY IT MATTERS: Small, but the plan asserts a retraction without a sweep item; the scaffold comment will be read as authority by whoever implements `solve_block`. Retire the doc and rewrite the runner comment against the real panic sites in the same change.

## Finding 12
SEVERITY: low
AXIS: precision of claims (item 6 wording)
ISSUE: "consumed as EXACT rationals … normalized to the apex U256 scale per batch" overstates what survives: `run_apex_solver` takes `initial_prices: HashMap<Address, U256>` (`algorithm/mod.rs:208-215`), so the rationals are rounded to integers at the boundary, and those integers are only the tâtonnement *starting point* — apex then steps them by `price × step / step_size_precision` every iteration (`iterator.rs:154-163`). The exactness buys avoiding f64 double-rounding relative to `snapshot_prices` (`monitor.rs:719-745`), which is real but modest; it does not justify a 1e6 floor on its own (Finding 1 shows the floor is what forces the overflow risk).
WHY IT MATTERS: The scale choice should be argued from the overflow bound, not from a notion of exactness that the API cannot carry. Rephrasing avoids an implementer defending 1e6 as a correctness requirement when it is the opposite.

# Verdict

**2 critical, 5 high, 3 medium, 2 low.**

Several v3 resolutions verified clean and should not be re-opened: item 2's expired-deadline-returns-empty behaviour (`mod.rs:246-254`), item 3's checkpoint inventory (search-only, clearing unbounded), item 7's limit direction and 18-dec contract (`limit_order.rs:19-21`, `token.rs:3-31`), item 8's BTreeSet collapse (`pair.rs:37`, `LimitOrder::Ord` on `(limit_price, id)`), item 9's "panic doc is stale" retraction, item 13's seam availability (`Solver::market_data()`), item 17's `is_partial` field semantics, and — for question (c) — the singles cell is **not** degenerate: a one-order cluster forms as long as a pool covers the pair, because `PoolLiquidity` registers both directions (`pool_liquidity.rs:28-34`) and apex's own test asserts it (`token_cluster.rs:299-312`); a single order with *no* pool correctly yields no cluster (`:289-296`).

But three of the four questions I was asked to stress came back with substantive problems, and two are the kind that produce confidently wrong numbers rather than crashes: the price-scale resolution (item 6) is unsatisfiable at the pinned config and overflows *silently* because every apex integer op wraps; the price-source resolution (item 6) omits both the inversion and the decimals factor; the per-component resolution (item 11) is most likely a no-op in exactly the config item 12 pins, and where it isn't, it changes the pricing problem; and the ring buffer (item 15) retains the wrong object and indexes on the wrong axis for the multi-block windows that are confirmed core scope.

**Another round is needed on items 6, 11 and 15 before the live-stage build.** Items 1/4 (pacing arithmetic) and 3 (watchdog occupancy) should be re-costed in the same pass, since they share the same knob. Findings 8–12 are fine to fold into the build. The 0x Settler extractor remains independent of all of this and can keep moving.
