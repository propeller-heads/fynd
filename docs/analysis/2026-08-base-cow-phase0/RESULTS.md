# APEX live batching study — results (final)

Numbers from the Hetzner live monitor (`mp/feat/apex-live-monitor`), joined offline with
`live_join.py`. See [`LIVE-MONITOR-FINDINGS.md`](LIVE-MONITOR-FINDINGS.md) for how the monitor
got here (deadline-pressure investigation, taxonomy fixes, the two correctness bugs found along
the way) — read that first if a number here looks surprising, since several of the fixes
described there directly changed what these numbers say.

## Data

**Final.** The monitor ran from **2026-08-07 20:36:27 UTC to 2026-08-12 07:59:21 UTC** (4.4742
days) on the final config, then was stopped and its systemd units disabled — no more data is
coming. **383,309 comparisons** joined from the full run. One watchdog-caught stall on
2026-08-09; otherwise continuous, and the last write on both streams parses cleanly (no data lost
to the shutdown, which itself needed `SIGKILL` — the process didn't exit on `SIGTERM` within
systemd's timeout, the same behavior the watchdog saw during the mid-run stall).

```bash
python3 docs/analysis/2026-08-base-cow-phase0/live_join.py <data-dir> --window 1   # or 5, 30
```

**Methodology notes — read before using these numbers**:

- Fills below $1 settled USD notional are excluded from all median/mean/win-rate stats (not from
  USD sums) — see `LIVE-MONITOR-FINDINGS.md` for why $1 and not the $100 first tried.
- `mean_bps` is reported but the median is the number to trust; a handful of outliers can still
  move a mean even after the dust floor.
- `batch_vs_singles`'s mean is separately unstable for an unrelated reason (see Known open items
  in the findings doc) — use its median/win-rate only.
- **USD-weighted totals are not outlier-robust.** One example found in this final pull: the
  `top, vs Fynd, window=5` uplift total ($434,236) is 99% a single $427k order with a ~9,941bps
  "win" — almost certainly one badly-priced Fynd quote, not real signal. Before quoting any
  USD/day figure as a business number, check for single-order dominance the way that example was
  found (sort per-order USD contribution, inspect the top few) — this pass did not audit every
  cell for it, only the one that looked obviously wrong.

## Headline: APEX vs Fynd, same block state, top-of-block

| window | n | median bps | mean bps | APEX win rate |
|---|---|---|---|---|
| 1 block | 14,193 | **-22.26** | -34.47 | **13.4%** |
| 5 blocks | 22,436 | -23.28 | -35.90 | 23.6% |
| **30 blocks** | 38,348 | **+2.04** | +6.08 | **55.1%** |

At single-block granularity APEX clearly loses to Fynd's per-order routing — median 22bps worse,
wins about 1 time in 7. The picture flips only once there's enough order flow in one batch to
cross: win rate climbs 13% → 24% → 55% from window 1 to 30, and the median result goes from a
clear loss to a small but real win.

## Full breakdown, both brackets and baselines

