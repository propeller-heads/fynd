# Critic provenance

agent_id: a6306622c0db1da45
subagent_type: general-purpose
model: opus
tool_uses: 55
total_tokens: 174942
duration_ms: 593507
plan_input: digest (~4KB; full plan ~20KB at .claude/plans/apex-batching-surplus-analysis.md)

# Critic output

# Findings

## Finding 1
SEVERITY: critical
AXIS: Unstated assumptions
ISSUE: The plan feeds every decoded trade into APEX as a `MarketOrder` (`tools/apex-batch/src/adapter.rs:118` `build_orders` returns `HashMap<PairAddresses, Vec<MarketOrder>>`). But APEX only forms solve units from **limit orders**: `apex-solver/src/algorithm/mod.rs:241` builds `TokenClusterSolver::new(limit_orders.into_iter().collect(), market_pairs).find_trading_clusters()`, and `TokenClusterSolver` holds `orders: FxIndexMap<PairAddresses, Vec<LimitOrder>>` (`src/algorithm/token_cluster.rs:16`). With an empty `limit_orders` map, `find_trading_clusters()` returns an empty vec, the `for (ix, cluster)` loop never executes, and `run_apex_with_config` returns `ApexResult::default()` — zero clearings, zero `pool_clearings`. `MarketOrder`s are consumed only by `MarketRouter` as *supply* alongside pools. Turbine confirms this reading: `turbine/src/clearing_algorithm/apex/solver.rs:704` routes ordinary user orders to `create_limit_order` and reserves `create_market_order` for `is_smart_order() || is_turbine_pool_execution()`; its own test asserts `limit_orders.len() == 4, market_orders.len() == 0`.
WHY IT MATTERS: Every headline number — `apex_vs_fynd_bps`, `internalization_share`, fill status — would be computed from an empty result. The shadow run would also report near-zero solve times and "confirm" the 1 s budget is easily met, hiding the bug. The whole Phase 1 measurement silently produces nothing.

## Finding 2
SEVERITY: critical
AXIS: Failure modes
ISSUE: The plan says the APEX stage solves "at the SAME N−1 state" by wrapping "fynd's LIVE ProtocolSim states" with "no serialization, perfect state parity". Two code facts break that. (a) `resolve_block_range(adapter, &trades, &prices_top)` **advances the solver to N** as part of producing the top/back comparison (`tools/hindsight/src/resolve/monitor.rs:662`, and the comment at :666 says exactly this). Anything running after it reads state N. (b) fynd-core does not hold pool states behind `Arc`: `MarketState.simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>>` (`fynd-core/src/feed/market_data.rs:311`), replaced wholesale by `update_states` under `apply_block_update`'s write lock (:180, :467). There is no way to retain a borrow across the advance. The only N−1 view is an explicit `clone_box()` of every pool of interest **before** `resolve_block_range` — i.e. `extract_subset` / `component_topology` + per-pool deep clone, whose cost the plan never budgets.
WHY IT MATTERS: Written as described, the APEX stage measures against state N while the Fynd baseline is state N−1 — the exact "biased bottom" contamination the plan explicitly rejects for the headline, applied silently and with no way to detect it from the JSONL. And once the pre-advance clone is added, its cost lands directly in the ~2 s Base block budget the plan is already worried about (weak point 9).

## Finding 3
SEVERITY: critical
AXIS: Verification gaps
ISSUE: The comparison budgets are asymmetric by an order of magnitude and the plan never reconciles them. Fynd's per-order re-solve uses `MonitorArgs::timeout_ms` defaulting to `fynd_rpc::config::defaults::WORKER_ROUTER_TIMEOUT_MS`, which is **100 ms** (`fynd-rpc/src/config.rs:209`). APEX gets ~1000 ms for the whole batch. For a 2-trade block that is 200 ms of Fynd compute vs 1000 ms of APEX compute; the "20 s exploratory" cell makes it 200×. `MonitorArgs.timeout_ms`'s own doc comment (`monitor.rs:96-99`) warns that a generous budget "silently hands the re-solve more time than any production quote gets — overstating savings" — the plan does the same thing in APEX's favour and does not flag it.
WHY IT MATTERS: `apex_vs_fynd_bps` is meant to isolate "what batch clearing adds on top of Fynd". As specified it also contains "what 5–200× more search time adds", and there is no control run (e.g. Fynd at 1000 ms/order, or APEX at 100 ms × n_trades) to separate them. The headline number is not attributable to batching.

