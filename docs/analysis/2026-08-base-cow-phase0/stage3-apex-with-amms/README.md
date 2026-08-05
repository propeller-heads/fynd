# Stage 3 — APEX batch clearing with AMM pools, offline sweep

> **Two sweeps live here.** Sweep 1 (`stage3_results-49562643.json`, table below) ran from-disk
> against the legacy 122-pool snapshot. Sweep 2 (`stage3_results-49575430.json`) ran LIVE at
> block 49,575,430 after the TVL-unit and v4 fixes: 400-pool subset (of 2,702 candidates ≥1 ETH
> TVL), full+serializable scope cells, and full-coverage fynd quotes (14,490). See "Sweep 2"
> below — it revises one finding and hardens the rest.

One partial day of decoded Base solver trades (2026-08-03, 00:00–13:15 UTC, 14,490 intents,
$9.17M) replayed against a persisted AMM snapshot (block 49,562,643; 122 serializable native
pools of a 230-pool subset — 108 uniswap_v4 dropped), batched into non-overlapping windows and
cleared by APEX with a 1 s per-component search deadline. Grid: windows {1,5,15,30,150} ×
limit 100 bps × anchors {original, current} × {pools, no_pools}, serializable pool scope.

**Read [CAVEATS.md](CAVEATS.md) before quoting any number.** The two capture-scope defects it
lists (TVL filter unit — the pool universe here is ~$350k+ TVL pools only; v4 absence) are fixed
in the capture code for the next snapshot but apply fully to THIS table. Results JSON:
`stage3_results.json` (per-cell counters included). Run: `stage3 --snapshot
base-native-49562643.json.zst --days 2026-08-03 --limit-bps 100 --solve-deadline-ms 1000`.

## Anchors

- **original** — floors and surplus baseline from the trade-time settled amount. Orders are
  42–55 h older than the pool state, so raw surplus includes price drift; `exdrift` strips
  orders whose pair price moved >100 bps between trade day and snapshot.
- **current** — floors and baseline from the snapshot-state pool-implied quote (drift-free by
  construction; excludes orders without such a quote — a majors-heavy 8.2k-order universe).

## Table (matched / surplus in USD; surplus columns: raw, ex-drift, net-of-negative-gaps)

| w | anchor | cell | matched | % day | surplus | exdrift | net | intern | realized | fallback | fynd n / med bps |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | orig | no_pools | 10,527 | 0.12 | 44 | 30 | 8 | — | 0.003 | — | 306 / +45.6 |
| 1 | orig | pools | 14,165 | 0.15 | 4,404 | 32 | 4,368 | 0.53 | 0.006 | 1,567 | 312 / +39.7 |
| 1 | curr | no_pools | 10,542 | 0.12 | 36 | 24 | 1 | — | 0.003 | — | 19 / −73.1 |
| 1 | curr | pools | 11,590 | 0.13 | 36 | 24 | 1 | 0.91 | 0.003 | 1,373 | 20 / −60.5 |
| 5 | orig | no_pools | 78,684 | 0.86 | 308 | 210 | 50 | — | 0.011 | — | 1,120 / +40.6 |
| 5 | orig | pools | 79,774 | 0.87 | 353 | 210 | 95 | 0.99 | 0.012 | 2,377 | 1,126 / +39.7 |
| 5 | curr | no_pools | 78,793 | 0.86 | 275 | 179 | 17 | — | 0.012 | — | 58 / −32.3 |
| 5 | curr | pools | 78,827 | 0.86 | 275 | 179 | 17 | 1.00 | 0.012 | 2,225 | 58 / −32.3 |
| 15 | orig | no_pools | 249,609 | 2.72 | 909 | 660 | 158 | — | 0.033 | — | 2,219 / +39.2 |
| 15 | orig | pools | 249,855 | 2.73 | 916 | 660 | 165 | 1.00 | 0.033 | 1,386 | 2,222 / +39.2 |
| 15 | curr | no_pools | 249,947 | 2.73 | 844 | 605 | 61 | — | 0.033 | — | 130 / +38.9 |
| 15 | curr | pools | 249,947 | 2.73 | 844 | 605 | 61 | 1.00 | 0.033 | 1,364 | 130 / +38.9 |
| 30 | orig | no_pools | 576,994 | 6.29 | 1,900 | 1,435 | 284 | — | 0.075 | — | 2,905 / +36.6 |
| 30 | orig | pools | 577,225 | 6.30 | 1,900 | 1,435 | 284 | 1.00 | 0.075 | 744 | 2,908 / +36.3 |
| 30 | curr | no_pools | 575,998 | 6.28 | 1,813 | 1,357 | 131 | — | 0.076 | — | 167 / +54.1 |
| 30 | curr | pools | 576,201 | 6.29 | 1,813 | 1,357 | 131 | 1.00 | 0.076 | 742 | 167 / +54.1 |
| 150 | orig | no_pools | 1,413,654 | 15.42 | 4,928 | 2,870 | 899 | — | 0.184 | — | 4,191 / +40.6 |
| 150 | orig | pools | 1,416,160 | 15.45 | 10,580 | 2,870 | 6,551 | 1.00 | 0.185 | 154 | 4,192 / +40.6 |
| 150 | curr | no_pools | 1,396,668 | 15.24 | 4,515 | 2,732 | 456 | — | 0.184 | — | 224 / +65.6 |
| 150 | curr | pools | 1,400,748 | 15.28 | 4,568 | 2,732 | 477 | 1.00 | 0.185 | 154 | 224 / +65.6 |

