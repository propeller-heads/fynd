# Handover: Routing-Quality Benchmark & "Beat Split" Challenge

You are being asked to write a routing algorithm that beats the current best (`split`) on output
quality. This document tells you exactly how the benchmark works, how to run it, what the current
standings are, and where `split` is weak so you can attack it.

The companion doc `docs/routing-quality-bench.md` has the full design rationale and results history.

## The challenge

Beat `SplitAlgorithm` on the 10k aggregator trade dataset, measured by `net_amount_out`
(== production `amount_out_net_gas`) — more output after gas wins. Current ranking on the full 10k:

**split > path_frank_wolfe > bellman_ford > most_liquid**

`split` beats Frank-Wolfe 635–275 (~2.3:1, +12.6 bps mean) with the best coverage. Your job: beat
`split`. Same rules: implement an `Algorithm`, run it through the offline harness, win on
`net_amount_out`.

## What the benchmark is

A deterministic, in-process replay harness. Production needs a live Tycho feed; for algorithm
research we capture one market snapshot to disk and replay it so every algorithm solves the **same
frozen state**. Fully reproducible — that is what makes iteration possible.

Pieces:

- `fynd-core/src/feed/market_data.rs` — `MarketSnapshot` + `MarketState::{to_snapshot,from_snapshot}`.
  Native `ProtocolSim` states round-trip through JSON via `#[typetag::serde]`. (VM pools don't
  serialize — snapshot is native protocols only.)
- `fynd-core/src/offline.rs` — the harness:
  - `load_snapshot(path) -> MarketState`
  - `prepare(snapshot, gas_token, max_hop, slippage) -> (MarketData, SharedDerivedDataRef)` — runs the
    derived-data computations once (spot prices → token gas prices + pool depths).
  - `OfflineSolver::<A>::new(market, derived, algo)` builds the graph + edge weights once;
    `.solve(order) -> OfflineSolution`.
  - `run_algorithm(market, derived, name, config, orders)` — string-keyed dispatch; add your algo here.
  - `AVAILABLE_ALGORITHMS` — list of runnable names.
- `fynd-core/examples/capture_snapshot.rs` — captures a live snapshot to disk.
- `tools/benchmark/src/quality.rs` — the `quality` subcommand: runs each algo over the dataset, reports
  coverage, head-to-head wins/losses vs a baseline, mean/median bps, total net.

Primary metric: `OfflineSolution.net_amount_out` (BigInt, gas-adjusted). `gross_amount_out`,
`total_gas`, `num_paths`, `protocols` are reported for context.

## How to run it

### 1. Get a market snapshot

If `market_snapshot.json` already exists in the repo root, reuse it for apples-to-apples comparison.
Otherwise capture one (needs a working Tycho key + a public RPC):

```bash
export TYCHO_API_KEY="$TYCHO_API_KEY_2"             # the fynd-ethereum key; generic TYCHO_API_KEY is rejected
export TYCHO_URL=tycho-fynd-ethereum.propellerheads.xyz
export RPC_URL="https://ethereum-rpc.publicnode.com" # p2pify URL in shell env 401s; llamarpc 521s
export MIN_TVL=10                                     # fynd endpoint requires tvl_gt == 10 exactly
export PROTOCOLS="uniswap_v2,uniswap_v3"             # key caps WebSocket connections at 2 → ≤2 protocols
export SNAPSHOT_OUT=market_snapshot.json
cargo run --release -p fynd-core --example capture_snapshot
```

A captured snapshot is ~1800 native pools / ~1500 tokens / ~4 MB at one block. Note: a fresh snapshot
is a *different block* than the one prior numbers were measured on — to compare your algo against the
recorded standings, either reuse the existing snapshot or re-run all algorithms on your new snapshot.

### 2. Get the dataset

```bash
cargo run --release -p fynd-benchmark -- download-trades   # → aggregator_trades_10k.json
```

### 3. Run the comparison

```bash
cargo run --release -p fynd-benchmark -- quality \
  --snapshot market_snapshot.json --requests-file aggregator_trades_10k.json \
  --algorithms most_liquid,bellman_ford,path_frank_wolfe,split,YOUR_ALGO \
  --baseline split --max-hops 3 --timeout-ms 5000 --output result.json
```

`--baseline split` makes the win/loss/bps columns measure everyone *against split* — so you can see
directly whether YOUR_ALGO beats it. Use `--num-requests N` for a fast sample during iteration
(`--seed` makes the subset reproducible); `0` = all trades.

Note: this branch (`feat/split-routing-benchmark`) does **not** contain `path_frank_wolfe` — that
algorithm lives on `main` (0.82.0). To run the full 4-way you must be on a `main`-based tree with
`split` + the harness ported in (that is how the recorded 4-way numbers were produced; see the bench
doc). On this branch you can compare `most_liquid,bellman_ford,split` directly.

