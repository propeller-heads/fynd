# Handoff: beat `split_bounded` by giving the portfolio its candidate discovery

Successor to `docs/routing-quality-handover.md`. Read that first for how the offline benchmark works.

> **RESOLVED (2026-07-12).** The discovery hypothesis below was tested and refuted: the portfolio's
> exhaustive BFS already contained every bounded-discovery candidate (union added `new=0` paths on
> every order; results bit-identical). The real gap was `split_bounded`'s shared-pool fill-and-spill
> producing tree routes that split at an intermediate token, which pool-disjoint allocation cannot
> express. Fill-and-spill with marginal-probe candidate selection was added to the portfolio, which
> now wins the head-to-head 568W/233L (was 346W/442L), net delta +5.88e25, coverage 5,891 kept,
> p50 12.2 ms. Remaining losses are grid noise (median 0.03 bps, max 3.8 bps). See
> `docs/routing-quality-bench.md` § "Beating `split_bounded`" for full results.

## Task

The hardened portfolio split (`split`, in `fynd-core/src/algorithm/split_exp.rs`) has a **better
allocator** than `split_bounded` but **weaker candidate discovery**. Give the portfolio
`split_bounded`'s discovery (direct + connector + anchor tokens, including the native-ETH sentinel),
run the portfolio allocation on those candidates, and re-benchmark against `split_bounded`. Goal:
win the head-to-head on `split_bounded`'s covered set **and** keep the portfolio's full coverage.

## Why (the finding that motivates this)

Full 10k, frozen `market_snapshot.json`, `--max-hops 3 --timeout-ms 5000`, baseline `split_bounded`,
common set = 810 trades (all algos solved):

| algorithm | coverage | wins vs bounded | losses | aggregate net Δ | p50 |
|---|---:|---:|---:|---:|---:|
| `split_bounded` | 810 | — | — | 0 | 2.4 ms |
| `split` (portfolio) | 5,891 | 346 | **442** | +3.49e25 | 10.4 ms |
| `split_legacy` (old exhaustive) | 5,891 | 112 | 681 | +3.21e25 | 9.6 ms |

The portfolio does **not** strictly beat `split_bounded`: by trade count `split_bounded` wins more
(442 vs 346), though the portfolio wins bigger in aggregate net and covers 7× more orders.
`split_bounded`'s edge is **discovery** — it finds split candidates through connector/anchor tokens
(incl. native ETH) that the portfolio's Most-Liquid-BFS discovery never surfaces, so no amount of
better allocation recovers them. The same root cause produced the portfolio's losses to Path
Frank-Wolfe. Fix the discovery and the allocator advantage should carry the head-to-head.

## The plan

1. Lift `split_bounded`'s discovery into the portfolio's candidate enumeration.
   - Today the portfolio enumerates candidates in `ExpSplitAlgorithm::setup()`
     (`split_exp.rs`, ~line 230) via `MostLiquidAlgorithm::find_paths(...)` on
     `StableDiGraph<DepthAndPrice>`.
   - `split_bounded`'s discovery is `BoundedSplitEngine::find_paths()`
     (`split_bounded.rs`, line 228) plus `default_anchor_tokens()` (line 159, includes the native-ETH
     sentinel `0x0000…0000`, WETH, major stables) and `candidate_edges_for_state` / `candidate_priority`
     (lines 423, 481). It runs on `StableDiGraph<()>`.
2. Replace (or union) the portfolio's candidate set with the bounded discovery output, then run the
   existing portfolio allocation unchanged (`disjoint_alloc` → `disjoint_waterfill` →
   `build_disjoint_legs`, and the `floor_alloc` never-lose floor). The allocator is weight-agnostic:
   it only reads `edge.component_id` and re-simulates via the market, so it does not need the
   `DepthAndPrice` edge weights.
3. Keep the never-lose portfolio structure: best of {single path, coarse floor, refined split}. The
   floor guarantees no regression vs the incumbent behaviour under tight timeouts.
4. Re-benchmark vs `split_bounded` (command below). Success = win more of the 810 common trades than
   you lose, keep coverage ≈ 5,891, and stay within a sane latency budget.

### The one real design obstacle: graph-type mismatch

- Portfolio graph = `StableDiGraph<DepthAndPrice>`; `split_bounded` graph = `StableDiGraph<()>`.
- The portfolio's `Path<DepthAndPrice>` carries `DepthAndPrice` edge weights only used for the cheap
  `MostLiquidAlgorithm::try_score_path` pre-ranking in `setup()`. The allocation phase uses only
  `path.iter()` (token sequence) and `edge.component_id`.
- Options, cheapest first:
  - **(a)** Write a discovery pass that returns `Path<DepthAndPrice>` (or a lightweight
    `Vec<(token_in, component_id, token_out)>` the allocator can consume) using `split_bounded`'s
    frontier logic but reading the `DepthAndPrice` graph the portfolio already builds. Keep
    `setup()`'s ranking, just widen the candidate set.
  - **(b)** Change `ExpSplitAlgorithm` to `GraphType = StableDiGraph<()>` and reuse
    `split_bounded::find_paths` directly, dropping the spot×depth pre-ranking (rank by full-amount
    sim instead, which `setup()` already does in step 3). Simpler discovery reuse, but the portfolio
    then needs a different ranking than `try_score_path`.
  - Prefer (a) if `try_score_path` pre-ranking matters for timeout-bounded hub tokens; else (b) is
    less code.