Columns: `intern` = internalization share on filled notional (order-vs-order crossing fraction);
`realized` = filled/submitted notional; `fallback` = pooled components re-solved without pools
after erroring or deadline-firing empty; fynd = per-order comparison vs Fynd's own routing
(trade-time quotes at the original anchor — drift-contaminated; snapshot-state quotes at
current — clean but small n).

## Findings

1. **The 1 s budget, not liquidity, decides everything pools-related.** Fallback counts vs
   components solved: at w=150, 154 of 155 pooled components fell back — exactly one pooled
   search in the whole cell finished inside 1 s; `pools_fallback_matched_usd` accounts for
   ~99.7% of the pools cell's matched USD. At w=1 (smallest components) 68 of 1,635 pooled
   solves completed, and those few show real pool routing: internalization 0.53 (original) /
   0.91 (current) and +10–35% matched over no_pools. Between (w=5–30), pooled solves
   effectively never complete and pools ≈ no_pools identically. The two-hop search over the
   ~120-token closure needs either a much larger budget, a smaller search space, or upstream
   partial-credit at the deadline before the pools axis says anything about economics.
2. **Window dominates matched volume**: 0.12% → 15.4% of day USD from w=1 to w=150 (~130×),
   consistent with Phase 0's window-beats-cleverness result. Fill rate by notional
   (`realized`) rises 0.3% → 18.5%.
3. **Honest surplus is thin and distributed**: ex-drift surplus is ~0.3–0.4% of matched USD at
   every window ($24 at w=1 up to $2.9k/half-day at w=150), with top-5 concentration falling
   from ~47% to 8% as windows grow. The drift columns work as designed: the w=1 and w=150
   original/pools raw-surplus spikes ($4.4k, $10.6k) are entirely drift-flagged outliers
   (top5 = 99% / 56%), and the current anchor reproduces the ex-drift numbers without them.
4. **Original − Current ex-drift ≈ drift cost bound**: small at every window (≤ $140/half-day),
   because the current anchor's tighter universe offsets the stale baselines.
5. **Fynd head-to-head (current anchor, small n)**: APEX crossing loses to Fynd routing at
   small windows (−60 bps at w=1 n=20, −32 at w=5 n=58) and wins at larger ones (+39/+54/+66
   bps at w=15/30/150, n=130/167/224). More orders per batch → better internal price
   discovery. Same caveat as everything above: gross of gas, majors-only universe, one
   partial day.

## Sweep 2 — widened universe, live state, honest fynd baseline (49575430)

Same day and grid plus a full-pool-scope axis (30 cells), captured live: 400-pool subset
(min_tvl = 1 ETH; 220 serializable + 34 hookless-v4 raw + 146 lunarbase live-only), snapshot
state block 49,575,430, full-coverage snapshot-state fynd quotes. Full table in
`stage3_results-49575430.json`. What changed vs sweep 1:

1. **The fynd head-to-head FLIPS on the clean anchor.** With a full-coverage, richer-universe
   fynd baseline, APEX crossing loses to fynd routing at EVERY window: median −119 bps (w=1,
   n=305) → −118 (w=5) → −110 (w=15) → −101 (w=30) → −60 (w=150, n=3,480). Sweep 1's "+39..+66
   bps APEX win at w≥15" was an artifact of the 6.7%-sampled, 122-pool fynd baseline. The
   deficit shrinking with window size is real (more counterparties → better clearing prices),
   but crossing at synthetic 100 bps floors clears roughly one limit-width worse than routing.
   Original-anchor comparisons still show +30..+51 bps — that column is drift-inflated
   (trade-time baseline vs snapshot clearing) and must not be quoted as a win.
2. **The 1 s budget still nullifies the pools axis.** 98–99% of pooled component solves
   deadline-fire with ZERO clearings and fall back to crossing-only (sweep-1 exact count:
   12,086 empty of 12,242; sweep 2 is the same shape with an even bigger search space).
   Full-vs-serializable scope differs by ≤0.5% matched — the 180 live-only pools are
   unreachable within budget. Where pooled solves DO complete (w=1: intern 0.80, w=5: 0.91),
   pools add +9–14% matched on the clean anchor — the only cells measuring pool economics.
3. **Drift columns work under pressure**: the widened universe lets stale-priced orders fill
   through more pools at the Original anchor (raw surplus up to $19k/cell), and ex-drift
   collapses every such cell to $30–$663 with top5 ≥ 74%. Clean-anchor ex-drift surplus is
   stable across both sweeps (~$23 at w=1 → ~$3.5k at w=150 per half-day).
4. **Matched volume is unchanged** (0.13% → 15.2% of day USD by window) — batching's addressable
   volume is set by order-flow connectivity, not by the pool universe.

## Provenance

- Snapshot meta in `stage3_results.json` (`.snapshot`): 122 pools, 146 priced tokens, 2,436
  sampled trade-time fynd quotes (6.7%), 8,193 snapshot-state pool-implied quotes.
- Code at `522394c4` (metric fixes `b6d63095`, parallel cells `51dd31dc`, v4-raw persistence +
  TVL unit fix `522394c4`). Forensic reviews and methodology audit: 2026-08-05, this branch's
  session logs.
- Next: fresh capture with `--min-tvl 1` and hookless-v4 raw persistence (needs
  `TYCHO_API_KEY`), then sweep #2 on the widened universe; deadline-sensitivity cells
  (`--solve-deadline-ms 3000/10000`) to separate budget effects from economics.
