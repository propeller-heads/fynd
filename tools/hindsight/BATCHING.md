# APEX batching-validation experiment

Validates the idea of batching one Ethereum block's swaps through the APEX batch solver
(`apex-solver`, a path dependency at `../../../apex-solver`). Per block, the settled trades
hindsight decodes are re-solved two ways against the same top-of-block market state:

- **S1** — one order per solve (control: same solver and pools, no batching)
- **S2** — the whole block as one batch (treatment)

**S0** is the settled on-chain outcome. Unfilled and out-of-universe orders count at S0.
A partial fill executes fully for the user at the clearing price: the batcher acts as the
missing liquidity source, supplying the buy-token remainder and receiving the unsold sell
amount. Gas is out of scope. The full experiment design lives in
`apex-solver/batching-validation-plan.md`.

## Running

Requirements: a mainnet RPC with `debug_traceTransaction`, and a Tycho API key.

```bash
cargo build --release -p hindsight

RPC_URL=https://... \
TYCHO_API_KEY=... \
./target/release/hindsight monitor \
  --tycho-url tycho-beta.propellerheads.xyz \   # bare host — the client adds the scheme
  --min-tvl 30 \
  --max-blocks 40 \                              # omit to run until Ctrl-C / SIGINT
  --max-lag-blocks 10000 \                       # solves are slow; don't rebuild on lag
  --apex-batching-dir ./poc-results
```

Optional knobs: `--apex-s1-deadline-ms` (default 500, per order),
`--apex-s2-deadline-ms` (default 3000, per batch), and `--apex-max-iterations`
(lifts APEX's price-search iteration cap, default 1000; the deadlines still bound
wall-clock time). The default protocol set is
`native_onchain`; pass `--protocols all_onchain` to also stream VM-simulated protocols
(Curve, Balancer, …) at the cost of a much heavier feed and slower solves (wrapped pools
sit in APEX's hot loop — raise the deadlines, e.g. 800/6000).

Every block runs **three limit-price variants**: `permissive` (limit ≈ 0 — every order may
fill; measures raw price movement), `anchored` (limit = the actual settled execution
price — APEX must beat reality to fill; fill rate is the headline), and `user_limit`
(limit = the user's signed minimum buy amount, recovered from router calldata by the
decoder's word scan; orders whose limit could not be recovered fall back to the anchored
limit — records carry `limit_source: calldata|settled_fallback`). Records carry a
`variant` field and the report renders one section per variant.

Pool coverage: V2/V3-family pools use APEX-native models; every other protocol wraps its
`ProtocolSim` (`TychoApexPool`). Native-ETH pools (Curve stETH/ETH, Uniswap V4) fold ETH
into WETH on the APEX side; multi-token pools (Curve tripools) expand into one pair view
per token combination sharing the same simulation.

Every solve (S1 and S2) runs Turbine's production **run-config panel**: seven APEX
configurations race in parallel threads under one shared deadline — varying initial-price
scaling (`price_factor`), two-hop routing, and the mixed Top(2)/Top(1) step strategies,
at 3000 search iterations — and the result clearing the most ETH-valued output wins.
The winning config is recorded per block (`s2_winning_config`) and per solve in the
results dumps. APEX itself is built with its `multithread` feature (parallel market-supply
queries, two workers per config).

The solver needs a few minutes to sync before the first block; the experiment then runs
once per block, at top-of-block state, alongside hindsight's own fynd re-solve.

## Outputs (in `--apex-batching-dir`)

| File | Contents |
|---|---|
| `apex-orders.jsonl` | One record per order per run (S1, S2) per variant: settled S0 amounts, APEX outcome, inclusion status (`cleared` / `partial` / `unfilled` / `out_of_universe`), batcher-absorbed amounts, ETH valuations at the block's derived prices |
| `apex-blocks.jsonl` | One record per block per variant: pool counts per conversion path, universe size, S1/S2 solve times, deadline flags, S2 per-pool AMM volumes |
| `inputs/apex_input_<block>.json` | Full `ApexInputData` dump per block — replay offline with `cargo run --release --example replay_batch --features dev -- <file>` in the apex-solver repo (wrapped Tycho pools deserialize as opaque `custom` entries there) |
| `results/apex_result_<block>_<variant>.json` | Full APEX solve output per block and variant: the S2 batch and every S1 solve — clearing prices, limit-order clearings, pool clearings with surplus/fee (18-dec scaled decimal strings) |

Appending is safe: rerunning with the same directory extends the JSONL files.

## Report

```bash
python3 tools/hindsight/scripts/apex_batching_report.py ./poc-results   # writes ./poc-results/report.html
```

Prints the headline numbers and writes a self-contained interactive HTML report, one
section per variant: stat tiles (S2−S1, S2−S0, fill rate, win rate, CoW potential and
realized netting, batcher inventory), per-block charts, per-order improvement
distribution, sortable per-block / per-token-volume / per-order tables, and the
accounting rules.

## Reading the results

- **S2 − S1 in bps of settled value is the primary metric** (batching effect, same solver
  and coverage on both sides).
- In the **permissive** variant every order may fill at any price, so a token whose real
  liquidity is outside the streamed protocol set can clear at a much worse price than
  reality (observed: stETH sized far beyond its only in-universe pool under
  `native_onchain`). Judge totals together with the per-order distribution and the
  outlier rows. The **anchored** variant excludes such fills by construction — its fill
  rate is the "can APEX beat reality" headline.
- `unfilled` includes both APEX cluster pruning and equilibria the price search did not
  reach within its iteration budget — APEX does not distinguish them outwardly.
