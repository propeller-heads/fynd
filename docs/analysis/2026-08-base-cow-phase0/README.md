# Base batching potential — Phase 0: coincidence-of-wants analysis

Paper trail for the July/August 2026 pre-analysis that gated the APEX batch-solver simulation:
how much value would batch settlement add on Base, measured from 10 days of decoded on-chain
flow before building any simulation.

## Headline results (10 days, 2026-07-25 → 08-03, 868,783 decoded trades)

- **Single-block batching adds ~0.02 bps per trade on top of Fynd** (~$330/wk). 99.4% of volume
  has no direct opposite trade in its block; 98.3% has no counterparty even at the token level,
  so no matching mechanism (direct, ring, or route-mediated) can touch it.
- On the 0.6% that matches: crossing is worth ~44 bps vs executed prices but only ~4 bps vs
  Fynd's own quotes — Fynd's routing already captures ~90% of the gap vs settled executions.
- Matching scales with the batching window, not the mechanism: 1-min window matches 11.6% of
  volume (~$450k/yr vs Fynd), 5-min 21.2% (~$1M/yr).
- The one unpriced mechanism: route-mediated netting (66% of theoretically matchable volume at
  single-block). Pricing it requires the APEX simulation — the follow-up phase.
- **Biggest non-batching finding:** 25% of all Base solver trades are in Uniswap v4 hooked pools
  (creator coins) invisible to tycho's plain `uniswap_v4` indexing — the largest solver-coverage
  lever identified.
- Excluded via sender verification: the Lydia Coins (lydiacoins.com) BSW/USAD wash operation —
  10 wallets funded by one operator via Disperse.app, ~$239M gross/4d with net flow ≈ 0. Would
  have fabricated $101M of "matched" volume (the perfect synthetic CoW).

## Files

| File | What |
|---|---|
| `PLAN.md` | Full working plan: decisions log, APEX-phase architecture, agent findings, implementation queue |
| `cow_scan.py` | The scan (stdlib Python): intent loading, quarantine, ETH≡WETH canonicalization, solvable-universe split, pairwise/ring/multilateral decomposition per tumbling window, two surplus baselines, wash-pair exclusion |
| `cow_scan_results.json` | Full scan output (per-window metrics, categories, volume buckets, per-day counters) |
| `p2p_matches_w1.json` | Single-block matches in the no-route slice (wash-check input; sorted by surplus) |
| `cow-phase0-report.html` | The report (self-contained; includes methodology appendix) |
| `cow-methodology.html` | Standalone methodology page (v2, with trap log and worked examples) |

## Data source & reproduction

Input data: `s3://propellerheads-hindsight/staging/base/comparisons/` (hindsight monitor JSONL,
one file per UTC day; collection began 2026-07-25). Sync locally and run:

```bash
aws s3 sync s3://propellerheads-hindsight/staging/base/comparisons/ <data-dir>/base-comparisons/
python3 cow_scan.py   # expects base-comparisons/ next to itself; ~4 min for 10 days
```

The scan is deterministic. When extending the window past the address-book/aerodrome_v1
deploys, segment the series at the deploy date rather than averaging through it.

## External verification used

- Allium (`base.dex.aggregator_trades`, `base.raw.transactions`): universe cross-check,
  5-week representativeness (match rate 0.29–0.39% in 4 of 5 weeks), wash sender lookup.
- Dune (`dex_aggregator.trades`, decoded call tables): solver coverage discovery, slippage
  tolerance study (n=27k; median configured slippage ≈ 100 bps; presets at 100/200 bps).
