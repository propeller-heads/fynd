# APEX live batching study — results

Numbers from the Hetzner live monitor (`mp/feat/apex-live-monitor`), joined offline with
`live_join.py`. See [`LIVE-MONITOR-FINDINGS.md`](LIVE-MONITOR-FINDINGS.md) for how the monitor
got here (deadline-pressure investigation, taxonomy fixes, the two correctness bugs found along
the way) — read that first if a number here looks surprising, since several of the fixes
described there directly changed what these numbers say.

## Data

Collection window for these numbers: **2026-08-07 20:36:27 UTC → last full `live_join.py` pull**,
~2.5 days, **213,052 comparisons** joined. The monitor has kept running since (confirmed active as
of the most recent status check, one watchdog-caught stall on 2026-08-09, otherwise uninterrupted)
— **re-run `live_join.py` for a current tally** before publishing anything from this file; treat
the numbers below as a snapshot, not a final count.

```bash
python3 docs/analysis/2026-08-base-cow-phase0/live_join.py <data-dir> --window 1   # or 5, 30
```

**Methodology note — read before using these numbers**: fills below $1 settled USD notional are
excluded from all median/mean/win-rate stats (not from USD sums) — see `LIVE-MONITOR-FINDINGS.md`
for why $1 and not the $100 first tried. `mean_bps` is reported but the median is the number to
trust; a handful of outliers can still move a mean even after the dust floor. `batch_vs_singles`'s
mean is separately unstable for an unrelated reason (see Known open items in the findings doc) —
use its median/win-rate only.

## Headline: APEX vs Fynd, same block state, top-of-block

| window | n | median bps | mean bps | APEX win rate |
|---|---|---|---|---|
| 1 block | 7,046 | **-27.09** | -42.74 | **12.8%** |
| 5 blocks | 12,178 | -24.84 | -43.25 | 21.2% |
| **30 blocks** | 20,378 | **+1.01** | +1.80 | **53.0%** |

At single-block granularity APEX clearly loses to Fynd's per-order routing — median 27bps worse,
wins about 1 time in 8. The picture flips only once there's enough order flow in one batch to
cross: win rate climbs 13% → 21% → 53% from window 1 to 30, and the median result goes from a
clear loss to roughly break-even.

## Full breakdown, both brackets and baselines

