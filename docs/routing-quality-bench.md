# Routing Algorithm Quality Benchmark

Running record for the effort to build a reproducible algorithm-quality benchmark and to write new
routing algorithms that beat the current ones (MostLiquid, BellmanFord, and a future split algo) on
the 10k aggregator trade dataset.

## Goal

1. Set up a test harness to compare routing-algorithm output quality.
2. Use routing-design ideas (from research notes) to write new algorithms.
3. Beat the current algorithms on the 10k dataset; later validate on a fresh dataset.
4. Record learnings and performance along the way; stop when the new algos win.

## Key decisions

- **Offline snapshot replay** (not live two-server compare). Capture one live market snapshot to
  disk, then replay it in-process so every algorithm solves the *same* frozen state. Deterministic,
  fast to iterate, fair per-algorithm comparison, reusable for the future dataset.
- **Native-serializable protocols only** in the snapshot (Uniswap v2/v3/v4 etc.). VM-backed pools
  (Balancer/Curve) are excluded; their states are not reliably serializable.
- **New algorithms live in `fynd-core/src/algorithm/`** as real `Algorithm` impls, registered so
  they can also run in production worker pools.

## Feasibility findings (verified in code)

- `ProtocolSim` has `#[typetag::serde(tag="protocol", content="state")]` (tycho-common 0.315), and
  native states like `UniswapV2State` derive `Serialize/Deserialize`. So `Box<dyn ProtocolSim>`
  round-trips through JSON for native protocols. VM states would bloat/fail — excluded by design.
- All snapshot fields are serde-capable: `ProtocolComponent`, `Token`, `BlockGasPrice`/`GasPrice`,
  `BlockInfo`. `MarketState` itself had no serde; added `MarketSnapshot` + `to_snapshot`/
  `from_snapshot` (uses `Vec` not `HashMap` to avoid non-string map-key issues).
- Derived data can be computed once offline by driving `ComputationManager::handle_event` with a
  single full-topology `MarketUpdated` event (spot prices → token gas prices + pool depths).
- `Solver::market_data()` is public, so a live capture tool can snapshot the running market.
- `PetgraphStableDiGraphManager<()>` impls `EdgeWeightUpdaterWithDerived` (via `() :
  EdgeWeightFromSimAndDerived`), so the harness is generic over both ML (`DepthAndPrice` weights)
  and BF (`()` weights).

## Architecture

- `fynd-core/src/feed/market_data.rs`: `MarketSnapshot` + `MarketState::{to_snapshot,from_snapshot}`.
- `fynd-core/src/offline.rs`: the harness.
  - `load_snapshot(path) -> MarketState`
  - `prepare(snapshot, gas_token, max_hop, slippage) -> (MarketData, SharedDerivedDataRef)`
  - `OfflineSolver<A>::new(market, derived, algo)` builds graph + edge weights once;
    `.solve(order) -> OfflineSolution`.
  - `OfflineSolution { net_amount_out (primary metric), gross_amount_out, total_gas, num_swaps,
    num_paths, protocols }`. Primary comparison metric is `net_amount_out` (== production's
    `amount_out_net_gas`).
- Capture tool: `fynd-core/examples/capture_snapshot.rs` (live; user runs it with TYCHO creds).
- Comparison CLI: `fynd-benchmark quality` subcommand (offline, deterministic).

## Algorithm ideas to try (from research notes), ranked

Current baselines are **single-path, no splitting**; the biggest unexploited win is splitting large
trades across parallel paths.

1. **Equal-marginal-price parallel split** (Balancer SOR2 style): take candidate paths from ML/BF,
   allocate flow so the marginal price after each leg is equal; add a path only if net (after gas)
   improves. Lowest-risk first win.
2. **Fill-and-spill** (Penumbra): water-fill the best route until its marginal price meets the next
   best, then spill; handles routes that share pools.
3. **Top-N re-simulation**: re-simulate the top-N candidate routes end-to-end (fixes
   relaxation-vs-execution divergence). Quality win on top of BF.
4. **Gas-in-relaxation** for BF (already partly present): relax on net-of-gas.
5. Long-term: convex network flow / dual-decomposition over token prices (optimal oracle).

Benchmark caveats from notes: large orders can go *negative* from gas/fragmentation (splitting is
not free); 5-hop is BF's sweet spot; verify wins aren't pool-oscillation artifacts.