## Finding 4
SEVERITY: high
AXIS: Unstated assumptions
ISSUE: APEX has no gas model. `ApexResult` carries `clearing_prices`, `limit_order_clearings`, `market_order_clearings`, `pool_clearings` (`apex-solver/src/algorithm/mod.rs:39-58`) — no gas anywhere. The Fynd side has both `amount_out` and `amount_out_net_gas` (`SolvedAmount`, surfaced in `capture::FyndCounterfactual`), and the settled side is `settled_amount_out_net_gas`. The plan's metric spec says only "`apex_amount_out`, `apex_vs_fynd_bps` (vs the same trade's fynd top quote)" without saying gross-vs-gross or gross-vs-net.
WHY IT MATTERS: If `apex_vs_fynd_bps` compares APEX gross to Fynd net-of-gas, APEX wins by roughly the gas share of every trade — on Base that is small in bps for large trades but dominant for the long tail, and it is a pure artifact. If it compares gross-to-gross, then the claim "APEX adds value on top of Fynd" ignores that a batch settlement's gas is real and differently distributed (one settlement vs n routes). Either choice needs to be stated and defended; leaving it unspecified means the number can silently be either.

## Finding 5
SEVERITY: high
AXIS: Blind spots & edge cases
ISSUE: The headline `internalization_share = 1 − pool-cleared volume / order volume` is arithmetically unsound as defined. `PoolClearing` is per-pair (`address, pair, sold_amount, bought_amount, surplus, fee` — `apex-solver/src/market/pools/models.rs:20`), so a multi-hop route through 2–3 pools emits 2–3 clearings whose summed volume exceeds the order volume, driving the share negative. The plan simultaneously claims 66% of the Phase 0 opportunity is "route-mediated" — i.e. precisely the multi-hop case that breaks the formula. Separately, `sold_amount`/`bought_amount` are in different tokens across clearings and in APEX's 18-decimal space, so any "volume" sum requires a USD conversion the metric definition doesn't mention.
WHY IT MATTERS: One of the two confirmed headline metrics, compared against a Phase 0 ceiling (1.7% @1 block, 23% @1 min), would be computed by a formula that can return values outside [0,1] and whose units don't compose. This needs a defined denominator (USD notional at the block price view) and a numerator that counts each order's *net* external pool exposure, not the sum of hop legs.

## Finding 6
SEVERITY: high
AXIS: Verification gaps
ISSUE: The plan states "Base has no vm:* protocols so recordings are complete there". That is false for the plan's own Base protocol list. `UniswapV4State` carries `impl_non_serializable_protocol!(UniswapV4State, "not supported due vm state deps")` (`tycho-simulation-0.345.1/src/evm/protocol/uniswap_v4/state.rs:85`), whose macro (`src/serde_helpers.rs:129`) makes both `Serialize` and `Deserialize` unconditionally error. `Update.states` is serialized with `crate::serde_helpers::protocol_states`, documented as "VM-backed states that can't be serialized are **silently skipped**" (`src/protocol/models.rs:191-195`), while `new_pairs`/`removed_pairs` (plain `ProtocolComponent`) serialize fine. I checked the other Base natives (`aerodrome_v1`, `aerodrome_slipstreams`, `lunarbase`, `uniswap_v2/v3`, `pancakeswap_v2`) — none use the macro, so v4 is the specific hole.
WHY IT MATTERS: Every offline replay (multi-block route (a), the config-A/B sweeps, the 20 s budget runs) loses all Uniswap v4 liquidity on Base *silently*, and the replayed state will contain v4 *components with no states* — a shape `market_state_at_block` must explicitly handle or it will build half-pools. Worse, the live path (which uses in-process states) keeps v4 while the offline path drops it, so live and replayed results are not comparable, which is the one property the "capture once, replay many" architecture exists to provide.

