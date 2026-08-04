# Grilling log (round 3)

## Statistics
- Findings raised: 12
- By severity: critical=2, high=5, medium=3, low=2
- Resolved: 12
- Deferred: 0
- Disputed: 0
- Subsumed: 0
- Critical/high deferral rate: 0%

Resolutions are consolidated into the **PHASE 1 v3.1** section of PLAN.md. Finding 3 was
resolved by measurement, not argument: `component_count.py` ran on the 10-day dataset before
these resolutions were written.

## Finding 1 — critical — Resolved
**Issue:** apex arithmetic wraps silently (`Signed256::mul` → `wrapping_mul`; ruint ops likewise);
the objective squares value = amount₁₈ × price, so |value| < 2^127.5 must hold; `increase_precision`
multiplies every price ×10 up to `max_precision_increases` (default 10 = ×1e10) — the "min price
≥ 1e6" floor and the overflow headroom are mutually unsatisfiable on realistic batches, and
violation produces garbage numbers with no counter.
**Action:** v3.1 item A: the 1e6 floor is RETRACTED. The scale S is derived from the overflow
bound: with `max_precision_increases` pinned to P=2 and a per-batch notional cap N_usd, assert
`N_usd · 1e18 · S · 10^P < 2^126`; S maximized under that bound; tokens whose scaled price
rounds below 1e3 units are excluded (`price_underflow`, counted per batch). Upstream issue
filed: wrapping arithmetic makes overflow silent — request checked ops or a documented input
bound. Property test with a $1e-9-priced token exercising the bound arithmetic.

## Finding 2 — critical — Resolved
**Issue:** the fynd→apex price transform omits the inversion (tycho `Price` = tokens received
per gas token, apex needs value-per-token orientation: more valuable token ⇒ larger price) and
the decimals factor (tycho rational is atomic-units, apex amounts are 18-dec lifted).
**Action:** v3.1 item B: transform pinned explicitly —
`apex_price(t) = round(S · 10^(dec_t − 18) · denominator_t / numerator_t)`
(inversion + decimal lift + scale). Anchored by a step-0 property test mirroring fynd-core's
own fixture (ETH at 2000 USDC: assert the apex USDC/WETH price ratio lands at 2000 × 1e12
after the decimals factor, exact).

## Finding 3 — high — Resolved (by measurement)
**Issue:** per-component isolation predicted to be a no-op — hub pools (WETH/USDC…) would
connect essentially every order, collapsing every eligible block to one component.
**Action:** Measured (`component_count.py`, 10 days, hub-linking approximation of the pinned
subset filter): **41.2% of eligible blocks are single-component; 58.8% have ≥2 components**
(2: 45.7%, 3: 11.4%, ≥4: 1.7%); 77.9% of orders sit in the hub-connected giant component. So
the isolation is real but partial: it contains aborts on ~59% of blocks and never protects the
giant component. v3.1 item C keeps per-component calls, adds an explicit `batch_errored`
per-block counter for giant-component aborts, reports the resulting sample-loss bias in the
report template, and keeps the upstream ask (per-cluster error isolation in `run_apex_solver`)
as the real fix.

## Finding 4 — high — Resolved
**Issue:** where components ≥2, per-component calls with component-scoped pools solve a
different-dimension problem than one call (all_tokens = whole-market union; supply and
upper-bound passes iterate all pairs; unpriced-intent tokens inflate the simplex) — not a pure
blast-radius change.
**Action:** v3.1 item C pins: each component call receives ONLY its component's pools and
tokens; the SAME partitioning is applied to the batch cell and the singles cell so the two
cells stay comparable (the headline is a within-partitioning comparison); per-component results
are explicitly not bit-for-bit equal to a hypothetical single call, stated in the report
template.

## Finding 5 — high — Resolved
**Issue:** pacing model counted one solve per bracket; singles add ×(1+n) at same-subset cost,
components add ×N with "1 s per batch" ambiguous; 50% vs 32% eligibility inconsistency; singles
would make 1-trade blocks eligible.
**Action:** v3.1 item D re-costs: singles use their own component's subset (small — a single
order's 2-hop closure), capped 250 ms each; budget = 1 s per COMPONENT batch solve; eligibility
corrected to CONNECTED blocks (~32%/day measured, not 50%); singles run only for orders in
batch-eligible blocks (the comparison needs both cells); all solves consume inputs cloned at
block time, so queue delay affects metric latency only, never state parity — the gate becomes
sustained throughput ≥ arrival rate with bounded queue depth and counted skips, not a per-block
wall-clock p99. Worker pool sized by the shadow run against this full load model.

