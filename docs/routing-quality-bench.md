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
- [TODO] (optional) fill-and-spill for shared-pool routes; convex oracle.

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

Primary comparison metric is `net_amount_out` (== production `amount_out_net_gas`).