## Finding 7
SEVERITY: high
AXIS: Scope / Dependencies & ordering
ISSUE: The recording format cannot carry days of Base blocks. `MarketRecording { metadata, updates: Vec<Update> }` is held entirely in memory and written as one blob: `serde_json::to_vec(recording)` then `zstd::encode_all` (`test-fixtures/src/recording.rs:74-79`); `read_recording` decompresses and deserializes the whole thing. `record_market` accumulates `updates: Vec<Update>` for a fixed `duration_secs` and constructs the `MarketRecording` only at the end (`tools/record-market/src/recorder.rs:90-143`) — nothing is flushed incrementally, and a crash or OOM loses the entire run. Base is 43,200 blocks/day. On top of that, `runner::market_state_at_block` is documented to "fold `recording.updates` in order up to and including" block k, which is O(k) per block — O(n²) over a run — and directly conflicts with `run_matrix`'s rayon fan-out across blocks (each worker would need its own full fold, or the fold must be sequential and the parallelism disappears).
WHY IT MATTERS: "Capture on from day one" plus "multi-block windows up to 150 blocks" plus "days of Base blocks" (implementation queue item 9) is not achievable with this format. This is a foundational dependency for route (a) of the multi-block work and it needs either a chunked/append format or a checkpointed replay before any capture is started — starting capture first and discovering this later wastes the calendar time capture was started early to buy.

