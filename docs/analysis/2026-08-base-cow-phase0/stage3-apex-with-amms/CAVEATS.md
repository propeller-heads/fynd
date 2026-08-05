# Stage 3 — reading the table: pitfalls & caveats

Independent methodology audit, 2026-08-05 (read-only review of `tools/apex-batch/src/bin/stage3.rs`
at commit `51dd31dc` plus the two fix commits `b6d63095`/`51dd31dc`; dataset funnel replicated in
Python to the exact 14,490-intent count). Ordered by how badly a naive reader would be misled.

1. **The "AMM" universe is only whale pools — the TVL filter is 100 ETH, not $100.** `--min-tvl
   100` flows into tycho's `ComponentFilter`, whose thresholds are denominated in the chain's
   native token; 100 therefore means ~$350k+ TVL. Every pools-cell number (matched uplift,
   internalization, Current-anchor quote coverage) is measured against only the ~230 deepest
   native pools on Base (122 in from-disk runs). Pool uplift and internalization are best read as
   lower bounds for a properly $-scoped universe. (Fixed for future captures: the flag now
   defaults to 1.0 with the unit documented.)
2. **Original-anchor surplus is mostly price drift, not batching edge.** Orders are 42–55 h older
   than the pool state. Read `apex_surplus_ex_drift_usd` or the Current anchor instead, and
   Original − Current per window as the drift cost. Even ex-drift is not fully clean: the flag
   compares day-median trade-day prices to snapshot prices with a 100 bps threshold, so uniform
   sub-100 bps drift across the book and intraday moves pass through unflagged.
3. **Pools ≥ no_pools is partly by construction.** Pooled components whose solve errors or
   deadline-fires empty are re-solved without pools inside the pools cell. Subtract
   `pools_fallback_matched_usd` before attributing the pools-vs-no_pools gap to AMM liquidity,
   and distrust `internalization_share` in cells with large fallback counts (fallback fills count
   as 100% internalized). A pooled solve that deadline-fires with even one clearing gets no
   fallback, so pools cells can also still lose matches to budget exhaustion.
4. **Solve times and deadline effects are contaminated by cell parallelism.** 20 cells ran
   concurrently on 10 cores with wall-clock 1 s deadlines; deadline-bound solves did less search
   than a dedicated core would allow, and results in such cells vary run to run. Read p50 against
   the 1 s budget; treat p90/max and any deadline-heavy cell's matched/surplus as
   scheduling-dependent. The sweep also runs at the 1 s live budget, not the 3 s offline budget
   the RESUME doc planned — cells are budget-realistic, not quality-ceiling.
5. **matched_pct can never reach 100 and double-counts crosses.** Each filled order counts at its
   own USD (a $100-vs-$100 cross = $200 matched, mirroring stage 2), while the denominator is all
   14,490 intents ($9.17M) including ~17% of USD whose tokens the snapshot cannot price at all —
   the admissible ceiling is ~83%, and lower in Current cells (only 9,043 orders have both tokens
   priced; ~8.2k have pool-implied quotes).
6. **The Current anchor is a majors-only, beatable-by-routing baseline.** Orders without a
   snapshot pool-implied quote (long-tail tokens) are excluded — the Current universe skews to
   WETH/USDC/cbBTC. The baseline quote is one route, ≤2 hops, no splits, no gas, and prices every
   order at untouched pools — so Current-anchor surplus mixes batching edge with plain routing
   sophistication, and at large windows the untouched-pool assumption flatters the baseline.
7. **`apex_surplus_usd` is a positive-only sum that grows mechanically with limit slack.** The
   uniform clearing price also pushes counterparties below their settled outcome; those negative
   gaps are in `negative_gap_usd` and netted in `apex_surplus_net_usd` — quote the net column.
   Check `surplus_top1_usd`/`surplus_top5_share` first: a cell whose top-5 share is near 1 is a
   handful of orders, not a distributed edge.
