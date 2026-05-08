# fynd-gas-audit

One-shot tool that quotes real aggregator trades through a local Fynd instance,
simulates each on mainnet via `eth_estimateGas` with storage + balance overrides,
and reports — in ETH — how far Fynd's quote-time `gas_estimate` drifts from the
actual gas a transaction would consume.

Branch: `tl/fynd-gas-audit`. Spec: `docs/superpowers/specs/2026-04-23-fynd-gas-estimation-audit-design.md` (local-only). Plan: `docs/superpowers/plans/2026-04-23-fynd-gas-audit.md` (local-only).

---

## Why this audit exists

Fynd ranks route candidates by `amount_out_net_gas = amount_out − gas_cost`. If
`gas_estimate` is biased, that ranking is biased: a systematically-low
`gas_estimate` makes every route look cheaper than it is, which flatters
routes in a way that doesn't reflect execution economics. This tool quantifies
the bias.

---

## Running it

1. Start Fynd in one terminal (ithaca RPC is recommended — LlamaRPC gets
   rate-limited by the simulator's slot-detection probes):
   ```bash
   TYCHO_API_KEY=supersecrettoken \
   RPC_URL=https://reth-ethereum.ithaca.xyz/rpc \
     cargo run --release -p fynd -- serve
   ```
   Wait until `/v1/health` reports `healthy=true, derived_data_ready=true`.
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

---

## Findings from the 2026-04-23 run

At the time of the audit, mainnet gas price was unusually low (0.44 gwei).
Absolute ETH numbers scale linearly with gas price — the **relative** figures
are the ones that matter.

**Sample yield (100 trades attempted):**

| outcome | count |
|---|---|
| success (quote + simulation) | 46 |
| Fynd returned no route | 44 |
| simulation reverted | 10 |

**Gas accuracy on the 46 successful trades:**

| metric | value |
|---|---|
| Trades where Fynd **under**-estimated | **45 / 46** |
| Trades where Fynd over-estimated | 1 / 46 |
| Mean \|error\| / actual cost | **36.78%** |
| Mean signed error (ETH) | -0.000044 |
| Sum of signed error (46 trades, ETH) | -0.002043 |

**Headline:** Fynd is systematically low by ~37% on gas. Essentially no
over-estimates in the sample.

**Worst offenders** (from the run's `report.md`):

- `USDT → WBTC` and reverse: Fynd quotes **19,665 gas**; actual is 188k–235k.
  That's a **10–12x under-estimate**. Both directions and multiple amounts hit
  the same wrong value. Smells like a hard-coded or stale entry in the derived
  gas table for this token pair.
- `USDC → 5086bf…` (small-cap): 175,098 → 412,734 (2.4×).
- `WETH → 68749665…` (Pandora / ERC-404): 283,665 → 455,765 (1.6×).

**Pattern:** most of Fynd's estimates cluster on round numbers (132,000,
151,665, 396,000, 283,665) that look like per-protocol static budgets rather
than per-swap dynamic calculations. Real executed gas is almost always higher
because of approvals, storage writes, and router bookkeeping that the static
budget doesn't model.

---

## What the 37% means for route selection

- **Portfolio bias:** Fynd's `amount_out_net_gas` is consistently
  over-estimated by roughly 37% of the gas cost. At 0.44 gwei that's
  rounding-error dollars; at 20 gwei it's real money (~$4/trade on the mean,
  scaled from the headline -0.000044 ETH).
- **Per-trade ranking:** a systematic bias doesn't by itself flip the winner,
  but it narrows margins. Higher-liquidity multi-hop routes that cost more gas
  lose ground to lower-liquidity single-hop routes whenever the gas cost is
  the tie-breaker. So the bias is more likely to matter when top-2 routes
  have similar `amount_out`.

Both claims can be checked with the CSV. `error_wei` is signed; group by
pair to see where the ranking-flip risk concentrates.

---

## Known limitations of the current audit

1. **Fynd's route coverage limits the sample.** With the default
   `worker_pools.toml` (one Bellman-Ford 2-hop pool), ~44% of aggregator
   trades return `NoRouteFound`. That's realism — those are trades real users
   brought to aggregators, Fynd just can't serve them — but it biases the 46
   surviving trades toward common-token pairs.
