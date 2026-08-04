# Grilling log (round 1)

## Statistics
- Findings raised: 22
- By severity: critical=3, high=8, medium=9, low=2
- Resolved: 20 (incl. 1 resolved-by-cut)
- Deferred: 1
- Disputed: 1
- Subsumed: 0
- Critical/high deferral rate: 0%

## Finding 1 — critical — Resolved
**Issue:** Decoded trades fed as `MarketOrder`s, which APEX treats as supply, not solve units — `run_apex_with_config` returns an empty result.
**Action:** Plan rewritten: decoded trades become **`LimitOrder`s** (limit from the minAmountOut policy), matching turbine's user-order path (`create_limit_order`). `adapter::build_orders` signature changes accordingly; `MarketOrder` path deleted from the adapter (no speculative second path).

## Finding 2 — critical — Resolved
**Issue:** APEX stage scheduled after `resolve_block_range`, which advances the solver to N; fynd-core states are `Box<dyn ProtocolSim>` replaced wholesale — no borrow survives the advance.
**Action:** Plan reordered: APEX stage runs **before `advance`**, between the fynd top-solves and `solver.advance()`, on a **`clone_box()`d filtered pool subset** taken at N−1 (needed anyway to obtain `Arc<dyn ProtocolSim>` for `TychoApexPool` and to avoid holding the market read lock during the solve). Clone cost is now an explicit shadow-run measurement with a per-block budget line.

## Finding 3 — critical — Resolved
**Issue:** 100 ms/order Fynd vs 1000 ms/batch APEX — headline conflates batching value with extra search time.
**Action:** Budget policy pinned: **primary cell = equal compute** (`apex_deadline = n_orders × fynd timeout_ms`), secondary cell = fixed 1 s (the sequencer-realism number). Both recorded per batch with the budget in the JSONL so every bps figure is attributable. The 20 s cell exists only offline and is labeled quality-ceiling, never compared to the 100 ms Fynd baseline as "batching value".