- Note the harness passes `connector_tokens = None`, so both algorithms fall back to
  `default_anchor_tokens()`. Make sure the portfolio's discovery uses the same anchor set for a fair
  and effective comparison.

## Current branch state (`exp/split-hardening`, local, nothing pushed)

Algorithms registered in the offline harness (`fynd-core/src/offline.rs`,
`AVAILABLE_ALGORITHMS` + `run_algorithm` / `run_algorithm_routes`):

| name | what | notes |
|---|---|---|
| `split` | the hardened portfolio | `SplitAlgorithm` delegates to `ExpSplitAlgorithm::portfolio` |
| `split_bounded` | ported from #270 (`origin/feat/split-shared-routing-quality`) | the target to beat |
| `split_legacy` | old exhaustive split | offline-only baseline, not in production registry |
| `split_incr` / `split_ff` | portfolio research strategies (refined-disjoint / fill-and-spill) | |
| `path_frank_wolfe` | ported from main | needs main's `bellman_ford` (also ported) |
| `most_liquid`, `bellman_ford` | reference single-path | `bellman_ford` is main's version after the PFW port |

Tooling: `fynd-benchmark quality` (adds `p50/p95/max_ms` latency), `fynd-benchmark routes` (dumps
routes in the route-viz normalized schema). Python: `analyze_nets.py` (exact per-trade net deltas,
the report truncates `total_net`), `stack_compare.py`, `add_paths.py`, `unroll_layers.py`,
`build_comparison.py`, `build_pr_charts.py`.

## How to benchmark

```bash
cargo build --release -p fynd-benchmark
./target/release/fynd-benchmark quality \
  --snapshot market_snapshot.json --requests-file aggregator_trades_10k.json \
  --algorithms bellman_ford,split_bounded,split --baseline split_bounded \
  --max-hops 3 --timeout-ms 5000 --output vs_bounded.json
python3 analyze_nets.py vs_bounded.json split_bounded   # exact wins/losses on the common set
```

The bar to clear vs `split_bounded` on its ~810 common trades: currently 346W/442L. Beat that
(more wins than losses) without losing coverage or blowing up p50 (< ~12 ms is fine; `split_bounded`
is 2.4 ms, portfolio 10.4 ms).

## Gotchas

- **Discovery is the whole lever.** The portfolio allocator already wins on aggregate net; do not
  touch the allocation math (`disjoint_waterfill`, the two-phase active-set selection, the floor).
  Only widen the candidate set.
- **Anchor set must include the native-ETH sentinel** `0x0000…0000` (see `default_anchor_tokens`).
  The bench doc notes that without it, full setups miss WETH→ETH→token routes and lose large-trade
  quality.
- **`split_bounded` declines most orders** (returns `InsufficientLiquidity` when no split beats the
  single path), so its coverage is ~810 on this snapshot and the common set is small. Compare on the
  common set via `analyze_nets.py`. Coverage is NOT a defect for `split_bounded` — in production it
  runs beside a single-path pool.
- **Win-count vs aggregate-net can disagree** (they do here). Report both; the production router
  ranks on net, but a candidate that loses more trades than it wins is a hard sell even if aggregate
  net is up. Aim to win both.
- **Latency**: the portfolio does 3 water-fill passes. Adding richer discovery increases the
  candidate-simulation cost (the dominant term). Watch p95/max on hub tokens; the per-solve timeout
  guards it but can truncate the fine pass (the floor still holds — see
  `portfolio_no_loss_under_tight_timeout`).
- **`split_bounded` was ported with its tests stripped** (they referenced #270's test utils). If you
  need its unit tests, pull them from `origin/feat/split-shared-routing-quality` and adapt.

## Branch / PR situation — do not push blindly

- This work is on `exp/split-hardening` (worktree), branched from an **old** `feat/split-routing-benchmark`.
- #270 (`origin/feat/split-shared-routing-quality`) is **307 commits ahead** of our common base and
  is where `split_bounded` lives. It has **no offline harness** (that lives on this branch's lineage).
- A git merge of this branch into #270 is not viable (massive conflicts, drags old Bellman-Ford/APIs).
  To land anything on #270 you must **port** the winning algorithm onto #270's current APIs and prove
  it beats `split_bounded` there — and get explicit approval before pushing to the shared PR branch
  or editing the #270 description (outward-facing).
- **Only replace `split_bounded` if the combined-discovery portfolio actually wins the head-to-head.**
  As of this handoff it does not.

## Key files

| File | Role |
|---|---|
| `fynd-core/src/algorithm/split_exp.rs` | the portfolio; `setup()` is where discovery is enumerated |
| `fynd-core/src/algorithm/split_bounded.rs` | discovery to lift: `find_paths` (228), `default_anchor_tokens` (159), `candidate_edges_for_state` (423), `candidate_priority` (481) |
| `fynd-core/src/algorithm/split_primitives.rs` | shared split helpers (main/pfw version; `build_split_route`, `HopDescriptor`, `PathAllocation`, `SimulatedHop`) |
| `fynd-core/src/offline.rs` | harness: `run_algorithm`, `run_algorithm_routes`, `AVAILABLE_ALGORITHMS` |
| `tools/benchmark/src/quality.rs` | `quality` subcommand (+ latency columns) |
| `docs/routing-quality-bench.md` | results history + the hardening writeup |