## Progress log

- [DONE] Proved snapshot serialization round-trips a real `UniswapV2State` through JSON.
  Test: `fynd-core` `market_data::tests::snapshot_round_trips_real_uniswap_v2_state`.
- [DONE] Offline harness end-to-end: real v2 pools → snapshot round-trip → derived compute → ML & BF
  both solve. Test: `offline::tests::offline_harness_solves_with_most_liquid_and_bellman_ford`.
- [DONE] Capture tool: `fynd-core/examples/capture_snapshot.rs`.
- [DONE] `quality` benchmark subcommand (offline comparison over the 10k dataset).
- [DONE] Captured a real snapshot and recorded ML vs BF baseline (below).
- [DONE] Implemented `SplitAlgorithm` (`fynd-core/src/algorithm/split.rs`), registered in the
  worker registry and the offline runner. Beats ML and BF on the sample (below).
- [TODO] Fill in full-10k 3-way numbers (`quality_3way_full.json`).
- [DONE] Hardened split into `split_max` (`fynd-core/src/algorithm/split_exp.rs`): finer,
  incremental allocation with a never-lose portfolio. Beats `split` 704–0 on the full 10k
  (see "Hardening: `split_max`" below).
- [DONE] Fill-and-spill for shared-pool routes: implemented and benchmarked, does not help on this
  market (top paths are almost always pool-disjoint). Kept as the `split_ff` research strategy,
  excluded from `split_max`. Convex oracle still open.

## How to run

```bash
# 1. Capture a snapshot (live; needs a working fynd Tycho key + a public RPC).
export TYCHO_API_KEY="$TYCHO_API_KEY_2"            # fynd-ethereum key
export TYCHO_URL=tycho-fynd-ethereum.propellerheads.xyz
export RPC_URL="https://ethereum-rpc.publicnode.com"
export MIN_TVL=10                                   # fynd endpoint requires tvl_gt == 10
export PROTOCOLS="uniswap_v2,uniswap_v3"            # beta/fynd keys cap WS connections at 2
cargo run --release -p fynd-core --example capture_snapshot

# 2. Download the trade dataset.
cargo run --release -p fynd-benchmark -- download-trades

# 3. Compare algorithms offline (reproducible).
cargo run --release -p fynd-benchmark -- quality \
  --snapshot market_snapshot.json --requests-file aggregator_trades_10k.json \
  --algorithms most_liquid,bellman_ford --baseline most_liquid --max-hops 4
```

### Environment gotchas (resolved)

- The `tycho-fynd-*` endpoints need a fynd-specific key (`TYCHO_API_KEY_2` here); the generic
  `TYCHO_API_KEY` is rejected ("Invalid authentication key").
- The fynd endpoint enforces `tvl_gt == 10` exactly (`MIN_TVL=10`).
- Beta/fynd keys cap concurrent WebSocket connections at 2 → subscribe to ≤2 protocols at once.
- Gas price needs a working RPC; the p2pify URL in the shell env 401s and `eth.llamarpc.com` 521s.
  `https://ethereum-rpc.publicnode.com` works.

## Results

### Snapshot

- Block 25401513, protocols uniswap_v2 + uniswap_v3, `tvl_gt=10`.
- 1793 native pools, 1502 tokens, 4.0 MB JSON (`market_snapshot.json`).

### Baseline: MostLiquid vs BellmanFord

1000-trade sample (seed 42), max_hops 3, timeout 1000ms:

| algorithm    | coverage | wins vs ML | losses | mean bps | median bps |
|--------------|---------:|-----------:|-------:|---------:|-----------:|
| most_liquid  | 564      | 0          | 0      | 0.00     | 0.00       |
| bellman_ford | 565      | 27         | 2      | +414.14  | 0.00       |

Coverage ~56% because the dataset contains many pairs whose pools are not uniswap v2/v3 (the only
protocols in this snapshot). BF beats ML on multi-hop trades (matches the research notes); most
single-hop trades are identical (median 0).

### SplitAlgorithm vs MostLiquid vs BellmanFord (full 10k)

Full dataset (9896 trades), max_hops 3, timeout 5000ms, baseline = bellman_ford
(`quality_3way_full.json`):

