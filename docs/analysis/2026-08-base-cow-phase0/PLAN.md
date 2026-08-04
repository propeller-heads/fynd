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

## PHASE 1 REPLAN (2026-08-04): live APEX monitor + multi-block CONFIRMED IN SCOPE

Pivot from capture-once-replay-many to **live-first**: extend the hindsight monitor with an
in-process APEX stage (TychoApexPool over the monitor's live ProtocolSim states — no
serialization, VM-safe, perfect state parity with the fynd baseline). Confirmed by user:
**multi-block batching is required scope**, not an extension.

Build order:
1. **Shadow run** — apex solve-time on real block batches at live Base state; sets pool-count
   controls for the 1 s budget.
2. **Live single-block stage** — batch = block N's decoded trades (skip <2), solved at N−1,
   ~1 s deadline; JSONL `apex` block per trade; Prometheus: apex_vs_fynd_bps,
   internalization_share (1 − pool-cleared/order volume = route-mediated REALIZED),
   apex_solve_ms, fill/skip/panic counters.
3. **Capture on from day one** — block-batch snapshots (limit_for + extractors to finish) +
   tycho Update recording (tee from the monitor's own stream preferred; record-market
   sidecar = acceptable approximation). Base recordings are complete (no vm:* on Base);
   VM-skip only matters if this moves to Ethereum.
4. **Multi-block**: two routes, both wanted —
   a. offline replay sweeps over recordings (windows 1/5/15/30/150, any budget incl. 20 s,
      config A/B on identical blocks) via the apex-batch crate;
   b. live window mode (--batch-window-blocks N): accumulate decoded trades N blocks, solve
      at window-end state with up to ~N×2s−overhead budget.
   Multi-block semantics: apex clears at window-END state; per-order baseline stays fynd's
   quote AT TRADE TIME (the real UX alternative); price drift over the window shows up as
   fill-rate loss against real minAmountOut limits — that IS the cost of waiting, report it,
   don't hide it.
5. Headline metrics vs Phase 0 ceilings: apex_vs_fynd bps/order (vs 0.02 measured + ≤0.12
   route ceiling) and internalization share (vs 1.7% cap @1 block, 23% @1 min).

Open (user): extend-monitor-vs-separate-binary (rec: extend), skip-<2-trades rule,
headline-metric pair confirmation, deploy path (local → staging k8s beside hindsight).

## PHASE 1 v2 — HARDENED after grill round 1 (2026-08-04, supersedes the v1 replan above)

Grill artifacts: .claude/plans/grill-phase1/ (critic_raw.md, grilling_log.md — 22 findings,
3 critical, 0% crit/high deferral). Root cause of all criticals: APEX API facts inherited from
turbine instead of verified against apex-solver. VERIFY EVERY APEX CLAIM AGAINST apex-solver.

Corrected architecture:
1. **Orders are LimitOrders, not MarketOrders** (MarketOrders are supply; limit-order map is what
   forms solve clusters — apex algorithm/mod.rs:241). Limit = minAmountOut policy as Fraction.
2. **APEX solves BEFORE `advance()`**, between fynd top-solves and the step, on a `clone_box()`d
   filtered pool subset at N−1 (states are Box<dyn ProtocolSim>, replaced wholesale on advance —
   no borrow survives; clone also yields the Arc the adapter needs and frees the read lock).
   Clone cost = explicit shadow-run line item.
3. **Budget parity**: primary cell apex_deadline = n_orders × fynd timeout_ms (equal compute);
   secondary fixed 1 s (sequencer realism); 20 s offline only, labeled quality-ceiling.
4. **Gas basis: gross-vs-gross** (hindsight convention); batch gas out of scope, stated.
5. **internalization_share redefined**: per-token NET pool exposure (not per-hop sums), USD at
   N−1 map, bounded [0,1], unit-tested on a multi-hop single-order batch (must be ≈0).
