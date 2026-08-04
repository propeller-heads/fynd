# Grilling log (round 2)

## Statistics
- Findings raised: 21
- By severity: critical=4, high=9, medium=6, low=2
- Resolved: 21
- Deferred: 0
- Disputed: 0
- Subsumed: 0
- Critical/high deferral rate: 0%

All resolutions are consolidated into the **PHASE 1 v3** section of PLAN.md; item numbers below
refer to that section.

## Finding 1 — critical — Resolved
**Issue:** `starting_price` engages for every unpriced *pool* token when `enable_two_hops` is true
(`get_market_tokens()` returns all pool tokens; `unwrap_or(config.starting_price)` at
apex mod.rs:299-311); the plan's "never engages" claim covered orders only.
**Action:** v3 item 6: the adapter's pricing precondition extends to the full token set — every
token of every included pool must be priced from the N−1 map; a pool with any unpriced token is
dropped (counted `pool_unpriced`), never silently defaulted. The v2 item-13 claim is retracted.

## Finding 2 — critical — Resolved (metric framing flagged for user sign-off)
**Issue:** APEX clears ≤1 pool per pair with no splitting (`get_pools_supply` takes max over
pools; `clear_amount` picks a single pool) while fynd splits and multi-hops — `apex_vs_fynd_bps`
measures batching *minus* a routing-engine gap.
**Action:** v3 item 1: a per-order **APEX-singles control** is added — every order is also solved
through APEX as a single-order batch (same config, subset, budget). The batching-isolated
headline becomes apex(batch) vs apex(singles); apex_vs_fynd is reported alongside, labeled
engine-inclusive. Singles solves are near-free (tiny clusters). The revised headline framing
needs explicit user/Alan confirmation before the report ships.

## Finding 3 — critical — Resolved
**Issue:** `ApexConfig.deadline` is an absolute `Instant`; computed at enqueue, a queued solve
returns an empty `ApexResult` (`deadline_passed()` on the first cluster) — silent zero-fill
exactly on busy blocks; computed in the closure, the queue has no backpressure.
**Action:** v3 item 2: deadline is computed at solve start inside the worker; `queue_wait_ms` and
the solver's `deadline_fired` flag are recorded per solve; the dispatch queue is bounded, with
overflow dropped and counted `apex_skipped{reason="queue_full"}`. Staleness is bounded by queue
capacity, not wished away.

## Finding 4 — critical — Resolved
**Issue:** Price numeraire/scale unpinned; a long-tail token whose price rounds to zero causes
hard division-by-zero panics inside APEX (`clearing.rs:66`, `mod.rs:~466`) and poisons `Fraction`
ordering.
**Action:** v3 item 7: per-batch price normalization pinned — scale chosen so the minimum price
≥ 1e6 units with an overflow-headroom assertion; tokens whose price would still round to zero are
excluded with their orders/pools (counted `price_underflow`); a zero-price hard precondition runs
before every call. Property test with extreme-price (1e-12 ETH) tokens added to step 0.

## Finding 5 — high — Resolved
**Issue:** The 1 s deadline is not a hard cap — it is checked only between clusters, in the
price-search loop, and once after search; the whole clearing phase (demand oracle loop, simplex,
clear_amount, validation) runs unbounded.
**Action:** v3 item 3: plan text corrected (1 s = search budget, not wall cap); an outer watchdog
at the join discards results older than 3× budget (counted `apex_overrun`); the shadow run gates
on measured **total** wall time (p99 < 2 s) and shrinks the pool subset otherwise. The pacing
model uses measured totals, not the nominal budget.

## Finding 6 — high — Resolved
**Issue:** "Clone also yields the Arc" is wrong: `extract_subset`/`clone_box` yield
`Box<dyn ProtocolSim>`, and `Arc::from(Box)` for an unsized type allocates and memcpys a second
full copy — 2 copies per state per bracket, 4 per block.
**Action:** v3 item 14: claim corrected; the shadow run's clone line item measures both terms
(subset clone + Arc conversion) separately. No premature optimization — measured first.