"top" = state N-1 (before this block's own trades executed, the fair/headline comparison);
"bottom" = state N (after, the biased-by-its-own-impact comparison). "vs Fynd" compares against
Fynd's own quote at the same state; "vs executed" compares against what the trade actually
settled for on-chain (whichever solver won it).

| window | bracket | baseline | n | median bps | win rate |
|---|---|---|---|---|---|
| 1 | top | fynd | 14,193 | -22.26 | 13.4% |
| 1 | top | executed | 14,196 | -18.18 | 29.5% |
| 1 | bottom | fynd | 12,080 | -11.75 | 17.5% |
| 1 | bottom | executed | 12,083 | -14.54 | 31.9% |
| 5 | top | fynd | 22,436 | -23.28 | 23.6% |
| 5 | top | executed | 22,445 | -20.69 | 30.0% |
| 5 | bottom | fynd | 21,471 | -11.27 | 29.3% |
| 5 | bottom | executed | 21,481 | -20.56 | 29.9% |
| 30 | top | fynd | 38,348 | 2.04 | 55.1% |
| 30 | top | executed | 38,356 | 8.35 | 60.5% |
| 30 | bottom | fynd | 38,490 | 3.46 | 57.9% |
| 30 | bottom | executed | 38,499 | 8.11 | 60.4% |

## Mean bps *if won* (conditional on APEX beating the baseline, vs Fynd)

| window | bracket | win median bps | win mean bps | win rate |
|---|---|---|---|---|
| 1 | top | 18.98 | 65.74 | 13.4% |
| 1 | bottom | 19.14 | 57.41 | 17.5% |
| 5 | top | 29.46 | 68.37 | 23.6% |
| 5 | bottom | 27.29 | 59.10 | 29.3% |
| 30 | top | 30.11 | 41.48 | 55.1% |
| 30 | bottom | 30.38 | 42.33 | 57.9% |

When APEX wins, it wins by a real margin (41–68bps mean) at every window. The problem at small
windows is win *frequency*, not win *size*.

## "Use APEX only when it beats Fynd" — best-of-policy uplift

Per order: `uplift = max(apex_bps - fynd_bps, 0)` — route through APEX when it wins, fall back to
Fynd otherwise, so the policy never does worse than Fynd alone. Averaged over every real
(≥$1) fill; losses count as zero, not negative. $/day computed over the full 4.4742-day run.

| window | bracket | uplift mean bps | uplift median bps | uplift $ (4.47d) | **$/day** |
|---|---|---|---|---|---|
| 1 | top | 8.79 | 0.00 | $21,626 | **$4,835** |
| 1 | bottom | 10.02 | 0.00 | $16,214 | $3,624 |
| **5** | **top** | 16.14 | 0.00 | **$434,236** ⚠ | **$97,062** ⚠ |
| 5 | bottom | 17.34 | 0.00 | $32,467 | $7,258 |
| 30 | top | 22.85 | 2.04 | $39,456 | $8,821 |
| 30 | bottom | 24.50 | 3.46 | $86,081 | $19,241 |

⚠ **Do not quote the window-5/top figure as-is** — one $427k order accounts for $405k of the
$434k (see Data section). With that order excluded the figure would land far closer to the
window-1/window-30 pattern; re-run with an explicit exclusion or a notional cap before using this
row.

Two things worth carrying forward when quoting the rest of this table:

- **Median uplift is 0 at windows 1 and 5.** With win rates under ~30%, most orders get zero
  uplift from this policy (APEX lost, so Fynd's own quote would have been used anyway) — only at
  w30's >50% win rate does the median move off zero.
- **The $-figures are notional-weighted; `uplift_mean_bps` is not.** The two tell different
  stories (a few large orders can dominate the dollar figure while contributing little to the
  per-order bps average, or vice versa) — state which one you're quoting, and see the outlier
  warning above for how badly this can go wrong unaudited.

## Batching alone (APEX batch vs. APEX solving each order in isolation)

Isolates the batching effect from the Fynd-comparison question above — does bundling orders into
one clearing help APEX itself, independent of how it stacks up against Fynd.

| window | bracket | n | median bps | win rate |
|---|---|---|---|---|
| 1 | bottom | 8,401 | 1.58 | 55.9% |
| 1 | top | 10,457 | 5.38 | 58.5% |
| 5 | bottom | 10,953 | 7.88 | 60.8% |
| 5 | top | 11,884 | 10.82 | 62.3% |
| **30** | bottom | 16,780 | **53.24** | **82.5%** |
| **30** | top | 16,763 | **53.24** | **82.5%** |

Batching pays off for APEX itself much more clearly than it does relative to Fynd — even at
window 1 there's a small positive median and >55% win rate against the no-batching baseline. The
30-block jump (53bps median, 82% win rate) is the clearest positive result in this whole study.
(Means for this comparison are unreliable — see the methodology notes above; use median/win-rate.)

## Internalization (share of filled notional crossed order-vs-order, not through a pool)

| window | bracket | share |
|---|---|---|
| 1 | bottom | 8.6% |
| 1 | top | 5.1% |
| 5 | bottom | 22.0% |
| 5 | top | 20.8% |
| **30** | bottom | **51.4%** |
| **30** | top | **50.8%** |

Internalization rises steeply with window size and pool size — see `LIVE-MONITOR-FINDINGS.md` for
why the pre-fix ~0.94 pooled figure (measured before the taxonomy fix and the slipstreams fix)
was largely a search-non-convergence artifact, not a real liquidity story.

## Bottom line

Single-block batching does not beat Fynd's own routing — it loses on a clear majority of orders.
The value in batching only shows up once there's enough accumulated order flow to actually cross:
by 30 blocks, APEX batching wins outright against Fynd (55% win rate, +2bps median), clearly
better than not batching at all (82% win rate, 53bps median vs. solving orders individually), and
internalizes just over half of filled notional order-against-order rather than through pools.

The "use the better of APEX or Fynd" policy is worth on the order of **$3,600–$19,000/day** at
current volume across the reliable rows of that table (the window-5/top row is excluded pending
the outlier fix above) — real, and larger than the day-1 estimate suggested, but this whole
dataset was computed against a pool universe missing `aerodrome_slipstreams`/`lunarbase` and
running on a `tycho-simulation` version with a confirmed target-price bug affecting the majority
pool family (`uniswap_v3`) — see `LIVE-MONITOR-FINDINGS.md`. **Re-measure once that fix lands**
before treating these numbers as final for a decision.