## Finding 8
SEVERITY: high
AXIS: Blind spots & edge cases
ISSUE: Order identity is transaction-hash-based and collides. The plan sets `MarketOrder.id = transaction hash` (`adapter.rs:113-116`). But hindsight emits multiple `DecodedTrade`s per transaction (multi-order CoW settlements are explicitly modelled — Allium finding #3 in the plan says 3.8% of settlements; nested routing puts several projects in one tx per finding #4). APEX stores orders in a `BTreeSet<MarketOrder>` ordered by `(execution_price, id)` (`apex-solver/src/market/market_orders.rs:32-40`) — two orders in the same tx, same pair, same price collapse to one silently. And `MarketOrderClearing.id` cannot be joined back to a unique trade. There is already a matching bug in the scaffold: `capture::captured_trades` joins ranges to trades with `trades.iter().find(|trade| trade.tx_hash == range.tx_hash)` (`tools/hindsight/src/capture.rs:143`) — every range from a multi-trade tx receives the **first** trade's `min_amount_out`.
WHY IT MATTERS: The exact flow where batching should shine (multi-order settlements) is the flow whose orders get merged, mis-limited, or unattributable. Silent order loss also corrupts the `internalization_share` denominator.

## Finding 9
SEVERITY: high
AXIS: Dependencies & ordering
ISSUE: On Base, essentially every order's limit will be synthetic. The only implemented extractor is CoW's, and it is doubly restricted: `order_min_amount_out` returns `None` unless the settle call carries exactly one trade (`tools/hindsight/src/decoder/intents/cow.rs:89-95`, test at :330), and the `CowSettlement` decoder itself bails out entirely on multi-trade transactions (:114-118). The three other extractors are stubs returning `None` (`solvers/zeroex.rs:53`, `solvers/paraswap.rs:56`, `solvers/kyberswap.rs:53`). The plan's own Allium finding #9 says Base flow is 0x-Settler dominant with CoW not among the leaders. So `limit_source` will be `Synthetic` for nearly all Base orders, i.e. `executed_out × (1 − 100 bps)`.
WHY IT MATTERS: `MarketOrder`/`LimitOrder` fills are all-or-nothing at the limit (`MarketOrders::query_supply` adds the whole `sell_amount` when `swap_price >= execution_price` and `break`s otherwise — `market_orders.rs:88-101`), so the limit *is* the fill decision. A headline surplus computed almost entirely against a 100 bps assumption is an assumption result, not a measurement — and the plan's Phase 1 build order puts the live single-block stage (step 2) before, not after, the extractors. The sequencing should be inverted, or the headline explicitly gated on the extracted-limit subset.

## Finding 10
SEVERITY: high
AXIS: Failure modes
ISSUE: `run_apex_with_config` is a synchronous, CPU-bound call that builds its own rayon thread pool per cluster (`apex-solver/src/market/router.rs:222`, gated on the `multithread` feature which `tools/apex-batch/Cargo.toml:10-13` enables). The plan places it inline in the monitor's `run_session` loop, which is `async` and shares a tokio runtime with the tycho stream. Blocking a runtime worker for a full 1 s (or the multi-block `N×2s` budget) per block is exactly the backpressure condition the monitor's own constants document as a known feed-death mode: "backpressure kills the remaining subscriptions (`Buffer full, unsubscribing!`). Nothing resubscribes" (`monitor.rs:43-48`). The plan mentions neither `spawn_blocking` nor the runtime interaction.
WHY IT MATTERS: The failure is not a crash — it is a rebuild loop (`rebuild_after_feed_death`, minutes of token loading each time) that eats the measurement window, and the resulting data gap looks like "quiet blocks" rather than an instrumentation failure.

## Finding 11
SEVERITY: high
AXIS: Unconsidered alternatives
ISSUE: Live `--batch-window-blocks N` mode is architecturally at odds with the monitor's stepping model. `run_session` is a strict sequential stepper: it decodes block N, solves every trade, advances, re-solves, and only then moves on. Accumulating N blocks and then spending "up to ~N×2s−overhead" on one APEX solve means the monitor consumes ~2× the wall-clock of the window it covers, so head-lag grows monotonically until `max_lag_blocks` (600 on Base, per the test at `monitor.rs:770`) triggers a solver rebuild — which discards the accumulator and restarts at current head. There is no throttle, no skip-ahead, and no lag budget analysis in the plan.
WHY IT MATTERS: The live window mode as specified will spend most of its time in rebuild cycles rather than producing window results, while the *same* measurement is already covered — better, with any budget and identical states — by the offline replay sweeps (route (a)). Route (b) looks like scope that can be dropped, and dropping it removes the plan's hardest pacing constraint.

## Finding 12
SEVERITY: medium
AXIS: Failure modes
ISSUE: `catch_unwind` around the solve is necessary but insufficient, and the plan treats it as the whole mitigation. (a) `validate_result` has more panic sites than the documented one: `tokens[addr]` indexing in the price/clearing loops and `orderbook_manager.get_order(pair, &clearing.id)` (`apex-solver/src/algorithm/mod.rs:566-580`); `truncate_to_precision` indexes `tokens[&clearing.sell_token]` similarly (:93-108); `select_tokens_to_clear`/`solve_cluster` index `tokens[address]` at :651 and :654. Any token reachable through a pool but absent from the `Vec<Token>` the adapter built panics. (b) `crate::instrument::reset()` / `snapshot()` are process-global (`algorithm/mod.rs:217`, :270) — a caught panic leaves them mid-solve, so the *next* block's `SolveMetrics` are garbage. (c) `catch_unwind` needs `AssertUnwindSafe` for the borrowed pools/orders.
WHY IT MATTERS: `adapter::apex_tokens` is documented to build tokens "referenced by the block's orders and pools" — if the pool set is filtered independently of the token set (which pool-count controls like top-K-per-pair will do), the mismatch is a panic per block, not a rare one. And silently corrupted metrics undermine the shadow-run timing conclusions.

## Finding 13
SEVERITY: medium
AXIS: Unstated assumptions
ISSUE: The plan describes the deadline as "returns best-so-far at checkpoints, so runs always produce *something*". The actual semantics are coarser and are documented on the type: when `deadline_fired`, "the trade-clearing vectors ... only contain entries from clusters that completed fully before the deadline. Clusters that hadn't started yet are missing entirely" and mid-cluster the result is prices only, with `..Default::default()` for all clearings (`apex-solver/src/algorithm/mod.rs:44-56`, :321-327). So at 1 s on Base the likely outcome is not "slightly worse prices" but "some or all orders absent from the result, unvalidated prices".
WHY IT MATTERS: An order absent because its cluster was cut is indistinguishable in the JSONL from an order that entered the batch and didn't clear (`apex_amount_out = 0`) unless the runner explicitly reconciles the input order set against `market_order_clearings`. Conflating them biases both `apex_vs_fynd_bps` (dropped orders excluded from the mean) and `internalization_share` (denominator includes orders APEX never touched). The metric spec has `status: solved/best-so-far-timeout/...` at the *batch* level only, not per order.

## Finding 14
SEVERITY: medium
AXIS: Unstated assumptions
ISSUE: `ApexConfig` defaults are never pinned by the plan and several matter. `enable_two_hops: true` (`apex-solver/src/configuration.rs:139`) makes `all_tokens` the union of *every* market token with the order tokens (`algorithm/mod.rs:288-293`) — on Base with `min_tvl = 100` that is the dominant runtime driver and the thing pool-count controls actually need to target. `max_workers: 4` contradicts the plan's "solve single-threaded per batch for reproducibility" (and `tools/apex-batch/Cargo.toml` enables the `multithread` feature). `starting_price: U256::from(100_000_000)` is substituted for any token missing from `initial_prices` (:287) — a silent default that directly contradicts the plan's `TokenUnpriced` exclusion policy unless the adapter filters those orders out *before* the call.
WHY IT MATTERS: An unpriced token silently getting a fabricated starting price is exactly the "sentinel value" failure the project conventions forbid, and it produces a plausible clearing rather than a counted exclusion.

## Finding 15
SEVERITY: medium
AXIS: Verification gaps
ISSUE: Nothing in the plan validates the adapter against ground truth before the numbers are produced. `TychoApexPool::query_supply` and `get_amount_out` are `todo!()` (`tools/apex-batch/src/adapter.rs:58`, :71), as are `initial_prices`, `build_orders`, `apex_tokens`, `market_state_at_block`, `solve_block`, `run_matrix`. Every scaffolded test in `adapter.rs`, `scaling.rs`, and `runner.rs` is `#[ignore]`d. The decision table promises "property tests incl. direct-vs-adapter `ProtocolSim` agreement" but the Phase 1 replan's build order (shadow → live → capture → multi-block → report) never schedules them. The two conversions in `query_supply` — price *inversion* (APEX `sell/buy` vs ProtocolSim `token_out/token_in`) and the counter-intuitive *upward* precision lift for low-decimal tokens (documented at `turbine/src/clearing_algorithm/apex/solver.rs:1121-1160`) — are both silently-wrong-by-10^12 if flipped.
WHY IT MATTERS: A reversed price direction or a missed rescale yields a fully-populated result with plausible-looking bps, no error, and no tripwire. This is the failure mode most likely to survive to the Friday report.

## Finding 16
SEVERITY: medium
AXIS: Verification gaps
ISSUE: The plan treats `snapshot_prices` output as "the price map at N−1". It is not a block-anchored quantity: it reads `solver.derived_data()` (`monitor.rs:719-747`), which is produced asynchronously by the derived-data manager and can lag the applied block, and it silently drops any token whose numerator/denominator won't parse as `f64` or whose denominator is ≤ 0. Nothing records how many tokens the map covered for the block, and the capture record (`capture::BlockBatchSnapshot.token_prices`) stores it without a freshness stamp.
WHY IT MATTERS: The plan's `TokenUnpriced` exclusion reason is the mechanism by which coverage is reported, but with no measured baseline for map coverage on Base there is no way to tell "APEX excluded 40% of orders because Base tokens are thin" from "the derived map hadn't caught up". Both look identical in the counters.

## Finding 17
SEVERITY: medium
AXIS: Blind spots & edge cases
ISSUE: `Update.is_partial` ("True when this update is for a partial (pre-confirmation) block" — `tycho-simulation/src/protocol/models.rs:186-188`) is referenced nowhere in `fynd-core/src` or `tools/` (grep returns zero hits). Base is the plan's target chain and is the one with sub-block pre-confirmation semantics. `runner::market_state_at_block` is specified to fold "up to and including the update whose `block_number_or_timestamp` is `block`", which is ambiguous when several partial updates share a block number.
WHY IT MATTERS: If the Base Tycho stream emits partial updates, the monitor's `advance()` barrier (which waits for `last_updated().number` to *increase*) and the replay's "the update for block k" both have undefined behaviour, and the resulting state mismatch between live and replayed runs is untraceable. This needs to be confirmed against the Base feed before capture starts, not after.

## Finding 18
SEVERITY: medium
AXIS: Scope
ISSUE: The dependency is a machine-local absolute path: `apex-solver = { path = "/Users/pistomat/Projects/propeller-heads/apex-solver", features = ["serde", "multithread"] }` (`tools/apex-batch/Cargo.toml:10`), with its own `# TODO: swap to git dependency before PR`. The plan's decision table says git dependency, and the open question lists "deploy path (local → staging k8s beside hindsight)". Additionally the cited `apex-solver/panic-validate-result.md` is **untracked** in that repo (`git status` shows `?? panic-validate-result.md`), so it is a local artifact, not a shared reference. Extending the monitor also means `tools/hindsight/Cargo.toml` — which today has no APEX dependency — inherits the path dep into the deployable binary.
WHY IT MATTERS: `./check.sh` on any other machine or in CI fails outright, and the k8s deploy path is blocked on Docker build credentials for a private repo that nobody has scoped. This is cheap to fix now and expensive to discover on deploy day.

## Finding 19
SEVERITY: medium
AXIS: Verification gaps
ISSUE: The Phase 0 ceilings and the Phase 1 measurements are computed over different universes and the plan compares them directly ("headline metrics vs Phase 0 ceilings: ... internalization share (vs 1.7% cap @1 block, 23% @1 min)"). Phase 0 measured Allium `dex.trades` intents netted per-tx across *all* Base DEX activity (105k intents/day, $54.8M). Phase 1 measures only what hindsight decodes — solver/venue flow matched against `registry/base.toml`'s `[solvers]` and `[venues.*]` tiers, a strict and much smaller subset (the plan itself lists two 0x Settlers still unverified and Bebop only just added).
WHY IT MATTERS: An internalization share of, say, 5% measured over hindsight-decoded flow is not comparable to a 1.7% cap measured over all Base intents — the denominators differ by an order of magnitude and the numerators draw from different order populations. Without an explicit reconciliation (what fraction of Phase 0's intent volume hindsight decodes), the "vs ceiling" framing can produce a headline that reads as >100% of a theoretical cap.

## Finding 20
SEVERITY: medium
AXIS: Scope
ISSUE: The plan never estimates how many Base blocks will actually have ≥2 decoded trades on *connected* pairs — the precondition for a batch to differ from n independent solves. Phase 0's own finding is that only 3.5% of blocks have any pairwise match, and APEX additionally discards disconnected tokens (`find_disconnected_tokens` drops any token lacking both supply and demand — `token_cluster.rs:129-140`), so many 2-trade blocks will collapse to single-order clusters where APEX ≈ Fynd by construction.
WHY IT MATTERS: Step 2 (live single-block stage) may be structurally incapable of producing signal above noise, and it is scheduled before the multi-block work that Phase 0 says is where the value is. A cheap pre-check — count blocks/day with ≥2 trades sharing a cluster, from decode data you already have — would either justify step 2 or let you skip straight to the window sweeps and buy back a day of the schedule.

## Finding 21
SEVERITY: low
AXIS: Unstated assumptions
ISSUE: The plan sources `internalization_share` "from `ApexSolution` pool_clearings". `ApexSolution` is a **turbine** type (`turbine/src/clearing_algorithm/apex/solver.rs`, built by turbine from an `ApexResult`); apex-solver's public API returns `ApexResult` (`apex-solver/src/lib.rs:46-56`) and turbine's `ApexSolution` carries `cleared_pools`, not `pool_clearings`.
WHY IT MATTERS: Small, but it is the second place (after `deadline_ms`, already corrected in the scaffold notes) where the plan's APEX API facts came from turbine rather than the dependency, which suggests the remaining API assumptions deserve the same verification pass.

## Finding 22
SEVERITY: low
AXIS: Scope
ISSUE: The live stage and the offline matrix now measure overlapping things through two independent code paths. `runner::Position::{Top, BiasedBottom}` and the 8-cell matrix (`runner.rs:31-70`) exist for the offline runner; the live stage does N−1 only. `RunConfig::derive_label`'s own test hardcodes `10_000ms`/`20_000ms` budgets (`runner.rs:216`) while the plan's live budget is 1000 ms.
WHY IT MATTERS: Two implementations of "build APEX input from a block" (live adapter over in-process states, offline adapter over replayed states) will drift, and a discrepancy between them will be indistinguishable from a real live-vs-replay difference. Worth deciding now whether the live stage shares `build_orders`/`initial_prices`/`apex_tokens` with the offline runner or whether the offline matrix is cut.

# Verdict

22 findings: 3 critical, 8 high, 9 medium, 2 low. Findings 1 and 2 are the ones that matter most — as written, the plan feeds orders into APEX in a form the solver structurally ignores (producing an empty result that the shadow run would misread as "fast"), and it schedules the APEX solve after the point where the monitor has already advanced past the N−1 state it claims to measure at. Finding 3 (100 ms Fynd vs 1000 ms APEX) means even a working implementation would not measure what the plan says it measures. The scaffolding is careful and the honesty discipline around coverage/exclusions is genuinely good, but the APEX-side API assumptions were inherited from turbine rather than verified against apex-solver, and that is where the plan breaks.
