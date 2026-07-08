---
icon: scale-balanced
---

# Split variant comparison

Three split variants coexist on this branch so a controlled benchmark can pick the production
default. This page records the comparison plan and known results so far; it will be replaced by
the outcome once the head-to-head runs.

## Contenders

| | `split` | [`split_probe`](split-probe.md) | [`split_bounded`](split-bounded.md) |
| --- | --- | --- | --- |
| Candidate discovery | Exhaustive BFS enumeration | Exhaustive BFS enumeration | Bounded amount-aware expansion (direct / connector / anchor) |
| Exit selection | Derived spot-depth score, first-hop diversity | Live two-point probes per exit | Implicit via live frontier simulation |
| Graph type | `DepthAndPrice` weights (derived) | `DepthAndPrice` weights (derived) | Weightless `()` |
| Derived data | Spot/depth for ranking, token gas prices for net | Token gas prices for net only | None |
| Net ranking | Gross minus gas in output-token terms | Gross minus gas in output-token terms | Gross only (gas-blind) |
| Single-path floor | Yes | Yes | Yes |
| Observed solve time (blue-chip, all\_onchain) | ~1.2–1.9 s | ~1.1–1.8 s | ~70–100 ms |

Known results, separate sessions on 2026-07-07 (different blocks — directional only):

* `split_probe` beat Bellman-Ford on all six blue-chip showcase trades, +7.9% to +161%
  ([details](split-probe.md#benchmark-results)).
* `split_bounded` beat PFW/BF on the same trade set, +8.8% to +165.6%, at ~86 ms mean solve
  ([details](split-bounded.md#benchmark-notes-from-the-source-branch-2026-07-07)).
* Per-trade quality of the two looks similar; latency differs by ~15x in `split_bounded`'s favor.

## Comparison protocol

Same-session, same-server, per-order interleaving is not possible with one solver pool per
algorithm, so run all three pools in **one** server so every quote request fans out to all
variants against the same block:

```toml
[pools.split]
algorithm = "split"
num_workers = 1
min_hops = 1
max_hops = 4
timeout_ms = 60000
max_routes = 1024

[pools.split_probe]
algorithm = "split_probe"
num_workers = 1
min_hops = 1
max_hops = 4
timeout_ms = 60000
max_routes = 1024

[pools.split_bounded]
algorithm = "split_bounded"
num_workers = 1
min_hops = 1
max_hops = 4
timeout_ms = 60000
max_routes = 1024
```

The worker router picks the best pool per order; per-pool results appear in worker logs. For
attribution, also run each pool in isolation against the frozen-snapshot offline harness
(`feat/split-routing-benchmark`).

Measure, per variant:

1. **Quality**: exact integer net output per trade; win/loss/tie counts against each other and
   against Bellman-Ford and Path Frank-Wolfe as single-path references; mean bps delta.
2. **Latency**: p50/p95 solve time per order (matters for quote SLAs; `split_bounded`'s ~86 ms vs
   ~1.5 s is the headline difference to confirm).
3. **Coverage**: solved-order fraction, including small orders, dust, exotic pairs, and orders
   where splitting does not pay (all three floor to `InsufficientLiquidity` by design — a
   single-path pool must run alongside).
4. **Gas realism**: `split_bounded` ranks gas-blind; compare net-of-gas outcomes specifically on
   small orders and many-path routes where gas dominates, where it should be weakest.
5. **Robustness**: repeat the 1k-request offline sample (seeds 42 and 4242) for statistical
   backing, not just the six showcase trades.

Trade set: the six blue-chip showcase orders (100k AAVE→USDC, 1M UNI→USDC, 2M LINK→USDC,
10k WETH→AAVE/UNI/LINK), plus the offline 1k-request samples, plus a small-order band
(0.1–10 WETH-equivalent) to exercise the gas-accounting difference.

## Decision criteria

Prefer the variant with the best net-output win rate that stays inside the production latency
budget. If quality is tied within noise, prefer `split_bounded` for latency; if `split_bounded`
loses measurably on gas-heavy or long-tail-exit orders, consider porting its bounded discovery
into the gas-aware ranking of `split_probe` — the approaches compose.

Once decided: keep the winner, delete the losers (replace, don't deprecate), and fold the
result into this page.
