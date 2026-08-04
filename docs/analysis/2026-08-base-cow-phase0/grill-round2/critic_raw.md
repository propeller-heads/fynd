# Critic provenance

agent_id: a4d9ec4e01b768d3a
subagent_type: general-purpose
model: opus
tool_uses: 49
total_tokens: 165884
duration_ms: 627361
plan_input: digest (~6 KB — authoritative PHASE 1 v2 + v2.1 + v2.2 + v2.3 sections verbatim; full PLAN.md path provided)

# Critic output

# Findings

## Finding 1
SEVERITY: critical
AXIS: Unstated assumptions
ISSUE: Plan item 13 states "`starting_price` never engages — unpriced orders pre-excluded". This is false. `solve_cluster` builds `all_tokens` as `market_router.get_market_tokens() ∪ orderbook.get_assets_with_orders()` when `enable_two_hops` is true (the `ApexConfig::default()` value — `/Users/pistomat/Projects/propeller-heads/apex-solver/src/configuration.rs:139`), then at `/Users/pistomat/Projects/propeller-heads/apex-solver/src/algorithm/mod.rs:299-311`:
```rust
let price = initial_prices.get(addr).cloned().unwrap_or(config.starting_price);
```
`get_market_tokens()` (`src/market/router.rs:638-645`) returns every token of every pool handed in — not just order tokens. Pre-excluding unpriced *orders* does nothing about unpriced *pool* tokens, which will be the large majority of a Base pool subset. Every one of them starts tâtonnement at the constant `100_000_000`.
WHY IT MATTERS: The price search runs over a token set seeded with hundreds of fictitious prices, dragging convergence and clearing prices. Under a 1 s deadline this is the difference between a converged batch and noise — and the plan has explicitly declared the risk closed.

## Finding 2
SEVERITY: critical
AXIS: Blind spots & edge cases
ISSUE: APEX clears **at most one pool per token pair, with no splitting**. `PoolLiquidity::get_pools_supply` takes `max` over the pair's pools (`/Users/pistomat/Projects/propeller-heads/apex-solver/src/market/pool_liquidity.rs:49-62`, doc comment: "Currently, this is only implemented as the maximum supply from a single pool"), and `clear_amount` picks the single pool with the largest `sold_amount` (`:76-95`). Fynd's baseline splits across pools (`fynd-core/src/algorithm/split_primitives.rs`, `water_fill.rs`) and multi-hops. The plan never mentions this asymmetry anywhere, and the headline metric is `apex_vs_fynd_bps`.
WHY IT MATTERS: The headline number is not "surplus from batching". It is batching benefit *minus* the routing-engine gap (splitting + path search), and for any order large enough to move one pool the routing gap dominates. Alan's question ("how much surplus does batch clearing deliver") cannot be answered by this comparison without decomposing the two effects. This is the single most likely way the deliverable is wrong while looking fine.

## Finding 3
SEVERITY: critical
AXIS: Failure modes
ISSUE: v2.3 spawns each solve with a fixed 1 s budget onto a queue and claims "bursts queue instead of lagging". `ApexConfig.deadline` is an **absolute `Instant`** (`configuration.rs:76-77`, `deadline_passed()` at `:102-104`). If the deadline is computed at enqueue time and the task waits in the blocking-pool queue, `run_apex_solver` hits `config.deadline_passed()` on the very first cluster (`algorithm/mod.rs:247-253`), sets `deadline_fired = true`, and returns an `ApexResult` with **empty** `limit_order_clearings`/`pool_clearings`. If the deadline is instead computed inside the closure, the "1 s budget" no longer bounds anything about block N and the queue can grow without backpressure.
WHY IT MATTERS: Exactly the busy blocks that matter most (many trades → most CoW opportunity) are the ones that queue, and they silently produce zero-fill batches indistinguishable from "APEX found nothing". The measured surplus is then biased downward precisely where the interesting signal lives. The plan's `apex_skipped{reason}` counter does not catch this because nothing was skipped.

