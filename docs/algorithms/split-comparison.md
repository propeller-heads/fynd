---
icon: scale-balanced
---

# Split variant comparison

Three split variants briefly coexisted on this branch so a controlled benchmark could pick the
production default. The benchmark ran on 2026-07-08; **`split_bounded` won** and the other two
variants (`split`, `split_probe`) were deleted. This page records the comparison and its results.

## Contenders

| | `split` | `split_probe` | `split_bounded` |
| --- | --- | --- | --- |
| Candidate discovery | Exhaustive BFS enumeration | Exhaustive BFS enumeration | Bounded amount-aware expansion (direct / connector / anchor) |
| Exit selection | Derived spot-depth score, first-hop diversity | Live two-point probes per exit | Implicit via live frontier simulation |
| Graph type | `DepthAndPrice` weights (derived) | `DepthAndPrice` weights (derived) | Weightless `()` |
| Derived data | Spot/depth for ranking, token gas prices for net | Token gas prices for net only | Token gas prices for net only (optional) |
| Single-path floor | Yes | Yes | Yes |

## Protocol

Two evidence sources, both with all variants against identical market state:

1. **Live same-block run**: one server with five pools (`split`, `split_probe`, `split_bounded`,
   `bellman_ford`, `path_frank_wolfe`), `min_responses: 0` so every pool answered every order.
   26 orders x 3 passes: six blue-chip showcase trades (100k AAVE→USDC, 1M UNI→USDC,
   2M LINK→USDC, 10k WETH→AAVE/UNI/LINK), 5x and 10x XXL versions, and a small-order band
   (0.1–10 WETH-equivalent). Block consistency verified per order across all pools.
2. **Offline 1k-request samples** (seeds 42 and 4242) on the frozen-snapshot harness, max_hops 4.

## Results (2026-07-08)

Latency, same-block (five pools sharing one machine, so relative numbers are the signal):

| pool | p50 | p95 |
| --- | ---: | ---: |
| `split` | ~2.0 s | ~2.7 s |
| `split_probe` | ~2.0 s | ~2.7 s |
| `split_bounded` | 134 ms | 149 ms |
| `bellman_ford` (reference) | 137 ms | 230 ms |

Quality:

* All three variants beat Bellman-Ford on every blue-chip and XXL order, by +3,200 to
  +16,200 bps. XXL sizes saturate the books; `split_bounded` stayed ahead there, winning the XXL
  head-to-head against both exhaustive variants (24/36 orders, mean +85–89 bps).
* `split_probe` never lost to `split` (live or offline) but shared its ~2 s latency and lost the
  overall head-to-head to `split_bounded`.
* Offline, `split_bounded` (gas-aware) vs `split` on the common-success set: 15W/6L (seed 42) and
  28W/9L (seed 4242). Against Bellman-Ford on its solved set: 66W/2L (+96 bps mean, seed 42) and
  81W/2L (+110 bps mean, seed 4242).
* Coverage: all variants floor out orders where splitting does not pay (by design — a single-path
  pool must always run alongside). Gas-aware netting makes `split_bounded` decline more small
  orders than the gas-blind port did; the declined orders were worth ≤ ~1 bps over Bellman-Ford,
  while the gas-blind version had been *winning* some of them only because it ignored gas
  (17W/4L at ~0.2 gwei flipping to 0W/21L at 10 gwei in a fair-net sensitivity check).

### Against Path Frank-Wolfe

`path_frank_wolfe` is the strongest pre-existing algorithm on large trades, so the winner also
had to beat it. Gas-aware `split_bounded` vs PFW, exact integer nets:

| evidence | W–L (ties) | mean bps | median bps |
| --- | --- | ---: | ---: |
| Live same-block, all orders | 36–2 | +6,461 | +5,328 |
| Live same-block, XXL only | 24–0 | +7,759 | +7,647 |
| Offline 1k sample, seed 42 (common set) | 65–3 | +95.2 | +1.08 |
| Offline 1k sample, seed 4242 (common set) | 73–10 | +31.5 | +1.51 |

The only live losses were the two passes of 10k WETH→LINK at −0.3 bps (tie-level noise).
Coverage is asymmetric by design: `split_bounded` returns `InsufficientLiquidity` when no split
beats the best single path net-of-gas, so PFW/BF answer those orders — the split pool only
speaks when splitting pays, and when it speaks it wins.

### What the winner inherited

`split_bounded` consolidates the learnings from every line of work in this comparison:

* Bounded amount-aware candidate discovery with connector/anchor priorities and the native-ETH
  sentinel anchor (from the `bounded_split` line) — the source of the ~15x latency advantage.
* Gas-aware net ranking: `combined_net`, per-path activation costs in both allocators, and a
  net-of-gas single-path floor (ported from `split`/`split_probe` after the gas sensitivity
  check above).
* Fill-loop leftover top-up so partial allocations always cover the full order (from the
  `split` hardening pass).

Deliberately not ported: `split_probe`'s live-probed exit ranking (bounded's frontier
simulation selects exits with live pool math already; probe's one +33% outlier on 100k
AAVE→USDC is the composition to revisit if exit choice ever needs sharpening) and the derived
spot-depth ranking hardening (NaN-safe sort, first-hop diversity), which has no target —
`split_bounded` uses no derived ranking.

Decision per the pre-registered rule (best net-output win rate inside the latency budget; the
gas-aware ranking of the exhaustive variants composed with bounded discovery): keep
`split_bounded` with gas-aware netting ported from `split`, delete the rest.

Known follow-up: bounded discovery can miss long-tail routes the exhaustive search found
(worst offline case: one USDS→USDC trade at +3,010 bps for `split` that `split_bounded` never
solved, in either the gas-blind or gas-aware version). Anchor-set tuning is the candidate fix.

Raw artifacts: `~/Documents/llm-output/2026-07-08-fynd-split-comparison/` (local).
