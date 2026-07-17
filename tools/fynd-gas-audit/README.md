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
   TYCHO_API_KEY=<your-token> \
   RPC_URL=https://reth-ethereum.ithaca.xyz/rpc \
      cargo run --release -p fynd -- serve -w tools/fynd-gas-audit/single_hops.toml
   ```
   Wait until `/v1/health` reports `healthy=true, derived_data_ready=true`.

   `single_hops.toml` is a worker config tailored to this audit: it pins
   `max_hops=1` on both `most_liquid` and `bellman_ford` pools so every
   sampled trade resolves to a single-hop swap. Use it when you want to
   isolate per-protocol gas accuracy without multi-hop overhead confounding the
   numbers. Drop the `-w` flag to fall back to Fynd's default
   the default pool config (`fynd.toml`), which allows multi-hop routing.
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

## Findings from the 2026-05-27 run (default routing, n=1000)

This run used Fynd's default pool config (multi-hop enabled), so the
sample includes both single-hop and sequential routes. Mainnet gas price at
the time was 0.10 gwei. Absolute ETH numbers scale linearly with gas price —
the **relative** figures are the ones that matter.

**Sample yield (1000 trades attempted):**

| outcome | count |
|---|---|
| success (quote + simulation) | 523 |
| Fynd returned no route | 399 |
| simulation reverted | 78 |

72 out of 78 simulation reverts are tokens whose balance/allowance storage slots the simulator's brute-force
probe (slots 0..=20) couldn't locate — proxy patterns or non-Solidity-default
storage layouts, biased toward small-caps. See limitation #2. The other 6 reverts 
just show "0x4e487b71" Panic().

**Gas accuracy on the 523 successful trades:**

| metric | value |
|---|---|
| Trades where Fynd **under**-estimated | **482 / 523** |
| Trades where Fynd over-estimated | 41 / 523 |
| Mean \|error\| / actual cost | **34.79%** |
| Median signed error (ETH) | -0.000009 |
| Mean signed error (ETH) | -0.000010 |
| P95 absolute error (ETH) | 0.000020 |
| Sum of signed error (523 trades, ETH) | -0.005169 |

**Headline:** Fynd is systematically low by ~35% on gas.

**By route shape:**

| shape | n | under | over | mean \|err\|/cost |
|---|---|---|---|---|
| sequential | 305 | 271 | 34 | 34.93% |
| single | 218 | 211 | 7 | 34.58% |

Single-hop and sequential routes have nearly identical mean error (~35%),
showing the bias is additive across hops rather than a routing artifact.

**By protocol — single-hop only:**

| protocol | n | mean \|error\| / cost | under | over |
|---|---|---|---|---|
| uniswap_v4 | 62 | **68.96%** | 62 | 0 |
| ekubo_v3 | 1 | 39.63% | 1 | 0 |
| uniswap_v2 | 51 | 29.60% | 51 | 0 |
| pancakeswap_v3 | 1 | 19.06% | 1 | 0 |
| fluid_v1 | 2 | 18.15% | 2 | 0 |
| uniswap_v3 | 96 | 17.24% | 94 | 2 |
| sushiswap_v2 | 5 | 1.00% | 0 | 5 |

Uniswap v4 is the dominant outlier at ~69% mean error. 

---

## Grouped swaps: uniswap_v4 → uniswap_v4

Consecutive uniswap_v4 hops are encoded as a single grouped PoolManager call.
The gas estimate for the group sums per-swap estimates and discounts skipped
intermediate token transfers.

| sequence | n | mean \|err\|/cost |
|---|---|---|
| uniswap_v4 (single) | 62 | 68.96% |
| uniswap_v4,uniswap_v4 | 74 | 60.63% |
| uniswap_v4,uniswap_v4,uniswap_v4 | 14 | 57.16% |

The error shrinks with each additional grouped leg because the batching
discount partially offsets the baseline v4 under-estimation. The grouped
estimates remain substantially under because the per-swap base estimates are
already too low.

---

## What the 35% means for route selection

- **Portfolio bias:** Fynd's `amount_out_net_gas` is consistently
  over-estimated by roughly 35% of the gas cost. At the 0.10 gwei this run
  saw, the per-trade loss is ~$0.02 on the mean (the -0.000010 ETH headline
  at ETH ≈ $2,080); at 20 gwei it scales to ~$4/trade.
- **Per-trade ranking:** a systematic bias doesn't by itself flip the winner,
  but it narrows margins. Higher-liquidity multi-hop routes that cost more gas
  lose ground to lower-liquidity single-hop routes whenever the gas cost is
  the tie-breaker. So the bias is more likely to matter when top-2 routes
  have similar `amount_out`.

---

## Known limitations of the current audit

1. **No-quote rate vs. per-protocol attribution trade-off.** With default
   routing (~40% no-quote), sequential routes mix protocols, making the
   single-protocol breakdown less clean. Use `-w tools/fynd-gas-audit/single_hops.toml`
   (`max_hops=1`) to isolate per-protocol accuracy — at the cost of ~61%
   no-quote and no sequential route coverage.
2. **Non-standard ERC-20 tokens break slot detection.** The simulator's
   balance/allowance slot probe (brute-force slots 0..=20) misses tokens that
   use proxy patterns or non-Solidity-default storage layouts (72 trades
   reverted for this reason on the 1000-trade run, ~8%). These are biased
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