## Finding 4
SEVERITY: critical
AXIS: Unstated assumptions
ISSUE: The plan never pins APEX's price **numeraire or scale**. APEX prices are bare `U256` integers with no scaling contract in the library — the api test uses ~1e6 magnitude (`/Users/pistomat/Projects/propeller-heads/apex-solver/tests/api.rs:42-44`, "1 USDC" = `1_000_000`) and `starting_price` defaults to `100_000_000`. A token whose value per 18-dec unit falls below one price-unit rounds to **zero**, and zero prices are not defended: `solve_clearings` computes `let buy_amount = remainder / buy_price;` (`src/algorithm/mod.rs:~466`) and `ClearingTarget::get_buy_amount` does `value / self.clearing_price.denominator` (`src/orderbook/clearing.rs:66`) — ruint integer division panics on a zero divisor. `Fraction::new(sell_price, 0)` also poisons the `Ord`/`cmp` comparisons used to gate fills.
WHY IT MATTERS: Base flow is dominated by 0x Settler long-tail tokens; a memecoin at 1e-12 ETH per token is exactly the case that rounds to zero at any sane scale. This is a hard panic inside the batch solve, not a decline, and it will fire on real blocks. Item 13's "price-map coverage stamped per block" measures presence, not magnitude.

## Finding 5
SEVERITY: high
AXIS: Verification gaps
ISSUE: The 1 s deadline is **not a cap on solve time**. It is checked in exactly three places: between clusters (`algorithm/mod.rs:247`), inside the price-search iteration loop (`algorithm/price_search.rs:145`), and immediately after price search (`algorithm/mod.rs:321`). Everything after that runs unbounded: `select_tokens_to_clear` (which calls `demand_oracle.query_supply_and_demand` in a *loop* until the token graph stabilises, `mod.rs:452-470`), `set_upper_bounds` (supply query over every pool), the simplex `trade_solver.solve()`, `market_router.clear_amount` per remainder, `truncate_to_precision`, and `validate_result`.
WHY IT MATTERS: v2.3's entire pacing model ("average APEX load ≈ 0.7 core-s per 2 s block", "critical path ≪ 2 s") is derived from a 1 s hard cap that does not exist. A cluster that clears the price-search checkpoint at t=0.99 s then runs the full clearing phase over a Base-scale pool set. The shadow run must measure *total* solve wall time, not assume the deadline bounds it, and the report's "quality at 1 s" axis is mislabeled.

## Finding 6
SEVERITY: high
AXIS: Unstated assumptions
ISSUE: Plan item 2 claims the clone "also yields the Arc the adapter needs". It does not. `MarketState::extract_subset` returns `simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>>` (`fynd-core/src/feed/market_data.rs:517-523`), and `ProtocolSim::clone_box()` returns `Box<dyn ProtocolSim>`. The adapter's field is `pub pool: Arc<dyn ProtocolSim>` (`tools/apex-batch/src/adapter.rs:42`). The conversion is `Arc::from(box)` — the pattern already used at `fynd-core/src/encoding/encoder.rs:99` — and `Arc::from(Box<dyn T>)` for an unsized `T` allocates a fresh `ArcInner` and **memcpys the whole value**, then frees the box.
WHY IT MATTERS: Clone cost is a named shadow-run line item and the plan has it structurally wrong: it is two full copies of every pool state per bracket, i.e. four per block for the N−1/N brackets, not one. For v3 tick lists and v4 states that is the dominant term.