## Finding 7 — high — Resolved
**Issue:** "Fynd's per-trade solves parallelize across trades" is unbuilt — `resolve_block_range`
awaits each solve sequentially (mod.rs:349-355, back loop :361-373); the mock tests cover the
sequential shape.
**Action:** v3 item 4: named build item — concurrent per-trade solves in `resolve_block_range`
(join_all/FuturesUnordered), mock tests updated. Until it lands the v2.2 lag ladder is explicitly
the primary pacing mechanism, not a backstop.

## Finding 8 — high — Resolved
**Issue:** No integration seam: `advance()` happens inside `resolve_block_range`, and the
`SteppingSolver` trait exposes no pool-state accessor — "the block loop clones a subset" names a
seam that doesn't exist.
**Action:** v3 item 15: named refactor — split `resolve_block_range` into tops/advance/backs
phases driven from the monitor's concrete-`Solver` path (which has `market_data()`), keeping the
`SteppingSolver` mock tests on the composed function. Budgeted as part of build step 3, not
assumed away.

## Finding 9 — high — Resolved
**Issue:** A single cluster `Err` aborts the entire batch (`solve_cluster(...)?` in
`run_apex_solver`), discarding already-merged clusters; error-prone blocks (many clusters) vanish
from the sample with no counter.
**Action:** v3 item 12: the adapter partitions orders+pools into connected components and calls
APEX once per component — an errored component is counted (`component_error{kind}`) and the rest
of the block survives. Per-order status gains `component_errored`. (This also spreads the budget
across components predictably.)

## Finding 10 — high — Resolved
**Issue:** Truncating fill computation vs exact limit validation: an order whose limit sits within
one wei of the clearing price can fail `validate_result` (no flooring slack on 18-dec-out tokens)
and killed the whole batch under the old design.
**Action:** v3 item 12 (tail): with per-component isolation the blast radius is one component,
counted; limits are **not** epsilon-fudged (they are the fill decision; distorting them corrupts
the study); the truncation-vs-validation rounding mismatch is filed upstream. At-limit property
test added to step 0.

## Finding 11 — high — Resolved
**Issue:** APEX pool identity is a 20-byte address; tycho `uniswap_v4` components have 32-byte
ids against a singleton PoolManager — truncation silently overwrites pools in `pools_map`.
**Action:** v3 item 11: for component ids that are not 20-byte addresses, apex address =
`keccak256(component_id)[0..20]`; the adapter keeps an address→component-id map and errors on
collision at build time (counted, actionable message).

## Finding 12 — high — Resolved
**Issue:** Checkpoint-plus-deltas replayed per block is O(n²) (~39 M update applications per
day-sweep) and conflicts with `run_matrix`'s block-parallel contract.
**Action:** v3 item 16: replay redesigned to **one forward fold per day per config**; APEX solves
fan out to rayon from cloned filtered subsets at eligible blocks; window-start states for
w∈{6,30,150} come from a ring buffer of retained subset clones (eligible blocks only, ≤150-block
depth). Checkpoints are demoted to crash-recovery; `run_matrix`'s per-block replay contract is
replaced, not kept alongside.

## Finding 13 — high — Resolved
**Issue:** `is_partial` gate has no predicate or action: partials share `block_number_or_timestamp`
with the confirmed update, and a partial-only state change would linger in a folded snapshot
forever.
**Action:** v3 item 17: policy pinned — capture drops `is_partial` updates at ingestion (counted);
fold key = confirmed updates only; the shadow run verifies whether the Base stream emits partials
at all and that fynd-core's live feed treats them equivalently (else that's a gate escalation, not
a silent divergence).

## Finding 14 — medium — Resolved
**Issue:** Format v2 (segments + checkpoints) is a build item comparable to the adapter —
writer and reader both rewritten, checkpoint construction needs a folded state and hits the v4
serialization hole; `gas_price_wei` is a single start-of-run value.
**Action:** v3 items 16+18: scope cut — segments-only format ships first (append-only hourly
zstd segments, incremental flush), checkpoints deferred to crash-recovery follow-up since the
fold-once replay design (F12) no longer needs random access. Per-block gas price recorded in
segments; the frozen-metadata value is retired.