6. **Recordings lose uniswap_v4 on Base** (UniswapV4State is non-serializable — the "Base
   recordings complete" claim was false). Offline sweeps = config A/B only (shared handicap);
   live-vs-replay parity not claimed on v4 blocks; upstream serialization ask filed; replay
   skips stateless components, counted.
7. **Recording format v2 before capture starts**: hourly append-only segments + periodic
   checkpoints; replay = checkpoint + ≤1h deltas; crash loses ≤1 segment.
8. Order id = {tx_hash}:{ordinal}; captured_trades join fixed to composite key.
9. **0x Settler floor extractor lands BEFORE the live stage** (dominant Base flow); headline
   gated on extracted-limit subset; synthetic slice separate with 50/100/200 bps sensitivity.
10. APEX solve in spawn_blocking, max_workers=1 live, AssertUnwindSafe + token-set closure
    precheck (decline batch, don't panic); SolveMetrics after a caught panic discarded.
11. **Live multi-block mode CUT** — multi-block is offline-replay-only.
12. Per-order status {filled, unfilled_at_limit, cluster_cut, excluded_*} via input-vs-clearing
    reconciliation (deadline drops whole clusters silently otherwise).
13. ApexConfig fully pinned (two_hops labeled axis, starting_price never engages — unpriced
    orders pre-excluded), price-map coverage + derived-data freshness stamped per block.
14. is_partial check on the Base stream is a hard pre-capture gate.
15. Path dep → git dep with [patch] for local dev before live stage merges into hindsight.
16. One shared input-builder: hindsight live stage calls apex-batch's lib (no duplicate path).

Corrected build order: 0) adapter property tests (direct-vs-adapter ProtocolSim agreement) →
0.5) ≥2-connected-trades/day pre-check from existing 10d data (go/no-go for live-stage value
claims) → 1) shadow run (solve time, clone cost, bytes/day, is_partial) → 2) 0x floor extractor →
3) live single-block stage → 4) capture (format v2) → 5) offline multi-block sweeps → 6) report.

STATUS: needs grill round 2 (verify LimitOrder construction/limit-price direction + Fraction
scaling, clone-cost claim, recording v2 replay math) before implementation.

### v2.1 additions (2026-08-04, user-confirmed)
- **APEX brackets both states, mirroring fynd**: solve batch at N−1 (headline) AND at N (biased
  bottom, same bias semantics as fynd's `back`). 2× apex budget per block — shadow-run line item.
- **Window set: {1, 6, 30, 150} blocks = {2s, 12s, 1min, 5min}.** Live = w1 only (v4-complete
  anchor). w ∈ {6,30,150} offline over recordings; w1 ALSO offline so all offline cells share the
  v4 handicap (and live-w1 vs offline-w1 measures that handicap directly). Multi-block offline
  cells solved at both window-start and window-end states.
- v4 serialization nuance (verified in tycho-simulation 0.345.1 source): UniswapV4State is
  blanket-non-serializable because of `hook: Option<Box<dyn HookHandler>>`; hookless pools are
  plain CLMM — upstream fix is small (serialize when hook.is_none()). Frame the ask accordingly.

### v2.2 (2026-08-04, user decision): single 1 s live budget; equal-compute cell dropped
- Live apex budget = fixed 1 s per batch solve (production realism), both brackets. The
  equal-compute attribution cell is REMOVED from live (n×100ms can exceed block time and models
  nothing a sequencer does); compute-asymmetry vs the 100 ms/order fynd baseline is a stated
  report caveat, and the mechanism-isolation cell lives offline only, if ever needed.
- Pacing consequence: top+bottom brackets ≈ 2×1 s apex + 2×n×100 ms fynd per eligible block >
  2 s Base blocks on busy stretches → lag-aware degradation ladder: (1) drop apex BOTTOM solve
  under lag, (2) skip apex for the block; every skip counted (apex_skipped{reason}). Shadow run
  measures sustainable coverage at real pacing.

### v2.3 (2026-08-04, user decision): async APEX — solves leave the block loop's critical path
- The block loop only `clone_box()`es the filtered pool subset (at N−1 and at N) and spawns each
  1 s solve on a capped blocking-thread pool; results join JSONL/metrics asynchronously.
- Fynd's per-trade solves parallelize across trades (worker pool already supports concurrent
  quotes): n×100 ms → ~100–200 ms wall.
- Critical path per block ≪ 2 s; average APEX load ≈ 0.7 core-s per 2 s block
  (2×1 s × ~35% eligible blocks) — bursts queue instead of lagging.
- Guardrails: dedicated APEX thread cap (CPU contention must not silently degrade the
  timeout-bound Fynd baseline — add a fynd-solve-time drift metric vs APEX-off runs); the lag
  degradation ladder from v2.2 remains as backstop only.

## PHASE 1 v3 — HARDENED after grill round 2 (2026-08-04, supersedes conflicting v2/v2.x items)