## Finding 7
SEVERITY: high
AXIS: Unstated assumptions
ISSUE: v2.3 asserts "Fynd's per-trade solves parallelize across trades (worker pool already supports concurrent quotes): n×100 ms → ~100–200 ms wall". Today they do not. `resolve_block_range` at `tools/hindsight/src/resolve/mod.rs:349-355`:
```rust
for trade in trades {
    tops.push(solver.solve(trade.token_in, trade.token_out, trade.amount_in).await);
}
```
Strictly sequential `await` per trade. The back-of-block loop (`:361-373`) is also sequential and additionally interleaves `reexecute` with `solve` per trade. Whatever concurrency the fynd worker pool offers *within* a quote, the monitor's wall time is n × per-quote latency.
WHY IT MATTERS: The plan lists this as an existing property that makes the pacing math work. It is unbuilt work (converting to `FuturesUnordered`/`join_all`), it changes the shared function the mock at `mod.rs:421-455` tests, and until it lands the v2.2 lag ladder is the *primary* mechanism, not a "backstop only".

## Finding 8
SEVERITY: high
AXIS: Dependencies & ordering
ISSUE: The plan repeatedly says "the block loop only `clone_box()`es the filtered pool subset". The block loop (`run_session`, `tools/hindsight/src/resolve/monitor.rs:661-662`) has no such seam — it calls `resolve_block_range`, and `advance()` happens *inside* that function at `tools/hindsight/src/resolve/mod.rs:358`. So the N−1 clone point and the N clone point are both inside `resolve_block_range`. That function is generic over the `SteppingSolver` trait (`mod.rs:267-275`), which exposes only `solve` / `advance` / `reexecute` — **no pool-state accessor**. Market access lives on the concrete `Solver::market_data()` (`fynd-core/src/solver.rs:1148`), reached in `StepAdapter::reexecute` but not through the trait.
WHY IT MATTERS: Wiring APEX in requires either extending `SteppingSolver` (breaking the stepping mock the resolve tests depend on) or hoisting the advance out of `resolve_block_range` and restructuring the monitor loop. Neither is "the block loop clones a subset". This is a real refactor sitting on the critical path of build step 3, unbudgeted.

## Finding 9
SEVERITY: high
AXIS: Failure modes
ISSUE: The plan guards only panics (item 10: `catch_unwind`). But a single cluster returning `Err` aborts the **entire block's batch**: `run_apex_solver` does `let cluster_result = solve_cluster(...)?;` (`algorithm/mod.rs:258`), discarding every cluster already merged into `result`. Reachable errors include `ClearingUnderLimitPrice`, `NegativeBalanceDelta`, `UnpricedTokenImbalance`, `PostTruncateImbalance` (`mod.rs:189-201`) and `MarketRouterError`. None of these are declines; they are whole-batch failures.
WHY IT MATTERS: Blocks with more clusters fail more often, so the surviving sample is systematically skewed toward small/simple batches — the opposite of what the study is trying to measure. The plan's per-order status taxonomy (`{filled, unfilled_at_limit, cluster_cut, excluded_*}`) has no state for "whole batch errored", so these blocks vanish from both numerator and denominator without a counter.

## Finding 10
SEVERITY: high
AXIS: Blind spots & edge cases
ISSUE: `validate_result` recomputes the limit floor independently of how the fill was computed, and mismatched rounding is a hard error. `clear_order` computes `bought_amount = realized_value / clearing_price.denominator` (truncating, `orderbook/clearing.rs:66,84`), while validation computes `min_buy = remove_extra_precision(order.get_limit_amount(sold_amount), buy_token.decimals)` and errors if `bought_amount < min_buy` (`algorithm/mod.rs:578-585`). `remove_extra_precision` is the **identity** for 18-decimal tokens (`src/utils.rs:15-20`), so there is no flooring slack on a WETH-out or 18-dec-token-out order.
WHY IT MATTERS: An order whose constructed limit sits at or within one wei of the clearing price can fail validation and, per Finding 9, kill the whole block. Since the plan's limits come from real extracted `minAmountOut` (0x Settler floors especially) they will sometimes sit exactly at a clearing price. The plan does not specify any epsilon or rounding direction when converting `minAmountOut` into `Fraction`.