## Finding 15 — medium — Resolved
**Issue:** Panic rationale anchored to a stale doc (the mod.rs:469 unwrap no longer exists); the
live panic vectors are `tokens[&addr]` index panics (pool tokens missing from the token vec) and
zero limit denominators — preventable preconditions, not `catch_unwind` material.
**Action:** v3 items 6+10: the token-closure precondition explicitly includes every pool token;
`sell_amount > 0`, nonzero prices, and nonzero limit denominators are asserted pre-call with
actionable messages. `catch_unwind` remains only as a last-resort backstop with `SolveMetrics`
discard. The stale `panic-validate-result.md` is superseded; a fresh upstream issue lists the
current index-panic sites.

## Finding 16 — medium — Resolved
**Issue:** Limit direction correct but the scaling contract unstated —
`Fraction::new(raw_usdc_min_out, raw_weth_sell)` type-checks and is wrong by 10^12.
**Action:** v3 item 8: pinned —
`limit_price = Fraction::new(lift18(min_amount_out, buy_decimals), lift18(sell_amount, sell_decimals))`,
both legs in APEX's 18-dec space. Step-0 property tests get explicit mixed-decimal cases
(USDC/WETH both directions) asserting exact rational equality.

## Finding 17 — medium — Resolved
**Issue:** `LimitOrder` carries no tokens; equal `(limit_price, id)` orders silently collapse in
the `BTreeSet`; `OrderbookManager::new` indexes `tokens[&pair.0]` (panic).
**Action:** v3 item 9: build-time uniqueness assertion (orders in == set sizes out; violations
counted and declined); the per-order reconciler reads the **pre-map trade list**, pinned in the
spec. The pair-token index panic is covered by the item-6 closure precondition.

## Finding 18 — medium — Resolved
**Issue:** A "dedicated APEX thread cap" is not expressible via `spawn_blocking` (runtime-wide
pool, shared with JSONL IO; OS threads still contend for cores); `multithread` feature +
`max_workers > 1` builds a fresh rayon pool per cluster outside any cap.
**Action:** v3 item 4: replaced with an APEX-stage-owned fixed worker pool (2 OS threads) fed by
a bounded channel; apex-solver built without the `multithread` feature for the live path
(`max_workers = 1`); the fynd-solve-time drift metric stays as the detector.

## Finding 19 — medium — Resolved
**Issue:** `enable_two_hops` is a first-order cost/correctness determinant, not a sweep axis, and
the pool-subset filter criterion — the only lever making 1 s plausible — is unspecified.
**Action:** v3 item 13: headline pinned to `two_hops = ON` with the full-pool-pricing
precondition (item 6); subset filter pinned: pools adjacent to order tokens ∪ pools linking two
order-adjacent tokens (2-hop closure), TVL-capped at K, K tuned by the shadow run. `two_hops=OFF`
becomes an offline secondary cell only.

## Finding 20 — low — Resolved
**Issue:** live-w1 vs offline-w1 measures the v4 handicap *plus* every other live/replay
divergence (frozen gas, partials, subset diffs, price provenance) — nothing isolates the v4 term.
**Action:** v3 item 20: relabeled as the combined live/replay divergence upper bound; an optional
isolation cell (live run with v4 components dropped) is noted for the upstream-ask evidence if
the combined bound turns out large.

## Finding 21 — low — Resolved
**Issue:** Headline is gated on the extracted-limit subset with no advance threshold on that
subset's size — a 30% coverage discovery at step 6 changes the deliverable.
**Action:** v3 item 19: coverage gate added at build step 2 — measure 0x-floor extraction
coverage on a sample day; below 50% of eligible orders, escalate to the user before the live
stage (add kyber/paraswap extractors or reframe as synthetic-primary).

# Verdict

Round 2 was productive in exactly the way round 1's verdict predicted: all four focus areas
produced real corrections, and the two most dangerous findings (F1 starting_price engagement, F2
routing-gap conflation) were outside the focus list. Every finding is resolved in the v3 section —
no deferrals — but the resolutions are substantial: a new control cell (APEX-singles), a redesigned
replay (fold-once), a redesigned pacing spec (owned worker pool, solve-start deadlines), and a
much stricter adapter input contract. Because v3 changes the architecture again, a **focused round
3 on the v3 section alone** is warranted before the live-stage build; the 0x Settler extractor
(pure decoder work, independent of every APEX finding) can proceed in parallel. The F2 headline
reframing (batch-vs-singles as the batching-isolated number) needs explicit user sign-off.