| algorithm    | coverage | wins vs BF | losses | mean bps | median bps |
|--------------|---------:|-----------:|-------:|---------:|-----------:|
| most_liquid  | 5864     | 22         | 235    | −21.27   | 0.00       |
| bellman_ford | 5876     | —          | —      | 0.00     | 0.00       |
| **split**    | **5891** | **711**    | **91** | **+19.85** | 0.00     |

**`split` beats BellmanFord 711–91 (≈7.8:1), beats MostLiquid by even more, and has the best
coverage of the three.** Median 0 because most (small) trades are single-hop and identical across
algorithms — splitting only changes large trades, which is exactly where the wins come from.

### How SplitAlgorithm works (`fynd-core/src/algorithm/split.rs`)

1. Enumerate candidate paths (BFS, reusing `MostLiquidAlgorithm::find_paths`); rank by the cheap
   spot×depth heuristic but never *drop* unscored paths (pools with missing derived data are still
   routable — dropping them was the bug that made early versions lose to BF).
2. Simulate candidates at the full amount; keep the best single-path result (this alone matches/beats
   greedy BF because it re-simulates many candidates end-to-end — the "Top-N re-simulation" idea).
3. Select **pool-disjoint** paths so their allocations never interfere on-chain.
4. Water-fill the order across them: each chunk goes to the path with the best *net* marginal output;
   a path is only activated when its first chunk's marginal output covers its gas (gas-aware).
5. Return the better of the split route or the best single path — so it never loses to single-path.

Tuning constants (`split.rs`): `DEFAULT_MAX_CANDIDATES=5000` (timeout-bounded), `DEFAULT_MAX_PATHS=4`,
`DEFAULT_NUM_CHUNKS=20`.

### vs Frank-Wolfe (`path_frank_wolfe`, on `main` 0.82.0)

`main` ships `PathFrankWolfeAlgorithm` (wraps BellmanFord to find paths, then optimizes the split
across them with a golden-section Frank-Wolfe loop). This branch predates it, so the comparison was
run in a throwaway worktree on `main` with `SplitAlgorithm` + the offline harness ported in
(`/tmp/fynd-fw`). Same snapshot + dataset.

1000-trade sample (seed 42), max_hops 3, baseline = `path_frank_wolfe`:

| algorithm        | coverage | wins vs FW | losses | mean bps |
|------------------|---------:|-----------:|-------:|---------:|
| most_liquid      | 564      | 2          | 36     | −43.3    |
| bellman_ford     | 565      | 0          | 10     | −0.09    |
| path_frank_wolfe | 565      | —          | —      | 0.00     |
| **split**        | **566**  | **57**     | **19** | **+11.95** |

Frank-Wolfe beats BF (10–0), confirming it's a real improvement. **`split` beats Frank-Wolfe 57–19
(~3:1), with positive bps, higher total output and the best coverage.**

Full 10k (9896 trades), max_hops 3, timeout 5000ms, baseline = `path_frank_wolfe`
(`quality_4way_full.json`):

| algorithm        | coverage | wins vs FW | losses | mean bps |
|------------------|---------:|-----------:|-------:|---------:|
| most_liquid      | 5864     | 20         | 415    | −25.99   |
| bellman_ford     | 5876     | 0          | 185    | −6.35    |
| path_frank_wolfe | 5875     | —          | —      | 0.00     |
| **split**        | **5891** | **635**    | **275** | **+12.60** |

Ordering on the full dataset: **split > path_frank_wolfe > bellman_ford > most_liquid.** `split` beats
Frank-Wolfe 635–275 (~2.3:1) with the best coverage of all four. FW beats BF (185–0) — it is a real
upgrade over BF, and split is above it.

The ~19 losses to FW are likely cases where the optimal split uses *overlapping* pools (FW models
shared-pool state via post-swap overrides; `split` forces pool-disjoint paths) or where FW's
golden-section line search finds a finer split than the 20-chunk water-fill. Closing them = allow
shared-pool splitting (fill-and-spill) and/or more chunks.

### Learnings

- The dominant quality lever is **splitting large trades**; small trades are single-hop and identical
  across algorithms (hence median bps 0 — the mean and win-count carry the signal).
- **Never filter candidate paths by a derived-data heuristic.** Pools with missing spot/depth still
  route fine; dropping them handed BF wins (losses fell 128→27→10 once unscored paths were kept).