## Finding 6 — high — Resolved
**Issue:** the ring buffer retains the wrong object (per-block subsets, not window-union
subsets) on the wrong index (eligible blocks, not chain blocks) — window-start cells for
w∈{6,30,150} cannot be built from it.
**Action:** v3.1 item E: ring buffer DELETED. Offline replay knows every order in advance
(capture indexes trades per block before the fold). The single forward fold, at each chain
block b, builds and dispatches every cell anchored at b: window-start cells for windows
[b+1, b+w] use the KNOWN union of that window's orders to filter the subset from the current
folded state; window-end cells for windows ending at b likewise. Memory = one folded market
state + in-flight cell subsets (bounded by the rayon queue), no retention across blocks.

## Finding 7 — high — Resolved
**Issue:** the 3×-budget watchdog bounds result freshness, not thread occupancy (no
cancellation path into `run_apex_solver`); the unbounded clearing phase scales with pool count
(the K knob), not the search budget.
**Action:** v3.1 item F states the honest contract: an overrunning solve occupies its worker to
completion — visible as queue depth + `apex_overrun`; the shadow run records `search_ms` and
`clearing_ms` as SEPARATE series and the K gate is set against the clearing-phase tail
specifically; upstream ask extended to include deadline checks in the clearing phase.

## Finding 8 — medium — Resolved
**Issue:** no pairing rule for the batch-vs-singles headline; asymmetric degradation
(deadline_fired batches vs surviving singles) selection-biases the number in batching's favor.
**Action:** v3.1 item G: headline computed over the INTERSECTION — orders whose batch cell and
singles cell both produced validated results (not deadline_fired, not errored, not skipped);
every exclusion counted by reason; a sensitivity view over all-orders-with-batch-result
reported alongside. Ships as part of the user/Alan sign-off package.

## Finding 9 — medium — Resolved
**Issue:** keeping `SteppingSolver` mock tests on a composed function nothing in production
calls is a deprecated shim kept alive for its tests.
**Action:** v3.1 item H: the composed function is dropped. The monitor calls the three phase
functions directly; the mock tests move onto the phase functions plus one test asserting the
monitor's sequencing (tops → clone → advance → backs). Replace, don't deprecate.

## Finding 10 — medium — Resolved
**Issue:** pool-drop-on-unpriced-token has a counter but no gate; the derived price map
(max_hops=2 from gas token, viability-filtered) will miss long-tail tokens and can hollow out
the 2-hop closure silently.
**Action:** v3.1 item I: price-coverage gate added next to the extractor gate — shadow run
measures the `pool_unpriced` share of the subset on a sample block set; >20% dropped pools →
escalate before the live stage (options: widen derived-price hops upstream, restrict the study
universe, or accept with the number stamped on every result).

## Finding 11 — low — Resolved
**Issue:** `runner.rs:170-186` still mandates catch_unwind citing the stale
`panic-validate-result.md`; the real panic sites are the `tokens[&addr]` indexes and
`OrderbookManager::get_order`'s expect.
**Action:** Implementation task recorded in v3.1 item J: rewrite the runner doc comment against
the real panic sites and the precondition-first policy in the same change that implements
`solve_block`; retire the stale doc reference.

## Finding 12 — low — Resolved
**Issue:** "EXACT rationals" overstates what the U256 boundary preserves; exactness only avoids
f64 double-rounding and does not justify any particular floor.
**Action:** v3.1 item A/B wording: the scale is argued from the overflow bound alone; the
rational source is kept because it is the cheapest correct input, not as a precision claim.

# Verdict

Round 3 killed the two silent-garbage paths (wrapping overflow at the pinned scale; the
uninverted, decimals-less price transform) and replaced two designs that would not have produced
the required cells (ring buffer; freshness-only watchdog framing). The component question was
settled with data instead of debate: 59% of eligible blocks genuinely split, so per-component
isolation stays as a partial mitigation with an honest counter for the rest. No findings were
deferred. The remaining uncertainty is now EMPIRICAL, not architectural — clearing-phase tails,
price coverage, extractor coverage, queue sizing — and the build order's own gates (step-0
property tests encode findings 1/2/7's checks; the shadow run measures findings 5/7/10's
numbers before anything live ships) are the correct instruments for it. **No round 4: proceed
to build steps 0–1**, with the two user sign-offs outstanding (headline reframing + pairing
rule, and the escalation thresholds).
