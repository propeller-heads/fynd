# APEX Batching-Surplus Analysis — Plan

**Requester:** Alan (Slack, #batching / the-endgame). **Deadline:** numbers by Friday 2026-08-07.
**Question:** how much surplus does APEX batch-clearing deliver over the status quo, for swaps that
already settle on-chain through solvers/venues — first on **Ethereum** (Base later, the original
Coinbase/APEX-in-sequencer framing).

## Decisions log (all confirmed by Matouš)

> **PIVOT 2026-07-31 (late morning):** target chain is **BASE** — Base-only venues, orders,
> protocol availability, everything. In-block budget ≈ **1 s** (Base blocks are 2 s). The
> Ethereum-first rows below are superseded where they conflict.
>
> **PHASE 0 INSERTED 2026-07-31 (afternoon): CoW pre-analysis gates the APEX work.** Before any
> APEX simulation, measure the coincidence-of-wants ceiling of real Base blocks from Allium data
> alone (CoW density = batching's only structural edge over per-order Fynd). Methodology + three
> worked examples + scan plan: https://claude.ai/code/artifact/533156b1-16fe-4454-a5db-aae1056a04db
> Key methodology facts (from real-block validation): rows MUST be netted per-tx into intents
> (multi-hop rows fabricate rings; same-tx opposites are already-realized CoW — only cross-tx
> matches count). Day-0 (24 h, 105k intents, $54.8M): pairwise cap 0.43% of volume ($237k/day),
> multilateral cap 2.0% ($1.09M/day), only 3.5% of blocks have any pairwise match → window sweep
> (1/5/15/30/150 blocks) is first-class. Surplus measured empirically from both sides' executed
> prices (examples: 2.9 bps WETH/USDC, 26 bps msUSD, 29% dislocated VANA). Tooling: Python +
> Polars + Allium API, ~30-45 min for a week, ~2-4 EU. Awaiting greenlight for the 7-day scan;
> APEX scaffold work stands and resumes after.

| # | Decision |
|---|---|
| Chain | **Base** (pivot — was Ethereum-first). Base RPC must support `debug_traceTransaction`; hindsight's Base address book + OP-stack receipt handling already exist. Base app/Coinbase + Relay are the venue flow that matters. |
| Base protocol sets (resolved 2026-07-31 via tycho PR #1260) | Tycho on Base serves `uniswap_v2, uniswap_v3, uniswap_v4, pancakeswap_v3, aerodrome_v1` (native, added in PR #1260), `aerodrome_slipstreams, lunarbase` — **all native, all serializable, all registered in fynd-core's protocol_registry** (tycho-simulation ≥ 0.345.1 pinned). **Parity config** = turbine ∩ Base = `uniswap_v2, uniswap_v3, uniswap_v4, pancakeswap_v3`. **Full-native config** adds `aerodrome_v1 + aerodrome_slipstreams + lunarbase` — so parity-vs-full on Base directly measures what Aerodrome (Base's biggest DEX) + lunarbase add: a headline result candidate, and the former "aerodrome blindspot" weak point is void. No VM protocols on Base at all, so the VM-serialization limitation is moot on this chain. |
| Pool representation | **Only** `Pool::Apex(Arc<dyn ApexPool>)` wrapping tycho `ProtocolSim` (`TychoApexPool`). No native apex UniswapV2/V3 models, no conversion code. |
| Protocol set | Parity with latest turbine: uniswap_v2, uniswap_v3, sushiswap_v2, pancakeswap_v2, pancakeswap_v3, uniswap_v4 (staging/prod set; fluid/ekubo are dev-only). All consumed via the tycho-simulation stream. Second config: full native-simulated coverage ("full-native"). |
| Coverage asymmetry | Parity-vs-full is a real sequencer tradeoff — keep both configs, be **verbose** in reporting: per-run coverage stats (orders/pools excluded and why, share of settled volume visible). |
| Trade universe | Solver/venue flow exactly as hindsight decodes it. That IS Alan's "transactions that happened onchain" for this analysis. |
| Order limits | **Real minAmountOut first**: extract from calldata per solver (CoW exact from settle calldata; 1inch v6 `minReturnAmount`; KyberSwap; ParaSwap — new `SolverKnowledge::min_amount_out` methods). Fallback synthetic limit: **executed_out × (1 − 100 bps)** (see slippage study below). Record `limit_source` per order. 50/200 bps sensitivity band held in reserve, not in core matrix. |
| Batch position | **Top-of-block** (state N−1, clean) AND **biased bottom** (state N; bias against apex documented — pools already moved by the batched trades themselves). Original-positions variant deferred until historical Tycho. |
| Time budgets | Two: **in-block ≈ 1 s** (Base 2 s blocks; final value after shadow runs) and exploratory 20 s. **Shadow runs first** to measure apex speed at Base scale; pool-count control (min_tvl, top-K pools per pair) is near-certain to be needed at a 1 s deadline. |
| VM protocols | Excluded (cannot serialize for snapshot replay). Sampled live in-process runs are a possible later step, not now. |
| Decimal scaling | **Option 2** — keep apex's 18-dec contract (it is baked into apex core: `truncate_to_precision`, `remove_extra_precision`, `validate_result`, all key off `Token.decimals`), but harden our adapter copy: typed `Scaled18(U256)` newtype domain, declines-with-counted-reason instead of panics (>18-dec tokens, scaling overflow), drop native-pool scaling paths entirely, property tests incl. direct-vs-adapter `ProtocolSim` agreement. Raising native-decimal apex core (Option 5) with apex owners is a separate roadmap track. |
| Code location | fynd repo, new tool (extend hindsight), new branch `mp/feat/hindsight-apex-batch` from origin/main. `apex-solver` as git dependency; turbine adapter (~300 lines: `TychoApexPool`, scaling utils, initial-price conversion) copied, not depended on. |
| Storage | Local JSONL + zstd-JSON market recordings. Binary codec (bincode/postcard behind same read/write fns) only if measured slow. |
| Initial prices | From Fynd's derived per-token price map at N−1 (what hindsight's `snapshot_prices` reads) → 18-dec U256. Not from per-pair Fynd quotes (slower, circular). Apex needs per-token absolute prices as tatonnement start; pools enter as ProtocolSim states. |
| Ground truth | Allium stays for `verify` (block_number, exact string amounts, fees, two-sided USD). Dune added later as scheduled coverage-discovery query only. Never union the two. |
| Revert probability | Out of scope (Alan handles separately). |

## Architecture: capture once, replay many

**Phase 1 — capture (extend hindsight monitor).** After each block's existing fynd re-solve,
serialize a *block batch snapshot*: decoded trades + fynd counterfactual quotes (already computed) +
extracted/synthetic minAmountOut per trade + fynd token-price map at N−1. Market state comes from a
parallel `record-market`-style recording (raw tycho `Update` messages: first = full snapshot, then
per-block deltas; replaying 0..k rebuilds state at block k; zstd-JSON via
`test-fixtures/src/recording.rs`). Start capture early — data accrues while phase 2 is built.

**Phase 2 — offline apex batch runner** (new binary / hindsight subcommand `apex-batch`). Reads
snapshots + recording, builds apex input per block: `MarketOrder` per decoded trade (limit per
policy), pools as `TychoApexPool` only, initial prices from fynd price map. Runs the config matrix,
rayon across blocks, `--jobs` for local macOS. Records per-order clearings + exclusion counters.

**Phase 3 — analysis.** Per-trade surplus vs fynd counterfactual (Alan's baseline), per-block total
surplus in USD and % of volume, win rates, split by venue/solver/limit_source/sandwich flag.
Report in streamlit (fynd_apex_dashboard.py pattern) or hindsight-HTML style.

**Config matrix (8 runs/block):** {parity, full-native} × {top, biased-bottom} × {real-limits} × {2 budgets}.

**Pre-capture step: DONE 2026-07-31** — Base book extended with 10 verified `[solvers]` lines
(bebop Blend+JAM v2, odos v2 Base-specific + v3, sushiswap RedSnwapper, openocean v2, paraswap v5
Base-specific, native RFQ pools ×2). Two-source verified by the solver-addresses agent; Bebop turned
out LIVE on Base ($12.6M/7d — largest missing venue), contradicting Allium's project list. 1inch
LOP v4 needs no line (shares the AR v6 address). Remaining gap: **two more live 0x Settlers on Base**
(`0x6b6e87d2…` $1.5M/7d, `0x68a14203…` 17.7k trades/7d) — verify via the 0x registry on-chain read
(`0x00000000000004533Fe15556B1E086BB1A72cEae`, `ownerOf(2)`) once a Base RPC is configured, then add.
Ethereum appendix lines (bebop ×4, odos, sushi, tokenlon ×3, airswap, native ×2, bitget, 0x settler
`0x7f54f056…` $120M/7d!) kept in the agent report for the later Ethereum pass.

## Prior art (file refs)

- Hindsight monitor two-state loop: `tools/hindsight/src/resolve/monitor.rs` (`StepAdapter`, `snapshot_prices` at :692).
- Recording: `tools/record-market/src/recorder.rs`, `test-fixtures/src/recording.rs` (VM states silently skipped — :62).
- Turbine apex adapter: `~/Projects/propeller-heads/turbine/src/clearing_algorithm/apex/solver.rs`
  (`TychoApexPool` :957, `create_filtered_pools` :461, `process_orderbooks` :643),
  `price_solver.rs` (mid-price graph — we replace with fynd prices), `utils.rs` (scaling, to be hardened).
- Apex core decimal contract: `apex-solver/src/token.rs`, `src/algorithm/mod.rs:87-100` (truncate),
  `src/utils.rs:15` (`remove_extra_precision`), `validate_result` at `algorithm/mod.rs:563+`.
- Single-order benchmark prior art: `turbine/tools/fynd_apex_benchmark/` (Fynd HTTP → mid-prices →
  apex `clear_batch` at frozen block, rayon, CSV, snapshots via `build_input_data`).
- Apex knobs: `deadline_ms` (best-so-far at checkpoint), `max_workers`, `SolverRunConfig` multi-config,
  `ApexInputData` serialization, `SnapshotRecorder`.
- Known apex bug to guard in runner: `apex-solver/panic-validate-result.md` (validate_result unwrap
  panic on pool-cleared token missing from clearing_prices — catch_unwind around solve like the benchmark does).

## Weak points (ranked, accepted)

1. Apex runtime at Ethereum scale unknown → shadow runs, pool-count control.
2. VM states can't serialize → full coverage = native-simulated only; sampled live runs later.
3. minAmountOut extraction uneven → limit_source recorded; synthetic 100 bps fallback; later
   per-venue calibration from our own extracted-limit distribution.
4. Parity of fynd baseline vs apex config → fynd counterfactual solved with matching protocol set
   (decision at implementation: run monitor with parity set; full-native baseline asymmetry documented).
5. Token price coverage gaps drop orders → counted, reported, never silent.
6. Biased-bottom double-impact → labeled.
7. Adapter panics → declines (Option 2).
8. Schedule: capture must start early in the week.
9. **Base pacing (new with pivot):** capture must keep up with 2 s blocks — per block: decode
   (receipts + traces) + fynd counterfactual solves must fit, or the monitor lags and rebuilds.
   Trace RPC latency on Base and fynd solve time per trade are the two levers; hindsight's
   `--max-lag-blocks` default ≈ 600 blocks on Base. Unproven: monitor has not been run on Base yet.
10. **1 s apex budget is aggressive** — apex `deadline_ms` returns best-so-far at checkpoints, so
   runs always produce *something*, but quality-at-1s vs quality-at-20s becomes a headline result
   of the shadow runs rather than a tuning detail.

## Agent findings (2026-07-31)

- **Dune vs Allium** (opus agent, 1.12 credits): Allium keeps verify role; Dune for coverage
  discovery; ~9 uncovered solvers with volume; venue tier is hindsight-exclusive ("moat");
  9 open Allium questions → being resolved by `allium-questions` agent (running).
- **Slippage tolerance** (opus agent, 1.37 credits, n=26,999, 2026-07-28): medians — 1inch v6 swap
  83 bps, unoswap 200 bps, Kyber 151 bps; discrete presets at 100/200 bps dominate; pooled p50
  ≈ 120 bps (inferred). → synthetic fallback limit = 100 bps below executed output (too-loose
  inflates apex surplus = dangerous direction; too-tight only conservative).
- Dune query IDs: 8173494, 8173523 (comparison); 8173714, 8173722, 8173728 (slippage).
- **Allium 9 questions** (opus agent, ~1.4 Explorer Units, all VERIFIED live):
  1. `transaction_fees_usd` is **per-transaction, duplicated on every trade row** (5,524/5,524
     multi-row txs had one distinct value; per-row summing over-counts gas 12.3%). `fee_details` is
     the raw tx receipt, no per-trade component. → **use hindsight's trace-based gas isolation**,
     never this column, for per-trade gas; chain totals need `max() GROUP BY transaction_hash`.
  2. For USD valuation prefer `usd_amount` (NULL only 0.4%) over the sold/bought legs (15–19% NULL;
     CoW much cleaner at ~1.5%; worst: zeroex/okx long-tail flow ~25–30%).
  3. CoW = one row per `Trade` event (per order); multi-order settlements exist but rare (3.8%).
  4. `unique_id` is a safe primary key incl. trace-indexed rows; one tx can carry rows from
     MULTIPLE projects (nested routing) — verify matching must not assume one project per tx.
  5. Live project list: docs stale. `paraswap_v6_2` present (project name stays `paraswap`, no
     `velora`); bebop_rfq/jam, odos_v2, native_rfq, airswap present; **bitget absent**;
     okx only ar_v1. **odos_v2 indexer stale since 2026-07-28 on both chains** — missing odos =
     Allium gap, not absent activity.
  6. `integrator`/`integrator_tag` exist on both chains but are ≈LiFi-only (everything else empty).
  7. `ethereum.dex.orderflow` EXISTS (docs URL is hyphenated `order-flow`) — tx-level frontend
     attribution view, 91 frontends, ~40% unattributed → supplementary cross-check for hindsight's
     venue tier, not a key.
  8. Freshness: ethereum ~14 min, base ~55 min behind — fine for offline replay.
  9. Base: 0x Settler dominant (4.5× ethereum volume); missing bebop/uniswap_x/tokenlon; adds
     sushiswap_aggregator_rp4.

## Environment / access

- Dune MCP: working (community plan, ~2.5 credits used total).
- Allium MCP: installed user-scope (`https://mcp.allium.so`, X-API-KEY header, **throwaway key —
  rotate after this analysis**), verified live 2026-07-31 after /reload-plugins.
- ALLIUM env keys for hindsight `verify`: not yet exported/confirmed in shell.

## Scaffold status (2026-07-31, worktree `apex-batch`, branch `worktree-apex-batch`)

**DONE**: `tools/apex-batch` crate (lib + bin; `run`/`shadow` subcommands; snapshot mirror;
hardened `Scaled18` scaling — implemented, not stubbed; `TychoApexPool`/runner/analysis stubs),
hindsight capture module + `--capture-dir` wiring, `min_amount_out` plumbing end-to-end (trait
takes `amount_in`, mirroring `embedded_quote` — ParaSwap needs it), CoW limit extraction
implemented, Base book +9 solver lines. `cargo check --workspace --all-features` clean; hindsight
254/254 tests; apex-batch `--ignored` tests: 10 pass, 2 fail on their own `todo!()` bodies
(`aggregate`/`timings`) by design.

Decisions taken on the scaffold's API findings: apex has no `deadline_ms` — `ApexConfig.deadline`
is `Option<Instant>`, so `solve_block` computes `Instant::now() + budget` per block per config;
`Pool::Apex` is `(PoolMetadata, Arc<dyn ApexPool>)`; no multi-config runner in apex — we call
`run_apex_with_config` per matrix cell; `BlockMarketState` stays local (no fynd-core dep);
snapshot mirror is hand-maintained with a round-trip test (report/record.rs precedent);
`apex_solver::types::Address::from(&str)` silently zero-fills on parse failure — adapter goes
through raw bytes instead.

## Implementation queue (Friday-ordered)

1. Three `min_amount_out` extractors (currently return `None` → synthetic fallback, decode/monitor
   safe): kyberswap `SwapDescriptionV2.minReturnAmount`, 0x Settler terminal-action floor,
   paraswap v6 triple middle word.
2. `capture::limit_for` — the 100 bps synthetic policy + `LimitSource` recording.
3. `TychoApexPool` method bodies (descale-before-sim, price inversion — mirror turbine
   solver.rs:1114), `initial_prices`, `build_orders`.
4. `market_state_at_block` — replay recording Updates 0..=k.
5. `solve_block`/`run_matrix` — catch_unwind (apex validate_result panic), per-block deadline,
   exclusion counters.
6. `analysis::aggregate`/`timings` (2 red tests already assert the requirements).
7. CoW caveat: buy-order flags word not consulted in `order_min_amount_out` — measure the
   sell/buy split before quoting numbers.
8. Shadow runs on Base (needs Base RPC with debug_traceTransaction + tycho Base creds) → fix
   1 s budget feasibility, pool-count controls; verify the two unconfirmed Base 0x Settlers via
   registry `ownerOf(2)` once RPC is up.
9. Capture days of Base blocks → matrix runs → analysis → Friday numbers.