## Finding 11
SEVERITY: high
AXIS: Blind spots & edge cases
ISSUE: Pool identity collapses to a 20-byte address. `PoolLiquidity::new` keys `pools: FxHashMap<Address, Pool>` by `pool.address()` (`src/market/pool_liquidity.rs:24-36`), and `PoolMetadata.address` is APEX's 20-byte `Address`. Tycho `uniswap_v4` components are identified by 32-byte pool ids against a singleton PoolManager — there is no distinct 20-byte address per pool. The adapter's only address constructor is `to_apex_address(AlloyAddress)` (`tools/apex-batch/src/adapter.rs:135-137`). uniswap_v4 is in the plan's declared Base **parity** set.
WHY IT MATTERS: Every v4 pool that truncates to the same 20 bytes silently overwrites the others in `pools_map`, and `pool_by_pair` accumulates addresses that resolve to one surviving pool. On Base — where v4 is live and in the headline config — this silently deletes liquidity with no counter. Needs an explicit component-id → unique-`Address` scheme (e.g. keccak-truncate of the component id) plus a collision assertion, neither of which the plan mentions.

## Finding 12
SEVERITY: high
AXIS: Scope
ISSUE: Recording format v2's replay math conflicts with the runner's parallelism design. `run_matrix`'s contract is "replay the recording once per block … Blocks are independent" and fan blocks across rayon workers (`tools/apex-batch/src/runner.rs:153-167`). Under "checkpoint + ≤1 h deltas", each block replay folds up to ~1800 Base blocks of updates. One day = 43,200 blocks × ~900 average deltas ≈ 39 M update applications *per sweep pass*, and the offline matrix is {1,6,30,150} windows × {window-start, window-end} × {parity, full-native}. Sequential forward folding is O(n) but forbids the block-parallel design.
WHY IT MATTERS: This is the offline half of the deliverable and it is quadratic as specified. The plan presents format v2 purely as crash-resilience; it never states that replay cost is the binding constraint on the sweeps, nor picks between "parallel across blocks" and "fold once forward".

## Finding 13
SEVERITY: high
AXIS: Verification gaps
ISSUE: Item 14 ("`is_partial` check on the Base stream is a hard pre-capture gate") is underspecified against how the field actually behaves. `Update.is_partial` (`tycho-simulation-0.345.1/src/protocol/models.rs:186-188`) marks pre-confirmation blocks, and a partial update carries the **same** `block_number_or_timestamp` as the confirmed one that follows. The runner's replay contract says "fold up to and including the update whose `block_number_or_timestamp` is `block`" (`tools/apex-batch/src/runner.rs:190-196`) — with partials there are several such updates. Worse, a component whose state moved in a partial that never confirmed, and which did not change in the confirmed block, keeps the un-confirmed state **forever** in the folded snapshot.
WHY IT MATTERS: "Hard gate" states neither the predicate nor the action. If the Base stream does emit partials, capture is blocked entirely (plan dead) or replay is silently wrong. This needs to be decided before capture starts, not discovered.

## Finding 14
SEVERITY: medium
AXIS: Scope
ISSUE: Format v2 is much larger than one bullet. `write_recording` serialises the whole `MarketRecording` with `serde_json::to_vec` and zstd-compresses it as one blob (`test-fixtures/src/recording.rs:70-76`), and `record_market` accumulates `let mut updates: Vec<Update> = Vec::new();` entirely in RAM (`tools/record-market/src/recorder.rs:87-113`). Segments + checkpoints means rewriting both the writer and the reader, plus building checkpoint construction (which requires the recorder to maintain a folded `MarketState` and serialise `Box<dyn ProtocolSim>` — the same v4 loss as item 6). Separately, `RecordingMetadata.gas_price_wei` is a *single* value captured once at recording start (`recording.rs:30-33`).
WHY IT MATTERS: This is a build item comparable in size to the adapter, and it sits at position 4 of a 6-step order with a Friday deadline. A frozen gas price over a multi-day capture also makes any replay-side gas figure meaningless — tolerable under the gross-vs-gross convention, but it should be stated rather than discovered.