- Re-simulating many candidates end-to-end already beats greedy BF before any splitting is added.
- Pool-disjoint splitting is simple and always on-chain-valid; it captures most of the gain without
  the complexity of shared-pool fill-and-spill.

### Remaining losses (91 trades vs BF)

≈1.5% of the common set. Likely BF reaching a path the BFS enumeration doesn't (hop-counting
semantics) or mega-hub timeouts. Net effect is still strongly positive (711 wins). Candidate for a
follow-up: align hop semantics with BF and add fill-and-spill for shared-pool routes.

## Hardening: the portfolio split (`split`)

The portfolio split router (`fynd-core/src/algorithm/split_exp.rs`, `SplitStrategy::Portfolio`) was
developed and benchmarked as `split_max`, then **promoted to be the `split` algorithm**:
`SplitAlgorithm` now delegates to it, so production (`worker_pools.toml`) and the offline harness both
run the portfolio under the name `split`. The old exhaustive water-fill it replaced is in this
branch's git history. The `split` / `split_max` labels below are the pre-promotion benchmark names
(incumbent exhaustive split vs. the portfolio); `split_incr` and `split_ff` remain as research
strategies. `path_frank_wolfe` was also ported from `main` onto this branch so all algorithms run on
the same frozen snapshot.

It sharpens the old exhaustive `split` on its two documented weaknesses (coarse allocation,
pool-disjoint restriction) while never regressing.

What changed:

1. **Incremental water-fill.** AMMs are path-independent in cumulative input (one swap of `x` ==
   two sequential swaps summing to `x`), so the marginal of the next chunk is probed against a
   committed pool overlay instead of re-simulating the whole path at the cumulative amount. Cost
   drops from O(chunks²) to O(chunks), which funds a 256-chunk grid vs the incumbent's 20.
2. **Two-phase allocation.** Naive fine-graining regresses on large trades: a smaller first chunk
   can fail the per-chunk gas-activation gate, so a profitable second path never turns on. Fix: pick
   the active path set at coarse (20-chunk) granularity where activation is correct, then refine the
   allocation over that fixed set with no gate.
3. **Never-lose portfolio.** `split_max` returns the best net of {best single path, an
   incumbent-equivalent coarse split, the refined split}. The coarse floor does exactly the
   incumbent's single 20-chunk pass, so a tight timeout can't starve it into a single-path fallback
   while `split` still splits. Enforced by `portfolio_never_loses_to_incumbent_split` and
   `portfolio_no_loss_under_tight_timeout`.

Full 10k (max_hops 3, timeout 5000 ms, baseline `split`, common-solved set 5,891):

| algorithm  | coverage | wins vs split | losses vs split | net delta vs split | win magnitude               |
|------------|---------:|--------------:|----------------:|-------------------:|-----------------------------|
| `split`    |    5,891 |             0 |               0 |                  0 | —                           |
| `split_max`|    5,891 |           704 |           **0** |          +2.81e24  | mean +1.6 bps, max +129 bps |

Identical coverage, never worse, better on ~12% of solved trades, up to +1.29% on a single trade.

Per-quote solve latency (in-process, single-threaded, `quality` now reports `p50/p95/max_ms`):

| algorithm      | p50     | p95      | max      |
|----------------|--------:|---------:|---------:|
| `bellman_ford` | 4.62 ms | 8.05 ms  | 26.81 ms |
| `split`        | 8.94 ms | 26.49 ms | 77.29 ms |
| `split_max`    | 9.37 ms | 26.86 ms | 78.37 ms |

`split_max` costs +0.43 ms at p50 (~5%) and ~+1–2% at p95/max over `split`. The tail is dominated by
candidate enumeration and full-amount simulation, which are shared with `split`; the extra allocation
passes only add a small fixed cost on trades that actually split. Run all variants with
`--algorithms split,split_incr,split_ff,split_max`; the report truncates `total_net`, so use
`analyze_nets.py RESULT.json split` for exact per-trade deltas.

Tried and dropped from the portfolio at the time: **fill-and-spill** (shared-pool splitting) with
naive top-4 candidate selection net-regressed alone and added ~0 to the portfolio max — the top
full-amount candidate paths are almost always pool-disjoint. It was later re-added with marginal-probe
candidate selection after the `split_bounded` head-to-head (next section) showed why the naive version
missed: the winning extra path ranks poorly at full size but wins on the margin. **Wider path cap
(8 vs 4)** added only sub-bps wins for ~2× the allocation cost and stays out.

