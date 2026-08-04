# Stage 2 — real APEX on Base trades, zero AMMs

Markus's ladder, point 2: the analytic scan (cow_scan.py) gives the N-N matching
*ceiling*; this run puts the same headline universe through the actual apex-solver
(`tools/apex-batch/src/bin/stage2.rs`) with an empty pool set, so fills are pure
order-vs-order clearing under APEX's real mechanism — uniform clearing price per pair,
cluster formation, limit enforcement (synthetic floors: settled × (1 − bps)),
10 s offline deadline per component.

Realized vs ceiling, matched volume (share of the same intent USD denominator):

| window | limit | APEX matched | APEX % | pairwise ceiling % | multilateral ceiling % | realized/pairwise | realized/multilateral | net surplus | analytic pairwise surplus |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 50 bps | $1,240,232 | 0.551% | 0.583% | 1.705% | 0.94× | 0.32× | $8,757 | $5,760 |
| 1 | 100 bps | $1,243,264 | 0.552% | 0.583% | 1.705% | 0.95× | 0.32× | $8,814 | $5,760 |
| 1 | 200 bps | $1,254,284 | 0.557% | 0.583% | 1.705% | 0.96× | 0.33× | $8,882 | $5,760 |
| 5 | 50 bps | $7,702,247 | 3.420% | 3.34% | 7.426% | 1.02× | 0.46× | $25,710 | $15,137 |
| 5 | 100 bps | $7,703,216 | 3.420% | 3.34% | 7.426% | 1.02× | 0.46× | $25,810 | $15,137 |
| 5 | 200 bps | $7,767,815 | 3.449% | 3.34% | 7.426% | 1.03× | 0.46× | $25,986 | $15,137 |
| 15 | 50 bps | $17,539,199 | 7.788% | 7.618% | 15.143% | 1.02× | 0.51× | $43,802 | $28,328 |
| 15 | 100 bps | $17,438,493 | 7.743% | 7.618% | 15.143% | 1.02× | 0.51× | $43,966 | $28,328 |
| 15 | 200 bps | $17,527,005 | 7.782% | 7.618% | 15.143% | 1.02× | 0.51× | $44,086 | $28,328 |
| 30 | 50 bps | $27,369,304 | 12.153% | 11.602% | 21.513% | 1.05× | 0.56× | $60,167 | $38,486 |
| 30 | 100 bps | $27,649,507 | 12.277% | 11.602% | 21.513% | 1.06× | 0.57× | $60,059 | $38,486 |
| 30 | 200 bps | $27,811,849 | 12.349% | 11.602% | 21.513% | 1.06× | 0.57× | $60,442 | $38,486 |
| 150 | 50 bps | $51,200,256 | 22.734% | 21.2% | 37.558% | 1.07× | 0.61× | $121,779 | $75,435 |
| 150 | 100 bps | $52,045,927 | 23.110% | 21.2% | 37.558% | 1.09× | 0.62× | $123,380 | $75,435 |
| 150 | 200 bps | $51,481,283 | 22.859% | 21.2% | 37.558% | 1.08× | 0.61× | $125,264 | $75,435 |

Net surplus = positive per-order gaps vs settled MINUS the negative gaps a uniform
clearing price imposes on the other side within its limit slack. The positive-only sum
grows mechanically with the allowed slack (50→200 bps) while the net is nearly invariant —
the net column is the mechanism's real value creation vs what settled on-chain.

Counters (100 bps cells):

- w=1: orders_in=541,983 filled=11,201 partial=9,731 unfilled_at_limit=315,046 cluster_cut=0 errored=1,147 ({'clearing_under_limit': 358}) panics=0 singles_skipped=204,159 wash_excl=0 unpriced=8 underflow=0 neg_gaps=10,152 solves=104,758 wall=306ms
- w=5: orders_in=541,983 filled=43,626 partial=30,299 unfilled_at_limit=389,176 cluster_cut=0 errored=8,666 ({'clearing_under_limit': 972}) panics=0 singles_skipped=68,842 wash_excl=0 unpriced=8 underflow=0 neg_gaps=34,871 solves=80,190 wall=1643ms
- w=15: orders_in=541,983 filled=93,418 partial=44,938 unfilled_at_limit=341,564 cluster_cut=0 errored=16,891 ({'clearing_under_limit': 742}) panics=0 singles_skipped=43,640 wash_excl=0 unpriced=8 underflow=0 neg_gaps=62,499 solves=33,620 wall=3822ms
- w=30: orders_in=541,983 filled=151,898 partial=53,005 unfilled_at_limit=277,580 cluster_cut=0 errored=22,317 ({'clearing_under_limit': 504}) panics=0 singles_skipped=35,646 wash_excl=0 unpriced=8 underflow=0 neg_gaps=85,855 solves=20,174 wall=8918ms
- w=150: orders_in=541,983 filled=250,923 partial=32,331 unfilled_at_limit=222,291 cluster_cut=0 errored=21,384 ({'clearing_under_limit': 82, 'trade_solver': 23}) panics=0 singles_skipped=13,516 wash_excl=0 unpriced=8 underflow=1 neg_gaps=103,323 solves=10,593 wall=3656ms

Conventions: matched volume counts each filled order at its own USD value (both sides of
a cross), mirroring the analytic 2× convention; surplus is vs the settled baseline,
positive per-order gaps only, valued at day-median USD prices (negatives counted apart).
The intent USD denominator includes the wash pair (as in the analytic scan); its orders
never enter APEX. Decimals-free scheme: zero pools ⇒ tokens declared 18-dec, raw amounts,
per-raw-unit prices — exact, see the binary's module docs.

## Engine-inclusive fynd baseline (plan item L; added 2026-08-04)

Each APEX-filled order is also compared against **fynd's own N−1 quote** for that trade
(`top.fynd_amount_out`, pro-rata for partial fills), on the subset fynd solved
(`fynd_compared`; APEX fills without a fynd quote are counted `fynd_uncompared`, ~5%).
USD deltas are valued against each order's quarantined USD notional (re-pricing raw units
lets one wrong-decimals quote dominate the sum).

| window | compared | apex ≥ fynd | median bps | mean bps | Σ delta (10 d) |
|---|---|---|---|---|---|
| 1 | 19,887 | 48.3% | −28.8 | +6.3 | +$4.6k |
| 5 | 71,042 | 49.8% | −5.6 | +11.5 | +$9.3k |
| 15 | 134,147 | 52.0% | +26.5 | +19.2 | +$13.0k |
| 30 | 199,933 | 53.9% | +31.8 | +24.5 | +$15.4k |
| 150 | 277,305 | 58.8% | +39.8 | +31.0 | +$39.1k |

(100 bps floor cell; the 50 bps cells shift medians up — tighter floors force clearings closer
to settled — without moving the Σ delta by more than ~2%.)

Reading: order-vs-order clearing with zero AMMs is **roughly at par with fynd per-order routing
at the single block** (median slightly negative, mean and total positive — the uniform clearing
price redistributes within the limit band), and pulls ahead as windows grow: at 5 minutes APEX
beats fynd's quote on 59% of compared fills, median +40 bps, ≈ +$3.9k/day on the compared
subset. This is the engine-inclusive view; the batching-isolated headline remains
apex(batch) vs apex(singles).