## Finding 15
SEVERITY: medium
AXIS: Verification gaps
ISSUE: The plan's panic rationale is anchored to a stale document. `apex-solver/panic-validate-result.md` describes an `unwrap()` at `src/algorithm/mod.rs:469` on a `balance_sheet` built from `clearing_prices`. That code no longer exists — validation now uses `token_balance_sheet()` and returns `ApexError::UnpricedTokenImbalance` (`src/algorithm/mod.rs:624-646`). The live panic vectors are different and, unlike the documented one, **preventable by construction**: `FxHashMap` index panics on `tokens[&addr]` (`mod.rs:96-97`, `:317`, `:569`, `:624`; `orderbook/manager.rs:36`) whenever the `tokens` vec omits any token referenced by a *pool* (not just by an order), and a zero `limit_price.denominator` divides by zero at `orderbook/pair.rs:143-144`.
WHY IT MATTERS: `catch_unwind` around the solve converts a fixable input-validation bug into a counted mystery. The correct guard is a hard precondition — `tokens` must cover the transitive closure of order tokens *and* every pool token, and `sell_amount > 0` — asserted before the call with an actionable message, per the project's fail-fast convention.

## Finding 16
SEVERITY: medium
AXIS: Unstated assumptions
ISSUE: The limit-price **direction** in the plan is correct, but the **scaling contract** is not stated. Verified: `limit_price` is min-buy-per-sell — `get_limit_amount(sell) = limit_price × sell` is the minimum buy amount (`src/orderbook/limit_order.rs:19-21`), and it is compared against `clearing_price = Fraction::new(price[sell], price[buy])` at `src/orderbook/clearing.rs:71`. So `Fraction::new(min_amount_out, sell_amount)` is the right shape. But it is only dimensionally correct if **both legs are already in APEX's 18-decimal space** — `Fraction::new(raw_usdc_min_out, raw_weth_sell)` type-checks perfectly and is wrong by 10^12.
WHY IT MATTERS: "Limit = minAmountOut policy as Fraction" is the plan's entire specification of the highest-risk conversion in the build, and it omits the one clause that makes it correct. A silent 10^12 error yields orders that always fill or never fill, and both look like plausible study results.

## Finding 17
SEVERITY: medium
AXIS: Blind spots & edge cases
ISSUE: `LimitOrder` carries **no tokens** (`src/orderbook/limit_order.rs:6-12`) — the pair comes solely from the `HashMap<PairAddresses, Vec<LimitOrder>>` key, where `PairAddresses = (sell, buy)` (`src/token.rs:47`). Orders are then stored in a `BTreeSet` ordered by `(limit_price, id)` (`src/orderbook/pair.rs:37`, `limit_order.rs:25-29`), so two orders with equal `limit_price` and equal `id` **silently collapse to one** with no counter. `OrderbookManager::new` also indexes `tokens[&pair.0]` directly (`src/orderbook/manager.rs:36`) — a panic, not a `Result`.
WHY IT MATTERS: `{tx_hash}:{ordinal}` (item 8) makes collision unlikely but nothing enforces uniqueness, and the disappearance is invisible to the per-order reconciliation in item 12 (the order is absent from the *input* the reconciler compares against, if the reconciler reads the built map rather than the pre-map trade list). Worth an explicit uniqueness assertion at build time.