Primary comparison metric is `net_amount_out` (== production `amount_out_net_gas`).

## Beating `split_bounded`: the gap was tree routes, not discovery

`docs/beat-split-bounded-handoff.md` hypothesized that `split_bounded`'s 442-trade edge over the
portfolio came from its bounded connector/anchor candidate discovery (incl. the native-ETH sentinel).
Tested and **refuted** on this snapshot:

1. The bounded discovery was made generic over the graph edge weight (`split_bounded.rs`,
   `find_candidate_paths`) and unioned into the portfolio's candidate set in
   `ExpSplitAlgorithm::setup()`. Instrumentation showed `new=0` candidates on every order: the
   portfolio's exhaustive BFS (connector filter off) already contains every path the bounded
   discovery finds, and its 5,000-candidate truncation never bit (observed pre-ranked sets ≤ ~1,024).
   Benchmark results were bit-identical. The union is kept — it costs nothing measurable and protects
   markets where BFS truncation would bite — but it closed zero losses.
2. Route dumps of the top losing trades showed the real gap: every structural loss was a **tree
   route** from `split_bounded`'s shared-pool fill-and-spill — a split at an *intermediate* token
   (e.g. PENDLE→WETH at full size, then WETH split 75/25 across two FET pools), where the parallel
   paths share a pool. No pool-disjoint allocation can express that shape.

Fix: fill-and-spill re-enabled in the portfolio with `split_bounded`'s candidate selection scheme
(top-8 full-amount paths + first-chunk marginal probe of the top-32, up to 12 candidates, active-path
cap 4), keeping the portfolio's two-phase coarse-set → 256-chunk fine allocation and the never-lose
floor.

Full 10k, frozen snapshot, `--max-hops 3 --timeout-ms 5000`, baseline `split_bounded`, common set 810
(exact deltas via `analyze_nets.py`):

| portfolio version    | wins | losses | ties | net delta vs bounded | loss mass | p50     | p95     |
|----------------------|-----:|-------:|-----:|---------------------:|----------:|--------:|--------:|
| before (disjoint-only) | 346 |    442 |   22 |            +3.49e25 |   2.43e25 | 10.3 ms | 30.7 ms |
| + fill-and-spill     |  568 |    233 |    9 |             +5.88e25 |   3.55e23 | 12.2 ms | 40.8 ms |
| + split-primitives route assembly | **709** | **93** | 8 | **+5.92e25** | 4.64e19 | 14.9 ms | 51.4 ms |

Coverage stays 5,891 (vs `split_bounded`'s 810 — it declines when no split beats the single path).
The final row is the encoding-safe assembly via `split_primitives::build_split_route` (see the
production-bug fix below): shared-hop merging and the sequential shared-overlay execution model
also improved the measured routes, cutting losses to 93 (loss mass 4.6e19, i.e. ~zero) at the cost
of +2.7 ms p50. The per-solve timeout still guards the tail and the floor holds under starvation
(`portfolio_no_loss_under_tight_timeout`).

### Production encode path — bugs the offline harness cannot see

A live audit against the real quote/encode path (KyberSwap/Nordstern comparison, 2026-07-13)
surfaced two bugs invisible offline because the harness never encodes routes:

1. `ExpSplitAlgorithm` built raw swap lists with a split fraction on every parallel leg and an
   empty route token map. The encoder rejected them, and after the token map was supplied it
   panicked (BigUint underflow): tycho-execution requires the last parallel swap out of a token to
   carry `split = 0` (remainder convention). Fix: assemble candidates via
   `split_primitives::build_split_route` — topological swap order, remainder splits, shared-hop
   merge, token map — the same path PFW and `split_bounded` use.
2. The worker reported sell `amount_out` from the route's last swap only, understating every split
   route. Backported main's fix: sum all swaps into the output token.

Live-audit head-to-head on the same 808 solved trades (local, `all_onchain` min_tvl=10, no RFQ):
the portfolio split beat `path_frank_wolfe` on 599/808 trades (74%), median +0.57 bps net-of-gas.
Charts: PR #284 description (`assets/bench-284-audit-charts` branch).