2. **Non-standard ERC-20 tokens break slot detection.** The simulator's
   balance/allowance slot probe (brute-force slots 0..=20) misses tokens that
   use proxy patterns or non-Solidity-default storage layouts (10 trades
   reverted for this reason on the 100-trade run). These are biased toward
   small-caps.
3. **Gas price is a single snapshot.** We multiply every trade by
   `eth_gasPrice()` at the start of the run. Mainnet gas varies tick-to-tick,
   but for comparing estimate-vs-actual *on the same trade* this doesn't
   matter — the price cancels out of relative error.
4. **No signed transactions.** The simulator uses `provider.estimate_gas()`
   on the quote's calldata directly. This is correct because
   `quote.transaction().data()` is complete (the Tycho router does its
   authorization inside the calldata, not via an outer EIP-1559 signature).
   Verified in `clients/rust/src/client.rs::fynd_swap_payload` — it just
   copies the bytes through. If encoding ever starts injecting an outer
   signature, this assumption breaks and everything reverts.
5. **`eth_estimateGas` is itself an estimate.** Nodes binary-search for a
   gas limit that works; results are typically within hundreds of gas of
   actual execution, never thousands. Plenty accurate for the errors we're
   measuring (tens of thousands of gas).
6. **Worker pool count.** The audit runs against whatever `worker_pools.toml`
   the user has enabled. Different pools = different routes = different
   `gas_estimate`. Rerunning with a 3-hop pool or multiple algorithms
   competing may change the numbers.

---

## Research next steps

The next person should triage findings in this order. Highest-leverage first.

### 1. Investigate the USDT↔WBTC 19,665 gas constant

Fynd returns `gas_estimate=19665` for both directions of USDT↔WBTC at wildly
different amounts (61M, 2B, 3500 USDT). Actual gas on all of them is 180k–350k.
19,665 is the Ethereum intrinsic transaction cost (21,000) minus 1,335 —
specifically matches the `21000 - 1335 = 19665` formula some gas tables use
for "storage refund reduced baseline." That smells like a stale or
misconfigured entry.

Start here: `fynd-core/src/derived/computations/token_gas_price.rs`. The
computation walks Fynd's derived graph to produce per-token gas costs in
output-token terms. Check:
- Is there a path-discovery bug that's returning the intrinsic-tx baseline
  when no simulation path is available?
- Is there an edge case where `buy_gas_units` from
  `GraphManager::simulate` returns near-zero for the USDT/WBTC pair?
- Trace a single-hop swap from USDT→WBTC through a specific pool (Uniswap
  v3 0.3% or 1%) and compare `pool.gas_cost` to the aggregate
  `route.total_gas()`.

### 2. Characterise the static-budget pattern

Fynd's estimates repeatedly land on exact values: 120,000, 132,000, 151,665,
175,098, 264,000, 283,665, 396,000. These look like per-protocol fixed costs
summed up across hops.

Check `tycho-simulation` (the dependency Fynd pulls protocol simulators from):
- Each protocol (UniswapV2, UniswapV3, Balancer, Curve, etc.) exposes a
  `gas_cost()` — likely a hard-coded constant, not a per-swap computed value.
- If the constant is correct for one realistic swap shape but ignores the
  actual swap's storage writes, intermediate approvals, etc., the error pattern
  matches exactly what we see.

Proposed experiment: instrument the tool to log `route.swaps()[*].gas_estimate`
per hop for each successful trade, then compare to per-hop actual gas by
simulating hop-by-hop. That'll localise whether error is per-protocol-uniform
or concentrated in specific protocols.

### 3. Compute a per-token gas model

Instead of per-protocol constants, model per-token additive costs:
- WETH wrap/unwrap ~40k
- ERC-20 approval (first time, zero → max): 46k; subsequent: 26k
- ERC-20 transfer into user slot: ~21k (cold SSTORE on first-ever, 5k otherwise)
- Rebasing / fee-on-transfer tokens: variable, often 70k+
- Proxy tokens (USDC, USDT): +~2–4k per call

Can be derived empirically: run this audit with a larger sample, fit per-token
intercepts via least squares on `actual_gas − route_baseline`, feed back into
Fynd's gas table. Even a crude lookup for the top 20 tokens should halve the
error.