## Finding 4 — high — Resolved
**Issue:** Gas basis unspecified (APEX has no gas model; Fynd has net-of-gas).
**Action:** Pinned: `apex_vs_fynd_bps` is **gross-vs-gross** (mirrors hindsight's verdict convention and its rationale at `compare.rs:127-132`); Fynd's `amount_out_net_gas` and settled net are still recorded for offline analysis; batch-settlement gas explicitly out of scope for Phase 1 metrics and listed as unmeasured in the report template.

## Finding 5 — high — Resolved
**Issue:** `internalization_share` formula unsound: per-hop pool clearings double-count multi-hop routes; cross-token sums need USD.
**Action:** Metric redefined: per token, **net** pool exposure = |Σ pool-clearing flows per token| aggregated to net (not summed per hop), valued in USD at the N−1 price map; `internalization_share = 1 − Σ_t net_external_usd / (2 × order_notional_usd)`, bounded to [0,1] with a unit test asserting a 3-hop single-order batch yields ≈0.

## Finding 6 — high — Resolved
**Issue:** `UniswapV4State` is non-serializable (`impl_non_serializable_protocol!`) — Base recordings silently lose all v4 liquidity; live vs replay incomparable; plan's "Base recordings are complete" claim false.
**Action:** Plan text corrected. Consequences pinned: (a) offline sweeps are **config-A/B only** (both cells share the v4 hole; absolute levels not claimed); (b) live-vs-replay parity explicitly not claimed for v4-touching blocks; (c) capture stores the per-block v4 component list so replay knows what is missing; (d) upstream ask filed with the tycho team for v4 state serialization (the real fix); (e) `market_state_at_block` must skip components with missing states, counted.

## Finding 7 — high — Resolved
**Issue:** Monolithic in-memory `MarketRecording` can't carry days of Base; O(k) fold per block conflicts with rayon fan-out.
**Action:** Recording format v2 specced before capture starts: append-only frame-per-update segments (hourly files, zstd per segment, incremental flush — crash loses ≤1 segment) + periodic full-state checkpoints; replay = nearest checkpoint + ≤1h of deltas; rayon parallelism across checkpoint spans with one shared fold per span. Shadow phase measures bytes/day before always-on capture is enabled.

## Finding 8 — high — Resolved
**Issue:** Order id = tx hash collides for multi-trade transactions; `BTreeSet` silently merges equal `(price, id)`; existing `captured_trades` first-match join bug.
**Action:** Order id = `{tx_hash}:{ordinal}` (per-tx trade ordinal); `captured_trades` join keyed on the same composite; a debug assert that input order count == distinct ids. (Today hindsight emits ≤1 trade/tx, so the ordinal is 0 everywhere — the id scheme makes the future multi-order CoW support safe rather than silently wrong.)

## Finding 9 — high — Resolved
**Issue:** Nearly all Base limits will be synthetic (only CoW extractor implemented; CoW is minor on Base; 0x Settler dominant); limits ARE the fill decision; build order puts live stage before extractors.
**Action:** Build order inverted: the **0x Settler floor extractor moves before the live stage** (it alone covers the dominant Base flow), kyber/paraswap follow; headline metrics **gated on the extracted-limit subset**, synthetic-limit results reported as a separate labeled slice with 50/100/200 bps sensitivity.

## Finding 10 — high — Resolved
**Issue:** Synchronous CPU-bound APEX solve inline in the async monitor loop → tycho backpressure feed-death → rebuild loops that look like quiet blocks.
**Action:** APEX solve runs in `tokio::task::spawn_blocking` (owning its cloned inputs), awaited with a hard timeout; rayon inner threading disabled live (`max_workers=1`). A watchdog counter (`apex_overruns_total`) distinguishes solver overruns from feed problems.

## Finding 11 — high — Resolved (by cut)
**Issue:** Live `--batch-window-blocks` mode consumes ~2× wall-clock of its window → permanent head-lag → rebuild churn; measurement already covered better offline.
**Action:** **Route (b) cut from the plan.** Multi-block is exclusively offline replay sweeps. This also removes the hardest pacing constraint and the accumulator/lag design entirely.

## Finding 12 — medium — Resolved
**Issue:** `catch_unwind` insufficient: more panic sites via `tokens[..]` indexing on token/pool set mismatch; process-global instrument state corrupted for the next solve; `AssertUnwindSafe` needed.
**Action:** Adapter builds the token set as the **closure** of (order tokens ∪ filtered-pool tokens) and declines the batch (counted `TokenSetIncomplete`) if closure fails — panics become preconditions checked before the call. `AssertUnwindSafe` noted in the spec. `SolveMetrics` from the solve after any caught panic are discarded and counted.

## Finding 13 — medium — Resolved
**Issue:** Deadline semantics: clusters cut by the deadline are absent from the result — indistinguishable from unfilled orders; per-order status missing.
**Action:** Per-order reconciliation added: input order set diffed against clearings; per-order status enum {filled, unfilled_at_limit, cluster_cut, excluded_<reason>}; means computed over filled+unfilled only, with cluster_cut share reported per batch.

## Finding 14 — medium — Resolved
**Issue:** `ApexConfig` defaults unpinned: `enable_two_hops:true` is the runtime driver; `max_workers:4` contradicts single-threaded reproducibility; `starting_price` sentinel silently prices unpriced tokens.
**Action:** Full `ApexConfig` pinned explicitly in code and in the run label. Orders/pools with any token missing from `initial_prices` are excluded **before** the call (counted `TokenUnpriced`) so the sentinel never engages; `two_hops` is a labeled config axis in the shadow run; `max_workers=1` live.

## Finding 15 — medium — Resolved
**Issue:** No adapter ground-truth validation scheduled; price inversion and decimal lift are silently-wrong-by-10^12 risks.
**Action:** Build order now starts with **step 0: implement and de-`#[ignore]` the adapter property tests** (direct `ProtocolSim` call vs through-`TychoApexPool` agreement per protocol on live-fetched states, both directions, mixed decimals) — before the shadow run, which would otherwise time a wrong adapter.

## Finding 16 — medium — Resolved
**Issue:** `snapshot_prices` is not block-anchored (async derived data, silent drops); no coverage baseline recorded.
**Action:** Capture and JSONL record per block: price-map token count, the derived-data block stamp, and per-order `TokenUnpriced` counts — making "thin token" vs "stale map" distinguishable. Freshness stamp added to `BlockBatchSnapshot`.

## Finding 17 — medium — Resolved
**Issue:** `Update.is_partial` unhandled anywhere; Base pre-confirmation semantics could break both the advance barrier and replay folding.
**Action:** Added to the shadow phase as a hard pre-capture check: observe the Base stream for `is_partial` updates; if present, monitor barrier and replay fold key change to `(block, !is_partial)` with partials dropped from recordings (documented). Capture does not start until this is answered.

## Finding 18 — medium — Resolved
**Issue:** Machine-local absolute path dep; `check.sh` fails elsewhere; deploy blocked on private-repo Docker credentials; panic-validate-result.md is untracked local lore.
**Action:** Swap to git dependency (ssh) with a `[patch]` section for local development before the live stage lands in `tools/hindsight`; the panic writeup's content moves into the committed plan/docs (already summarized in PLAN.md) and an upstream issue gets filed on apex-solver; deploy-credentials scoping added to the deploy-path open question for the user.

## Finding 19 — medium — Disputed
**Issue:** Phase 0 ceilings computed on the Allium universe; Phase 1 measures hindsight-decoded flow — incomparable denominators.
**Action:** The critic misread which dataset produced the final Phase 0 numbers. The committed paper trail states the input was hindsight's own decoder output, not Allium: `docs/analysis/2026-08-base-cow-phase0/README.md`: "Input data: `s3://propellerheads-hindsight/staging/base/comparisons/` (hindsight monitor JSONL, one file per UTC day…)" and "Headline results (10 days, 2026-07-25 → 08-03, 868,783 decoded trades)". The 0.02 bps / 1.7% / 23% ceilings are already on the hindsight universe (Allium was used only for the day-0 prototype, representativeness check, and sender lookups). Same universe, same denominators; comparability holds. (The critic's underlying instinct — report the hindsight-vs-Allium volume coverage ratio alongside — is cheap and gets one line in the report template anyway.)