## Finding 18
SEVERITY: medium
AXIS: Failure modes
ISSUE: v2.3's "dedicated APEX thread cap" is not expressible through `spawn_blocking`. Tokio's blocking pool is **runtime-wide** (`max_blocking_threads`, default 512) and shared with every other blocking call in the monitor process — including the JSONL `RotatingWriter` IO. Capping it caps *all* blocking work, not APEX. More importantly, blocking threads are OS threads that compete for cores with fynd's tokio worker threads, so a thread-count cap does not bound CPU contention at all. Separately, `apex-batch` builds apex-solver with the `multithread` feature (`tools/apex-batch/Cargo.toml`), and any `max_workers > 1` makes APEX build a **fresh rayon `ThreadPool` per cluster per solve** (`apex-solver/src/market/router.rs:222`) entirely outside any tokio cap.
WHY IT MATTERS: The stated guardrail for "CPU contention must not silently degrade the timeout-bound Fynd baseline" does not do what the plan says it does. A real cap needs either a `Semaphore` sized to a core budget or a dedicated `rayon`/`std::thread` pool the APEX stage owns. The fynd-solve-time drift metric is a good detector but detects the problem after the data is contaminated.

## Finding 19
SEVERITY: medium
AXIS: Unconsidered alternatives
ISSUE: `enable_two_hops` is labeled "an axis" (item 13), but it is a first-order determinant of both cost and correctness, not a sweep dimension. With it `true` (the default), the price search runs over every token in the pool subset and every unpriced one gets `starting_price` (Finding 1). With it `false`, `all_tokens` collapses to order tokens only (`src/algorithm/mod.rs:301-305`) — cheap and fully priced, but pools reachable only through an intermediary token become unusable. The plan never states which value the headline uses, and never states the **pool-subset filter criterion** that bounds the token set in either case.
WHY IT MATTERS: The two settings measure different mechanisms (pure CoW+direct-pool clearing vs. routed clearing), and the pool filter is the only lever that makes the 1 s budget plausible. Both are load-bearing for the headline and both are currently unspecified.

## Finding 20
SEVERITY: low
AXIS: Verification gaps
ISSUE: v2.1 claims "live-w1 vs offline-w1 measures that [v4] handicap directly". It measures the v4 handicap *plus* every other live/replay divergence: the frozen recording gas price (Finding 14), partial-block folding (Finding 13), differences between the live filtered pool subset and the replayed component set, and price-map provenance (live derived-data snapshot vs. captured snapshot). Nothing isolates the v4 term.
WHY IT MATTERS: A conflated number will be reported as "the v4 handicap" and used to justify the upstream serialization ask. Either isolate it (replay the same blocks with v4 components explicitly dropped from the live run) or label it as an upper bound on the combined divergence.

## Finding 21
SEVERITY: low
AXIS: Dependencies & ordering
ISSUE: The build order puts the 0x Settler floor extractor at step 2 of 6, before the live stage — correct — but the plan also gates the headline on the extracted-limit subset (item 9) without any go/no-go on that subset's *size*. The ≥2-connected-trades/day pre-check at step 0.5 gates cluster formation but not limit coverage.
WHY IT MATTERS: If 0x Settler floor extraction lands at, say, 30 % coverage, the headline is computed on a minority slice and the rest is a synthetic-100bps study — a materially different deliverable that should be discovered at step 2, not step 6. A coverage threshold decided in advance costs nothing and prevents a Friday surprise.

# Verdict

21 findings: 4 critical, 9 high, 6 medium, 2 low. Round 2's four focus areas all came back with real problems — the limit-price *direction* is right but its 18-decimal scaling contract is unstated (F16); the clone claim is factually wrong about `Arc` and understates cost ~2× (F6); checkpoint+delta replay is O(n²) against the runner's own block-parallel design and the `is_partial` gate is undefined (F12, F13); and the async-APEX pacing model rests on three things that are not true — a hard deadline cap that does not exist (F5), fynd trade-solve parallelism that is not implemented (F7), and a thread cap that `spawn_blocking` cannot provide (F18). Beyond the focus list, two independent findings threaten the headline itself: `starting_price` demonstrably does engage (F1), and APEX clears one pool per pair with no splitting while the fynd baseline splits and multi-hops (F2) — that second one means the headline measures batching *plus* a routing-engine gap unless decomposed. **Another round is needed**; F1–F4 and F2's decomposition question in particular should be resolved before any code is written, since F2 may change what the study is measuring.