Grill artifacts: grill-round2/ (21 findings: 4 critical, 9 high; all resolved, 0 deferred).
Root causes this round: (a) APEX's input contract (pricing + token closure) was under-specified;
(b) the v2.3 async pacing spec assumed capabilities that don't exist (hard deadline cap, fynd
per-trade parallelism, per-stage tokio thread caps); (c) the headline conflated batching value
with a routing-engine gap.

**Study design:**
1. APEX clears ≤1 pool per pair, never splits, ≤2 hops (pool_liquidity.rs takes max over a
   pair's pools) while the fynd baseline splits and multi-hops → apex_vs_fynd includes a
   routing-engine gap. New control cell: every order is ALSO solved through APEX as a
   single-order batch (same config/subset/budget). **Batching-isolated headline =
   apex(batch) vs apex(singles)**; apex_vs_fynd reported alongside, labeled engine-inclusive.
   [NEEDS USER/ALAN SIGN-OFF on the reframed headline before the report ships.]
2. Live budget: 1 s per batch solve with the deadline `Instant` computed AT SOLVE START inside
   the worker (never at enqueue — an already-expired absolute deadline returns a silently EMPTY
   result, not an error). `queue_wait_ms` + the solver's `deadline_fired` flag recorded per
   solve; bounded dispatch queue, overflow counted `apex_skipped{reason="queue_full"}`.
3. The 1 s deadline is a SEARCH budget, not a wall cap (checked only between clusters, in the
   price-search loop, and once post-search; the clearing phase — demand oracle, simplex,
   clear_amount, validation — is unbounded). Outer watchdog discards results older than 3×
   budget (`apex_overrun`); shadow run gates on measured TOTAL wall p99 < 2 s, else shrink the
   pool subset.
4. Pacing: APEX-stage-OWNED fixed worker pool (2 OS threads) fed by a bounded channel — NOT a
   tokio `spawn_blocking` cap (runtime-wide, shared with JSONL IO, caps nothing per-stage).
   apex-solver built WITHOUT the `multithread` feature live (`max_workers=1`; with it, every
   cluster builds a fresh rayon pool outside any cap). Fynd per-trade solves in
   `resolve_block_range` are SEQUENTIAL today (mod.rs:349) — parallelizing them is a named
   build item (join_all + mock-test update); until it lands the v2.2 lag ladder is the primary
   pacing mechanism. Load estimate corrected by the connectivity pre-check: ≥2-trade blocks
   ≈50%/day (not 35%), so ≈1 core-s average per 2 s block for both brackets, bursty.

**APEX input contract (adapter preconditions — panics become counted declines):**
5. Token closure includes EVERY pool token: with `two_hops=true`, apex prices
   `get_market_tokens()` = all tokens of all pools, and any unpriced one silently gets
   `starting_price` (mod.rs:299-311). Precondition: every token of every included pool priced
   from the N−1 map, else the POOL is dropped (`pool_unpriced` counter). Retracts v2 item 13's
   "starting_price never engages" claim.
6. Price scale pinned: per-batch normalization so min price ≥ 1e6 units, overflow headroom
   asserted; tokens whose price rounds to zero excluded with their orders/pools
   (`price_underflow`); zero-price hard precondition (zero divisors panic inside apex —
   clearing.rs:66). Price SOURCE pinned (user, 2026-08-04): fynd's derived token prices —
   `Solver::derived_data().token_prices()`, the same data the experimental `GET /v1/prices`
   endpoint serves — consumed as EXACT rationals (numerator/denominator vs the gas token),
   normalized to the apex U256 scale per batch; NOT the lossy f64 path
   `snapshot_prices` uses for USD reporting (monitor.rs:719).
7. Limit scaling contract:
   `limit_price = Fraction::new(lift18(min_amount_out, buy_dec), lift18(sell_amount, sell_dec))`
   — BOTH legs in 18-dec space (direction verified: limit = min-buy-per-sell,
   limit_order.rs:19-21). Property tests: mixed-decimal pairs both directions, exact rational
   equality; at-limit order (limit == clearing price) does not error.
8. Per-order uniqueness asserted at build — equal `(pair, limit_price, id)` orders silently
   collapse in the BTreeSet (pair.rs:37); reconciliation reads the PRE-map trade list.
9. `sell_amount > 0`, nonzero prices, nonzero limit denominators asserted pre-call with
   actionable messages; `catch_unwind` only as last resort (SolveMetrics discarded after).
   panic-validate-result.md is stale (that unwrap is gone); fresh upstream issue for the
   `tokens[&addr]` index-panic sites.
10. v4 pool identity: apex `Address` = `keccak256(component_id)[0..20]` for non-address
    component ids (v4's 32-byte ids truncated naively collide and silently overwrite pools);
    build-time address→component-id collision assertion.
11. Per-component isolation: the adapter partitions orders+pools into connected components and
    calls APEX once per component — one cluster `Err` (`ClearingUnderLimitPrice`,
    `PostTruncateImbalance`, …) aborts only its component (`component_error{kind}` counter;
    apex's `?` at mod.rs:258 aborts the whole call otherwise). Per-order status gains
    `component_errored`. Limits are NOT epsilon-fudged; the truncating-fill vs exact-validation
    rounding mismatch goes upstream.
12. Headline config pinned: `two_hops=ON` with the full-pool-pricing precondition. Pool subset
    filter: pools adjacent to order tokens ∪ pools linking two order-adjacent tokens (2-hop
    closure), TVL-capped at K, K tuned by the shadow run. `two_hops=OFF` = offline secondary
    cell only.

**Integration seam (named refactor, build step 3):**
13. `advance()` lives inside `resolve_block_range` and `SteppingSolver` exposes no state
    accessor — split `resolve_block_range` into tops/advance/backs phases driven from the
    monitor's concrete-`Solver` path (`market_data()`), keeping `SteppingSolver` mock tests on
    the composed function.
14. Clone cost model corrected: subset `clone_box` (copy 1) + `Arc::from(Box)` (copy 2, full
    memcpy of unsized values) = 2 copies per state per bracket, 4 per block. Shadow run
    measures both terms.

**Recording/replay v2 (redesigned):**
15. Replay = ONE forward fold per day per config (checkpoint+delta per block is O(n²): ~39 M
    update applications/day/pass, and conflicts with run_matrix's block-parallel contract —
    that contract is replaced). APEX solves fan out to rayon from cloned filtered subsets at
    eligible blocks; window-start states for w∈{6,30,150} from a ring buffer of retained
    subset clones (eligible blocks only, ≤150 deep).
16. Segments-only format ships first: append-only hourly zstd segments, incremental flush,
    per-block gas price in-stream (metadata's single start-of-run `gas_price_wei` retired).
    Checkpoints deferred to a crash-recovery follow-up (fold-once replay needs no random
    access).
17. `is_partial` policy: capture DROPS partial updates at ingestion (counted); fold key =
    confirmed updates only (partials share the block number with the confirmed update that
    follows; a partial-only state change must never linger in a fold). Shadow run verifies the
    Base stream's actual partial behavior and fynd-core feed equivalence.

**Coverage gates:**
18. 0x floor extractor coverage measured on a sample day at build step 2: <50% of eligible
    orders with extracted limits → escalate before the live stage (add kyber/paraswap or
    reframe synthetic-primary).
19. live-w1 vs offline-w1 relabeled: combined live/replay divergence upper bound (v4 + gas +
    partials + subset diffs), not "the v4 handicap". Optional isolation cell (live with v4
    dropped) if the bound is large.

Connectivity pre-check (build 0.5) RAN 2026-08-04: GO — ~14k connected blocks/day (32%),
~9.3k/day on fynd-solvable trades; 08-03 is a partial capture day. Script:
connectivity_precheck.py.

STATUS: focused grill round 3 on this v3 section before the live-stage build; the 0x Settler
extractor proceeds in parallel (independent of all round-2 findings).

## PHASE 1 v3.1 — after grill round 3 (2026-08-04; supersedes conflicting v3 items)

Grill artifacts: grill-round3/ (12 findings: 2 crit, 5 high; all resolved, none deferred).
Verdict: NO round 4 — remaining risk is empirical and the build order's own gates (step-0
property tests, shadow run) are the instruments for it. Component question settled by
measurement (`component_count.py`, 10d): 41.2% of eligible blocks single-component, 58.8%
split ≥2; 77.9% of orders in the hub-connected giant component.

A. **Price scale from the overflow bound (1e6 floor RETRACTED).** apex arithmetic WRAPS
   silently (Signed256/ruint); the objective squares value = amount₁₈×price, so
   |value| < 2^127.5 must hold, and `increase_precision` multiplies all prices ×10 up to
   `max_precision_increases` (default 10!). Pin P = max_precision_increases = 2; per batch,
   with notional cap N_usd, choose S maximal s.t. N_usd·1e18·S·10^P < 2^126; tokens whose
   scaled price < 1e3 units excluded (price_underflow). Upstream issue: silent wrapping
   overflow. Property test with a $1e-9 token.
B. **Price transform pinned (inversion + decimals).** tycho `Price` rationals are tokens-out
   per gas token INCLUDING decimals; apex needs the inverse orientation in 18-dec space:
   `apex_price(t) = round(S · 10^(dec_t − 18) · den_t/num_t)`. Property test mirroring
   fynd-core's ETH=2000-USDC fixture (ratio exact incl. the 1e12 decimals factor).
C. **Per-component isolation kept, honestly scoped.** Each component call gets ONLY its
   component's pools/tokens; the SAME partitioning applies to batch and singles cells (the
   headline is a within-partitioning comparison; per-component ≠ single-call bit-for-bit,
   stated). Giant-component aborts counted `batch_errored` per block; sample-loss bias
   reported; upstream ask (per-cluster error isolation) remains the real fix.
D. **Pacing re-costed.** Budget = 1 s per COMPONENT batch solve; singles use their own
   component subset, capped 250 ms; batch-eligibility = CONNECTED blocks (~32%/day measured,
   50% figure corrected); singles only for orders in batch-eligible blocks. All solves consume
   block-time-cloned inputs → queue delay affects metric latency only, never state parity.
   Gate: sustained throughput ≥ arrival rate, bounded queue depth, counted skips (not per-block
   wall p99). Worker pool sized by the shadow run against this full load model.
E. **Ring buffer DELETED.** Offline replay indexes all orders in advance; the single forward
   fold, at each chain block b, dispatches every cell anchored at b — window-start cells for
   [b+1, b+w] filter the current folded state by the KNOWN union of that window's orders;
   window-end cells likewise. Memory = one folded state + in-flight cell subsets.
F. **Watchdog contract stated honestly.** Overrunning solves occupy their worker to completion
   (no cancellation path) — visible via queue depth + apex_overrun; shadow run records
   search_ms and clearing_ms as SEPARATE series (clearing is O(pools), unguarded by the
   deadline); the K gate is set against the clearing tail. Upstream ask: deadline checks in
   the clearing phase.
G. **Headline pairing rule.** Batch-vs-singles computed over the INTERSECTION of orders whose
   both cells produced validated results (not deadline_fired/errored/skipped); exclusions
   counted by reason; all-batch-result sensitivity view alongside. Part of the sign-off
   package.
H. **Phase split without a shim.** The composed resolve_block_range is dropped; monitor calls
   tops/advance/backs phase functions directly; mock tests move to the phases + one
   monitor-sequencing test.
I. **Price-coverage gate** beside the extractor gate: shadow run measures pool_unpriced share
   of the subset; >20% dropped → escalate before the live stage.
J. Implementation sweep: runner.rs catch_unwind doc rewritten against the real panic sites
   (`tokens[&addr]` indexes, `get_order` expect); stale panic-validate-result.md reference
   retired.

K. **NATIVE PROTOCOLS ONLY for APEX (user decision, 2026-08-04, schedule-driven).** The APEX
   pool universe is restricted to natively-simulated protocols (uniswap_v2/v3/v4, aerodrome,
   …); vm:* pools are excluded from the subset filter. Rationale: time to ship + VM ~5×
   simulation latency inside the 1 s budget + native states are the serializable ones for
   capture. The singles control shares the restriction, so the batching-isolated headline is
   unaffected; apex_vs_fynd gains the caveat that fynd's baseline may route through vm:* pools
   APEX cannot see (engine-inclusive gap, stated in the report). Full-native config cell
   deferred, not cut.

USER SIGN-OFFS OUTSTANDING: (1) headline reframing — apex(batch) vs apex(singles) as the
batching-isolated number, apex_vs_fynd engine-inclusive alongside, with item G's pairing rule;
(2) escalation thresholds (extractor <50%, price coverage >20% dropped pools).

COMPARABILITY INVARIANT (user, 2026-08-04): every APEX cell is measured on the SAME states and
conventions as the Fynd cell it is compared against — same cloned N−1/N pool states (top =
fair counterfactual headline, back = biased-bottom floor including self-impact, identical bias
semantics both engines), same decoded orders and extracted limits, same gross-vs-gross gas
basis, same component partitioning across batch/singles cells. No APEX number is reported
against a Fynd number measured at a different state.

NEXT: build step 0 (scaling + adapter + prices + limits property tests), then shadow run.