8. **`internalization_share` measures net pool exposure, not "orders that avoided pools".** Two
   opposite orders each routed through pools in one solve net to zero exposure and count as fully
   internalized. It is defined on FILLED notional only; cross-check `realized_share` — when
   1 − realized_share ≈ internalization, the number carried no crossing signal. (Known minor
   defect: realized_share's denominator includes a sliver of notional from components skipped
   after admission, so the self-check is approximate.)
9. **Fynd-comparison columns have wildly different n and mixed state bases.** Original-anchor
   cells compare APEX-at-snapshot vs fynd-at-trade-time (cross-state, drift-contaminated both
   directions); Current-anchor cells use snapshot-state fynd quotes that exist for only a 6.7%
   sample of orders (2,436), leaving n as small as ~19 after intersecting with fills — medians
   there are noise. Also the snapshot fynd solver is native-protocols-only while the trade-time
   quotes came from the full production config; the two baselines are different engines.
10. **This is survivor flow on a half day.** The dataset is aggregator trades that actually
    settled on 2026-08-03 00:00–13:15 UTC (US session absent, three ~16-min capture gaps, $9.17M
    total; the local mirror's last day is a partial S3 sync). It answers "what would batching
    have done to this flow", not "what flow would batching attract" — flow that opts into a
    w=150 (5-minute) batch wait is a different population from flow that chose instant aggregator
    execution.
11. **Floors are synthetic.** Limit = settled (or current quote) × (1 − 100 bps) assumes every
    user tolerates up to 1% worse than their actual outcome; real limit prices were never
    extracted in this stage. Stage 2's 50/100/200 bps sweep moved matched% only slightly, which
    bounds — but does not eliminate — this sensitivity.
12. **Window buckets are one fixed tiling.** Batches are non-overlapping `block/w` buckets
    aligned to absolute block numbers; a crossing pair 1 block apart can straddle a boundary.
    Small-window matched% is biased down by boundary splitting, and a different bucket offset
    would give different numbers; no offset-sensitivity run exists.
13. **Uniswap v4 — half the eligible pool subset — is missing from this run.** 108 of 230 subset
    pools are v4 (non-serializable hooks) and absent from every from-disk sweep against the
    49562643 snapshot; the 122 survivors are v2/v3-era natives plus aerodrome. vm:* and RFQ
    liquidity is excluded by design everywhere. (Fixed for future captures: hookless v4 raw
    states are now persisted and re-decoded on load; hooked v4 stays live-only.)
14. **All numbers are gross of gas and settlement costs.** Order-level and batch-level gas are
    out of scope (stated study convention); surplus is not deliverable PnL, and the gas savings
    batching would actually generate are equally unmeasured.
15. **One day, one snapshot, no error bars.** Nothing in this table separates signal from
    day-to-day variance; deadline-bound cells are additionally nondeterministic across runs.
    Treat differences between adjacent cells as within-noise unless they are large, monotone
    across windows, and survive the ex-drift and net columns. Do not compare stage-3 matched%
    against the stage-2 table directly — stage 2 aggregates 10 full days, this is one partial
    day.

## Audit verdicts (per axis)

- Dataset validity: CAVEAT — 51.8% funnel survival (routable + priced + quarantine), $9.17M/day,
  13.25 h coverage, survivor flow.
- State consistency: CAVEAT BY DESIGN — correctly implemented post-fix; neither anchor measures
  "pools at trade time" (that is stage 4).
- Solver budget: CAVEAT + gap — no fallback for deadline-fired solves with partial clearings;
  fallback solves double a component's wall budget; fallback deadline fires uncounted.
- Metric definitions: SOUND with caveats (drift flag threshold, fynd-n variance).
- Window semantics: SOUND with boundary-tiling caveat.
- Pool scope: DEFECT (TVL unit, fixed forward) + v4 absence (fixed forward, hookless only).
- Statistics: CAVEAT — single partial day, no error bars.
- The three forensic fixes (internalization denominator, anchor-consistent baseline, no-pools
  fallback) verified correctly implemented; fallback counters do not double-count.
