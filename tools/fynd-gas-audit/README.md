# fynd-gas-audit

Tool that quotes real aggregator trades through a local Fynd instance,
simulates each on mainnet via `FyndClient::execute_swap` in dry-run mode (an
`eth_call` against the fully-assembled signed transaction with storage + balance
overrides), and reports — in ETH — how far Fynd's quote-time `gas_estimate`
drifts from the actual gas a transaction would consume.

---

## Why this audit exists

Fynd ranks route candidates by `amount_out_net_gas = amount_out − gas_cost`. If
`gas_estimate` is biased, the ranking is biased: a systematically-low
`gas_estimate` makes every route look cheaper than it actually executes. This
tool quantifies the bias.

---

## Running it

1. Start Fynd in one terminal (ithaca RPC is recommended — LlamaRPC gets
   rate-limited by the simulator's slot-detection probes):
   ```bash
   TYCHO_API_KEY=supersecrettoken \
   RPC_URL=https://reth-ethereum.ithaca.xyz/rpc \
      cargo run --release -p fynd -- serve -w tools/fynd-gas-audit/single_swaps.toml
   ```
   Wait until `/v1/health` reports `healthy=true, derived_data_ready=true`.

   `single_swaps.toml` is a worker config tailored to this audit: it pins
   `max_hops=1` on both `most_liquid` and `bellman_ford` pools so every
   sampled trade resolves to a single-hop swap. Use it when you want to
   isolate per-protocol gas accuracy without multi-hop overhead confounding the
   numbers. Drop the `-w` flag to fall back to Fynd's default
   `worker_pools.toml`, which allows multi-hop routing.
2. Run the audit in a second terminal:
   ```bash
   RPC_URL=https://reth-ethereum.ithaca.xyz/rpc \
     cargo run --release -p fynd-gas-audit -- --n 100 --max-per-pair 20
   ```
3. Artifacts are written to `tools/fynd-gas-audit/out/` (gitignored):
   - `trades.json` — the 100 sampled trades, reproducible given the same seed
   - `results.csv` — one row per trade with estimate, actual, status, reason
   - `report.md` — aggregate table + worst-10 trades + interpretation notes

CLI flags: `--n`, `--max-per-pair`, `--seed`, `--dataset`, `--fynd-url`,
`--rpc-url`, `--out-dir`, `--sender`. Defaults produce the canonical
100-trade audit.

**Vary `--seed` between runs.** It defaults to `42` for reproducibility, but
that means back-to-back runs draw the same sample. If you're using the audit
to validate a gas-estimation change, sticking to one seed lets you overfit to
that specific 100-trade slice. Pass a fresh `--seed` (e.g. `--seed $RANDOM`)
each time you want a fresh, independent draw from the 10k dataset.

---

## Findings from the 2026-05-08 run (single-hop, n=500)

This run used `single_swaps.toml` (1-hop only, both `most_liquid` and
`bellman_ford` pools) so the error attribution is per-protocol with no
multi-hop overhead in the way. Mainnet gas price at the time was 6.09 gwei.
Absolute ETH numbers scale linearly with gas price — the **relative** figures
are the ones that matter.

**Sample yield (500 trades attempted):**

| outcome | count |
|---|---|
| success (quote + simulation) | 155 |
| Fynd returned no route | 312 |
| simulation reverted | 33 |

The no-quote rate (~62%) is high because pinning `max_hops=1` rules out every
trade whose only routable path needs an intermediate token. The 33 simulation
reverts are tokens whose balance/allowance storage slots the simulator's
brute-force probe (slots 0..=20) couldn't locate — proxy patterns or
non-Solidity-default storage layouts, biased toward small-caps. See
limitation #2.

**Gas accuracy on the 155 successful trades:**

| metric | value |
|---|---|
| Trades where Fynd **under**-estimated | **155 / 155** |
| Trades where Fynd over-estimated | 0 / 155 |
| Mean \|error\| / actual cost | **37.75%** |
| Median signed error (ETH) | -0.000453 |
| Mean signed error (ETH) | -0.000532 |
| P95 absolute error (ETH) | 0.001056 |
| Sum of signed error (155 trades, ETH) | -0.082530 |

**Headline:** Fynd is systematically low by ~38% on gas. **Zero** over-estimates
in the sample.

**By protocol** (single-hop isolates the simulator under test):

| protocol | n | mean \|error\| / cost |
|---|---|---|
| ekubo_v3 | 9 | **90.03%** |
| pancakeswap_v3 | 2 | 44.25% |
| uniswap_v4 | 34 | 40.96% |
| uniswap_v3 | 80 | 37.46% |
| uniswap_v2 | 23 | 19.81% |
| sushiswap_v2 | 3 | 19.61% |
| pancakeswap_v2 | 4 | 12.25% |

Constant-product AMMs (uniswap_v2, sushi/pancake v2) sit in the 12–20% band;
tick-aware AMMs (uniswap v3/v4) are roughly 2× worse at 37–41%; ekubo_v3 is
the heaviest under-estimator at ~90%. Fix gas estimation there first.

**Worst offenders** (from the run's `report.md`):

- **Single largest absolute error**: `0x40d16f… → USDC`, a 1.2M-token swap
  where Fynd quoted `374,000` gas but actual was **1,222,007** (3.3× under,
  -0.0052 ETH on its own). High-baseline estimates undershoot when the pool
  is heavy (many tokens, hooks, metapool routing) — the static budget
  doesn't scale with pool complexity.
- **The `19,665` magic constant.** Fynd quotes exactly `19,665` gas on
  `USDC ↔ USDT` (actual 194k–233k) and `USDT ↔ WBTC` (actual ~200k) — both
  directions, multiple amounts. **10–12× under-estimate** every time. The
  same constant hitting multiple unrelated stablecoin/BTC pairs strongly
  suggests a default-fallback entry in the derived gas table that activates
  whenever a per-pair lookup misses, rather than a stale entry for one
  specific pair.
- `USDC → small-cap`: `132,000`–`134,000` → 280k–305k (~2.3× under). The
  recurring round numbers look like per-protocol static budgets rather than
  per-swap calculations.

**Pattern:** estimates cluster on round constants (`19,665`, `132,000`,
`134,000`, `374,000`) that look like per-protocol static budgets, not
per-swap dynamic costs. Real execution adds approvals, storage writes, and
router bookkeeping on top of the swap math, and the static budgets don't
model any of it.

---

## What the 38% means for route selection

- **Portfolio bias:** Fynd's `amount_out_net_gas` is consistently
  over-estimated by roughly 38% of the gas cost. At the 6.09 gwei this run
  saw, the per-trade loss is ~$1.20 on the mean (the -0.000532 ETH headline
  at ETH ≈ $2,285); at 20 gwei it scales to ~$4/trade.
- **Per-trade ranking:** a systematic bias doesn't by itself flip the winner,
  but it narrows margins. Higher-liquidity multi-hop routes that cost more gas
  lose ground to lower-liquidity single-hop routes whenever the gas cost is
  the tie-breaker. So the bias is more likely to matter when top-2 routes
  have similar `amount_out`.

Both claims can be checked with the CSV. `error_gas` is signed; group by
protocol to see where the ranking-flip risk concentrates (Ekubo v3 is the
clear outlier).

---

## Known limitations of the current audit

1. **`single_swaps.toml` ↔ no-quote rate trade-off.** With `max_hops=1`, ~62%
   of the 500 sampled aggregator trades return `NoRouteFound` because the only
   routable path needs an intermediate token (most multi-hop trades go via
   WETH/USDC). That's the cost of clean per-protocol attribution. Drop the
   `-w` flag and rerun against the default `worker_pools.toml` (2-hop) to
   recover most of those trades — at the price of mixing protocols within a
   route, which makes the per-protocol breakdown meaningless.
2. **Non-standard ERC-20 tokens break slot detection.** The simulator's
   balance/allowance slot probe (brute-force slots 0..=20) misses tokens that
   use proxy patterns or non-Solidity-default storage layouts (31 trades
   reverted for this reason on the 500-trade run, ~6%). These are biased
   toward small-caps.
3. **Gas price is a single snapshot.** We multiply every trade by
   `eth_gasPrice()` at the start of the run. Mainnet gas varies tick-to-tick,
   but for comparing estimate-vs-actual *on the same trade* this doesn't
   matter — the price cancels out of relative error.

---

## Architecture pointers

| module | file | purpose |
|---|---|---|
| types | `src/types.rs` | `AuditTrade`, `RowStatus`, `AuditRow` |
| sampler | `src/sampler.rs` | Parses the 10k aggregator dataset, stratified cap-per-pair sampling with deterministic seed |
| quoter | `src/quoter.rs` | `FyndClient` wrapper; one `POST /v1/quote` per trade |
| simulator | `src/simulator.rs` | Builds `StorageOverrides` (balance + allowance via the shared `erc20-overrides` crate, plus a huge native-ETH balance), then routes through `FyndClient::execute_swap` in dry-run mode (signs with `Signature::test_signature()`; recovers `gas_used` from the returned `SettledOrder::gas_cost()`) |
| report | `src/report.rs` | CSV writer (via serde) + markdown generator with aggregate table, worst-10, high-exclusion finding. `error_wei` for each row is derived on the fly from `error_gas × gas_price_wei` rather than stored on `AuditRow` |
| main | `src/main.rs` | CLI parsing (clap), dataset download, orchestration loop, ETH-price-via-Fynd for the report header |

Invariants worth preserving when extending:

- The simulator only ever signs with `Signature::test_signature()`, and only
  because `FyndClient::execute_swap` requires a `SignedSwap` shape. The
  server's dry-run path discards it. If a future encoding starts validating
  the outer signature on the dry-run path, this tool has to either gain
  access to a real key or grow a sign+simulate split — today it's stateless.
- `AuditRow` fields use `Option<_>` so the CSV row always has the same column
  count regardless of status. Don't drop the `Option` — preserves the
  "every sampled trade appears in the CSV" invariant.
- Gas price is a single snapshot applied uniformly. Don't make it per-trade
  without also computing per-trade actuals at per-trade timestamps; the
  relative comparison is only clean under a shared price.