### 4. Check whether the bias affects route selection in practice

On the 46 successful trades, `amount_out_net_gas` is over-estimated by
~0.37 × gas_cost. In output-token terms that's tiny at 0.44 gwei. Simulate
what happens at 20 gwei and 50 gwei:
- Pick a trade where Fynd's chosen route was *close* to a rejected
  alternative.
- Recompute both routes' `amount_out_net_gas` using actual gas instead of
  estimated.
- Does the winner flip? How often?

`fynd-core/src/worker_pool_router/mod.rs` lines 143–354 have the ranking
code. `price_guard/guard.rs` also filters on net-gas; worth verifying
neither is sensitive to the bias direction (low estimate → more routes
pass the guard).

### 5. Cover more of the long tail

44/100 trades in the canonical dataset got `NoRouteFound`. That's a separate
finding (not about gas accuracy, but about route coverage). Options:
- Add a second worker pool in `worker_pools.toml` — 3-hop Bellman-Ford, or
  Most-Liquid — and rerun. Sample shifts, yield should improve.
- Filter the dataset to pairs the Fynd instance can route before sampling —
  this gives cleaner accuracy numbers but loses the "real aggregator traffic"
  property.
- The current tool treats `NoRouteFound` as a row in the CSV with
  `status=no_quote`, so no code changes needed to slice the data either way.

### 6. Sanity-check the gas-price snapshot

The audit reads `eth_gasPrice()` once and uses it everywhere. At 0.44 gwei
the absolute ETH numbers look small enough to be confusing. Two things to
verify:
- Is `eth_gasPrice` on ithaca returning a base fee, a priority fee, or the
  sum? Most nodes return base + tip, but ithaca's value is suspiciously low.
- Cross-check with `eth_feeHistory` and pick e.g. the p50 base fee over
  the last 100 blocks to get a stable number.

Fixing this only changes the ETH magnitude, not the 37% relative finding.

---

## Architecture pointers

| module | file | purpose |
|---|---|---|
| types | `src/types.rs` | `AuditTrade`, `RowStatus`, `AuditRow`, `Artifacts` |
| sampler | `src/sampler.rs` | Parses the 10k aggregator dataset, stratified cap-per-pair sampling with deterministic seed |
| quoter | `src/quoter.rs` | `FyndClient` wrapper; one `POST /v1/quote` per trade |
| simulator | `src/simulator.rs` | Brute-force ERC-20 slot detection (copied from `fynd-swap-cli/src/erc20.rs`), balance + ETH overrides, direct `provider.estimate_gas()` |
| cost | `src/cost.rs` | Signed `i128` arithmetic: estimate − actual, times gas price, to wei and ETH |
| report | `src/report.rs` | CSV writer + markdown generator with aggregate table, worst-10, high-exclusion finding |
| main | `src/main.rs` | CLI parsing (clap), dataset download, orchestration loop, ETH-price-via-Fynd for the report header |

Invariants worth preserving when extending:

- The simulator never signs. If this ever becomes necessary (e.g. to support
  a future encoding that needs a user signature), the architecture has to
  split into a quote phase and a sign+simulate phase — currently everything
  is one stateless `provider.estimate_gas`.
- `AuditRow` fields use `Option<_>` so the CSV row always has the same column
  count regardless of status. Don't drop the `Option` — preserves the
  "every sampled trade appears in the CSV" invariant.
- Gas price is a single snapshot applied uniformly. Don't make it per-trade
  without also computing per-trade actuals at per-trade timestamps; the
  relative comparison is only clean under a shared price.

---

## How to redo the 2026-04-23 run exactly

```bash
# terminal 1
TYCHO_API_KEY=supersecrettoken \
RPC_URL=https://reth-ethereum.ithaca.xyz/rpc \
  cargo run --release -p fynd -- serve

# terminal 2, after Fynd is healthy
RPC_URL=https://reth-ethereum.ithaca.xyz/rpc \
  cargo run --release -p fynd-gas-audit -- \
    --n 100 --max-per-pair 20 --seed 42
```

Output will be at `tools/fynd-gas-audit/out/`. Seed 42 reproduces the same
100-trade sample; gas price will differ (it's taken live); simulations will
differ because mainnet state has moved.