## How `split` works (your target)

`fynd-core/src/algorithm/split.rs`:

1. Enumerate candidate paths (BFS, reusing `MostLiquidAlgorithm::find_paths`); rank by the cheap
   spot×depth heuristic but **never drop** unscored paths (pools with missing derived data still
   route — dropping them was a bug that lost trades to BF).
2. Simulate candidates at the full amount; keep the best single-path result. This alone matches/beats
   greedy Bellman-Ford because it re-simulates many candidates end-to-end ("Top-N re-simulation").
3. Select **pool-disjoint** paths so their allocations never interfere on-chain.
4. Water-fill the order across them: each chunk → the path with the best *net* marginal output; a path
   is only activated when its first chunk's marginal output covers its gas (gas-aware).
5. Return the better of the split route or the best single path (never loses to single-path).

Tuning constants: `DEFAULT_MAX_CANDIDATES=5000` (timeout-bounded), `DEFAULT_MAX_PATHS=4`,
`DEFAULT_NUM_CHUNKS=20`.

## Where `split` is weak (how to beat it)

- **Pool-disjoint only.** It refuses to split across paths that share a pool. When the optimal split
  *does* use overlapping pools, it falls back to a worse disjoint set. Frank-Wolfe handles this with
  post-swap state overrides (fill-and-spill). This is the biggest gap — ~275 of split's wins over FW
  would flip if FW also covered the disjoint cases, and split loses to FW exactly here.
- **Coarse allocation.** 20-chunk water-fill, not a true line search. A golden-section / convex
  optimizer on the split fractions would find better allocations.
- **Heuristic candidate ranking.** spot×depth ordering; for mega-hub tokens the best path can rank
  late and get cut by the timeout. Better pre-ranking (e.g. cheap marginal-price probe) helps.
- **No shared-pool accounting in the water-fill marginal.** Marginals are computed from original pool
  state per path; correct only because paths are disjoint. Lifting the disjoint constraint requires
  modelling cumulative pool depletion (see `split_primitives.rs` `MarketOverrides` on `main`).

Ideas from the research notes (`docs/routing-quality-bench.md` lists them): equal-marginal-price
splitting across overlapping routes, fill-and-spill, convex network flow / dual decomposition over
token prices (the optimal oracle).

## How to add your algorithm

1. Implement the `Algorithm` trait in `fynd-core/src/algorithm/your_algo.rs`. Use `split.rs` and
   `most_liquid.rs` as templates. `GraphType`/`GraphManager` are usually
   `StableDiGraph<DepthAndPrice>` + `PetgraphStableDiGraphManager<DepthAndPrice>` (if you want derived
   edge weights) or the `()` variants (if you simulate directly).
2. Register it: `pub mod your_algo;` + `pub use` in `algorithm/mod.rs`; add a `match` arm + spawn fn
   in `worker_pool/registry.rs` and to `AVAILABLE_ALGORITHMS` there.
3. Wire it into the harness: add a `match` arm in `offline::run_algorithm` and to
   `offline::AVAILABLE_ALGORITHMS`.
4. Add unit tests (see `split.rs` tests: it asserts >15% gain on two equal pools and never losing to
   single-path on tiny orders). Use `algorithm::test_utils` to build markets.
5. Run `cargo nextest run -p fynd-core`, `cargo +nightly clippy -p fynd-core -p fynd-benchmark
   --all-targets --all-features`, `cargo +nightly fmt`, and the rustdoc check from `.claude/knowledge/rust.md`.

## Rules / fairness

- Compare on the **same snapshot** as the incumbent, or re-run all algorithms on your new snapshot.
- Primary metric is `net_amount_out`. Watch coverage (don't win by only solving easy trades) and watch
  that wins aren't pool-oscillation artifacts (a known BF pitfall — see the notes).
- Routes you return must be valid (correct token chaining; splits sum to the order amount). The
  offline harness sums terminal-leg outputs, so multi-path routes must tag each leg's `split` and end
  in the order's `token_out`.

## File map

| File | Role |
|---|---|
| `fynd-core/src/feed/market_data.rs` | `MarketSnapshot` + `to_snapshot`/`from_snapshot` |
| `fynd-core/src/offline.rs` | offline harness: load → prepare → `OfflineSolver` / `run_algorithm` |
| `fynd-core/src/algorithm/split.rs` | the incumbent `SplitAlgorithm` (your target) |
| `fynd-core/examples/capture_snapshot.rs` | live snapshot capture |
| `tools/benchmark/src/quality.rs` | `quality` subcommand (comparison + reporting) |
| `docs/routing-quality-bench.md` | design rationale, results history, research ideas |