## Finding 20 — medium — Resolved
**Issue:** No estimate of blocks/day with ≥2 connected trades; live single-block stage may be structurally signal-free; scheduled before the multi-block work where Phase 0 says value lives.
**Action:** Added as shadow-phase item 0.5: compute from the existing 10-day local data the count of blocks with ≥2 decoded trades sharing a token (a 10-minute script). Decision rule recorded: if <N blocks/day (threshold set with the user), the live single-block stage ships as instrumentation-only (solve-time + internalization) and the value claims come from offline window sweeps.

## Finding 21 — low — Resolved
**Issue:** `ApexSolution`/`pool_clearings` naming confuses turbine's type with apex-solver's `ApexResult`.
**Action:** Plan text corrected to `ApexResult { pool_clearings, limit_order_clearings, .. }` (apex-solver API); a general "verify against apex-solver, not turbine" note added to the implementation queue — this was the third turbine-inherited API error (after deadline_ms and MarketOrder).

## Finding 22 — low — Resolved
**Issue:** Live and offline paths duplicate "build APEX input from a block" and will drift.
**Action:** Single shared implementation pinned: `tools/apex-batch` stays a library; hindsight's live stage depends on it and calls the same `build_orders`/`initial_prices`/`apex_tokens`/scaling as the offline runner. One code path, two state sources.

# Verdict

The plan does not survive round 1 as written — all three criticals were real, and all three came from the same root cause: APEX API facts inherited from turbine's integration instead of verified against apex-solver itself (MarketOrder-vs-LimitOrder, ApexSolution naming, deadline_ms earlier). The hardened plan reorders the pipeline (adapter property tests → connectivity pre-check → shadow run → extractors → live stage), fixes the order representation, moves the solve before the advance on cloned state, pins the budget-parity policy, cuts the live multi-block mode, and re-specs the recording format. These are architecture-level changes: **a second grill round on the hardened plan is warranted before scaffolding/implementation** — a fresh critic should specifically re-verify the LimitOrder construction (limit-price direction and 18-dec scaling of the `Fraction`), the pre-advance clone cost claim, and the recording format v2 replay math.