"top" = state N-1 (before this block's own trades executed, the fair/headline comparison);
"bottom" = state N (after, the biased-by-its-own-impact comparison). "vs Fynd" compares against
Fynd's own quote at the same state; "vs executed" compares against what the trade actually
settled for on-chain (whichever solver won it).

| window | bracket | baseline | n | median bps | win rate |
|---|---|---|---|---|---|
| 1 | top | fynd | 7,046 | -27.09 | 12.8% |
| 1 | top | executed | 7,048 | -22.41 | 26.4% |
| 1 | bottom | fynd | 5,931 | -14.08 | 17.3% |
| 1 | bottom | executed | 5,933 | -17.63 | 28.0% |
| 5 | top | fynd | 12,178 | -24.84 | 21.2% |
| 5 | top | executed | 12,179 | -22.73 | 27.6% |
| 5 | bottom | fynd | 11,651 | -12.41 | 27.0% |
| 5 | bottom | executed | 11,652 | -22.72 | 27.3% |
| 30 | top | fynd | 20,378 | 1.01 | 53.0% |
| 30 | top | executed | 20,383 | 4.51 | 57.8% |
| 30 | bottom | fynd | 20,421 | 2.13 | 56.0% |
| 30 | bottom | executed | 20,426 | 4.40 | 57.9% |

## Mean bps *if won* (conditional on APEX beating the baseline)

| window | bracket/baseline | win median bps | win mean bps | win rate |
|---|---|---|---|---|
| 1 | top vs fynd | 15.02 | 44.01 | 12.9% |
| 1 | top vs executed | 19.13 | 37.15 | 26.6% |
| 5 | top vs fynd | 24.81 | 56.46 | 21.3% |
| 5 | top vs executed | 25.71 | 52.21 | 27.7% |
| 30 | top vs fynd | 28.09 | 37.20 | 53.0% |
| 30 | top vs executed | 37.49 | 46.55 | 57.8% |

When APEX wins, it wins by a real margin (37–56bps mean) at every window. The problem at small
windows is win *frequency*, not win *size*.

## "Use APEX only when it beats Fynd" — best-of-policy uplift

Per order: `uplift = max(apex_bps - fynd_bps, 0)` — route through APEX when it wins, fall back to
Fynd otherwise, so the policy never does worse than Fynd alone. Averaged over every real
(≥$1) fill; losses count as zero, not negative. $/day computed over the exact elapsed window at
measurement time (2.4696 days).

| window | bracket | uplift mean bps | uplift median bps | uplift $ (2.47d) | **$/day** |
|---|---|---|---|---|---|
| 1 | top | 5.67 | 0.00 | $1,849.62 | **$749** |
| 1 | bottom | 5.26 | 0.00 | $1,366.12 | $554 |
| 5 | top | 12.04 | 0.00 | $2,980.85 | $1,207 |
| 5 | bottom | 13.20 | 0.00 | $3,264.14 | $1,322 |
| **30** | **top** | **19.71** | **1.01** | $4,785.62 | **$1,937** |
| 30 | bottom | 21.44 | 2.15 | $5,410.28 | $2,190 |

Two things worth carrying forward when quoting this:

- **Median uplift is 0 at windows 1 and 5.** With win rates under 27%, most orders get zero
  uplift from this policy (APEX lost, so Fynd's own quote would have been used anyway) — only at
  w30's >50% win rate does the median move off zero.
- **As a share of notional this is small** — 0.02–0.14% depending on window/bracket, not the
  5–20bps `uplift_mean_bps` alone suggests. The gap is because `uplift_mean_bps` is an unweighted
  per-order average while the $-figure is notional-weighted: the orders APEX wins on skew smaller
  than the population average. State which one you're quoting.

## Batching alone (APEX batch vs. APEX solving each order in isolation)

Isolates the batching effect from the Fynd-comparison question above — does bundling orders into
one clearing help APEX itself, independent of how it stacks up against Fynd.

| window | bracket | n | median bps | win rate |
|---|---|---|---|---|
| 1 | bottom | 4,168 | 1.98 | 55.8% |
| 1 | top | 5,270 | 6.23 | 58.7% |
| 5 | bottom | 5,968 | 5.39 | 59.6% |
| 5 | top | 6,502 | 7.81 | 61.2% |
| **30** | bottom | 8,418 | **52.52** | **81.2%** |
| **30** | top | 8,408 | **52.84** | **81.2%** |

Batching pays off for APEX itself much more clearly than it does relative to Fynd — even at
window 1 there's a small positive median and >55% win rate against the no-batching baseline. The
30-block jump (52bps median, 81% win rate) is the clearest positive result in this whole study.
(Means for this comparison are unreliable — see the methodology note above; use median/win-rate.)

## Internalization (share of filled notional crossed order-vs-order, not through a pool)

Only cleanly re-measured for windows 5 and 30 after the final live_join.py fixes; window 1 needs a
fresh pull (see Data section) before citing — don't reuse older window-1 internalization figures
from earlier in this investigation, they predate either the dust-floor fix, the slipstreams fix,
or both.

| window | bracket | share |
|---|---|---|
| 5 | bottom | 12.8% |
| 5 | top | 12.0% |
| 30 | bottom | 48.3% |
| 30 | top | 47.7% |

Internalization rises steeply with window size and pool size — see `LIVE-MONITOR-FINDINGS.md` for
why the pre-fix ~0.94 pooled figure was largely a search-non-convergence artifact, not a real
liquidity story.

## Bottom line

Single-block batching does not beat Fynd's own routing — it loses on a clear majority of orders.
The value in batching only shows up once there's enough accumulated order flow to actually cross:
by 30 blocks, APEX batching is roughly break-even against Fynd (win rate crossing 50%), clearly
better than not batching at all (81% win rate, 52bps median vs. solving orders individually), and
internalizing about half of filled notional order-against-order rather than through pools. The
"use the better of APEX or Fynd" policy is worth roughly **$750–$2,000/day** at current volume,
scaling with batch window — real, but a small number relative to total flow, and one that should
be re-measured once the `tycho-simulation` fee-markup fix (see findings doc) lands, since it
affects the majority of the pool universe this whole dataset was computed against.
