---
icon: crosshairs
---

# Split Probe

`split_probe` is the [Split](split.md) algorithm with one change: first-hop exit selection uses
live pool math instead of the derived spot-depth heuristic. Everything else — candidate
enumeration, full-amount ranking, the pool-disjoint and shared-pool allocators, route assembly,
and the single-path floor — is shared with `split`.

## Why exit selection is the critical decision

The first hop carries the entire order, so price impact concentrates there; downstream hops only
carry post-split fractions. Benchmarks on large blue-chip trades showed that when the derived
spot-depth score misjudges exits, `split` routes the whole order through a shallow pool and loses
most of its value there, no matter how well the allocators work afterwards. The heuristic
misjudges exits in three ways:

1. `get_limits`-based depth undervalues concentrated liquidity, so v2-style pools outrank much
   deeper v3/v4/curve pools.
2. A path with any edge missing derived spot, depth, or token price data scores `f64::MIN` and is
   effectively excluded — pool depth computations fail for dozens of pools per block, and token
   gas prices cover only part of the token set.
3. The score is a zero-size linearization; it cannot answer "which exits absorb this order size".

## How the probe works

For every distinct first-hop exit (pool + receive token), `split_probe` runs two simulations:
one at a near-marginal amount (`order / 1024`) and one at the full order amount. The ratio of the
two average prices is a dimensionless efficiency in `(0, 1]`:

```
efficiency = (out_full / in_full) / (out_marginal / in_marginal)
```

Deep exits stay near 1.0 at full size; saturated exits collapse toward 0. Because both probes use
the same pool and token pair, the units cancel — efficiencies are comparable across exits into
different tokens without any price data. Candidate truncation visits exit buckets in efficiency
order under the same first-hop diversity round-robin as `split`.

Probing is capped at 64 exits (two simulations each, mostly analytic pools), which adds no
measurable latency: benchmark solves stayed at 1.1–1.8 s, unchanged from `split`.

## Benchmark results

Live rerun of the blue-chip large-trade showcase, 2026-07-07, protocols `all_onchain` via
`tycho-beta.propellerheads.xyz`, Bellman-Ford as the same-session single-path reference.
Full artifacts: `~/Documents/llm-output/2026-07-07-fynd-probe-eval/`.

| Trade | split\_probe net | Solve | Bellman-Ford net | Solve | vs BF |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100,000 AAVE → USDC | 2,089,860 USDC | 1167 ms | 800,081 USDC | 162 ms | +161.2% |
| 1,000,000 UNI → USDC | 2,737,613 USDC | 1338 ms | 2,387,194 USDC | 120 ms | +14.7% |
| 2,000,000 LINK → USDC | 13,418,083 USDC | 1135 ms | 8,943,115 USDC | 129 ms | +50.0% |
| 10,000 WETH → AAVE | 50,697 AAVE | 1777 ms | 27,859 AAVE | 115 ms | +82.0% |
| 10,000 WETH → UNI | 3,424,331 UNI | 1834 ms | 2,928,780 UNI | 115 ms | +16.9% |
| 10,000 WETH → LINK | 2,136,072 LINK | 1761 ms | 1,980,287 LINK | 114 ms | +7.9% |

For context: on 2026-07-03 (before probe ranking and the shared truncation hardening), the same
six trades ran −40% to −95% against Bellman-Ford, and the AAVE order returned no route at all.
Route shapes confirm the mechanism: AAVE now exits through the native ETH wrapper and LINK
through the deep v3 pool — exactly the exits the heuristic buried before.

## Known limitations

* Probes cover the first hop only. A deep path can still lose candidate slots to a mid-path pool
  monopoly; if benchmarks surface this, the next lever is a bounded amount-aware search
  (beam-style generation or a Bellman-Ford anchor path).
* Probes cost up to 128 extra simulations per order. On markets where most exits are VM pools,
  this grows solve time; the cap bounds the worst case.
* Within a bucket, paths still use the spot-depth order — downstream hop quality is corrected by
  the full-amount ranking pass, not by probes.

## Source reference

| File | Purpose |
| --- | --- |
| `fynd-core/src/algorithm/split.rs` | Shared implementation; `ExitRanking` selects the variant |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"split_probe"` to the probe-ranked variant |
