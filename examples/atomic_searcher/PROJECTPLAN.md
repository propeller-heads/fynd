# Atomic Searcher Showcase - Project Plan

## Status: IN PROGRESS

Last updated: 2026-03-10
Branch: `ms/atomic-searcher-showcase` (based on `ms/bellman-ford-algorithm` / PR #43)
Repo: `/Users/markusschmitt/Documents/GitHub/fynd`

## What Was Built

An `examples/atomic_searcher/` showcase for Fynd that runs Janos Tapolcai's cyclic
arbitrage algorithm as an autonomous searcher (Token A -> ... -> Token A), complementing
the BellmanFord solver (Token A -> Token B) from PR #43.

### Files Created

```
examples/atomic_searcher/
  main.rs              - CLI, Tycho feed setup, per-block search loop (494 lines)
  cycle_detector.rs    - Layered BF for cycle detection + tests (700+ lines)
  amount_optimizer.rs  - Golden section search + tests (750+ lines)
  types.rs             - CycleCandidate, EvaluatedCycle, BlockSearchResult (49 lines)
  README.md            - Algorithm docs, usage, references
  PROJECTPLAN.md       - This file
```

### Library Changes (visibility only)

Made these types `pub` (was `pub(crate)`) so the example can use them:
- `src/feed/events.rs`: MarketEvent, EventError, MarketEventHandler
- `src/feed/mod.rs`: TychoFeedConfig, DataFeedError
- `src/feed/tycho_feed.rs`: TychoFeed
- `src/graph/mod.rs`: GraphManager, GraphError

### Cargo.toml Changes

- Added `[[example]] name = "atomic_searcher"`
- Added dev-dependencies: `num-bigint`, `num-traits`, `toml`

## Commits on Branch (5)

```
425ea7f test: add 11 unit tests for cycle detector and amount optimizer
9470f93 fix: address review findings from extreme code review
7545330 improve: show gross profit and cycle status labels in output
bb4310a fix: add blacklist support and token-level filtering to atomic searcher
f2a0ee7 feat: add atomic searcher showcase example
```

## Algorithm

1. Subscribe to Tycho market updates via Fynd's TychoFeed
2. On each block, extract subgraph around source token (BFS to max_hops)
3. Run layered Bellman-Ford relaxation (same as solver) but check edges back
   to source for cycles where amount_out > amount_in
4. Filter cycles through blacklisted tokens (AMPL rebase token)
5. For each candidate, run golden section search to find optimal input amount
6. Log results: PROFITABLE / GROSS+ / no-arb with amounts

Based on: https://github.com/jtapolcai/tycho-searcher
Paper: https://www.overleaf.com/read/ksqhzzmndmqh

## What Was Tested

### Unit Tests (11, all passing)

Cycle detector (6):
- simple_cycle_found: triangle A->B->C->A, product 6x
- no_cycle_when_unprofitable: all sp=1
- empty_graph_no_cycles
- multi_hop_cycle: 4-hop A->B->C->D->A, product 8x
- extract_subgraph_respects_depth
- dedup_removes_duplicate_cycles

Amount optimizer (5):
- gss_finds_optimal_amount: profitable triangle
- gss_unprofitable_cycle: product < 1
- gss_handles_small_seed: seed=1
- evaluate_cycle_returns_correct_amounts: exact 1000*2*3/1=6000
- candidate_with_missing_nodes_returns_none

### Live Testing (Ethereum mainnet via Tycho)

- ~2,087 tokens, ~2,438 pools, 4,876 edges
- Protocols: uniswap_v2, uniswap_v3, sushiswap_v2
- Max hops: 3 and 4
- Seed amounts: 0.5 ETH and 1.0 ETH
- Blocks observed: ~12 across multiple runs
- Results: GROSS+ cycles found (WETH/USDT/WBTC triangle, 0.01-0.04 bps gross),
  all unprofitable after gas (~0.012 ETH per 3-hop). Consistent with saturated market.
- Performance: 130-180ms per block search

### AMPL Investigation

All initial "profitable" cycles routed through AMPL (Ampleforth, rebase token at
0xd46ba6d942050d489dbd938a2c909a5d5039a161). Pool-level blacklist (blacklist.toml)
only blocked 2 direct AMPL/WETH pools, but searcher found longer paths:
WETH -> USDC -> SPOT -> AMPL -> WETH. Fixed with token-level --blacklist-tokens flag.

## Code Review (Extreme: Claude + Codex + Gemini)

Review report at: `notes/team-review-ms-atomic-searcher-showcase-20260309-2121.md`

### Issues Found and Fixed

1. GSS f64 precision: capped at 1000 ETH, safe_to_biguint guards, non-finite short-circuit
2. Redundant cycle scan: removed first pass (-89 lines), find_closing_edges covers it
3. seed_eth validation: bail on <= 0 / NaN / infinity

### Issues Acknowledged (not fixed, documented)

4. O(V) Address-to-NodeIndex lookup per edge in GSS (should build HashMap)
5. No state overrides for repeated pools (solver has this, example doesn't)
6. net_profit uses i128 with saturation (should use BigInt)
7. Distance arrays sized by full graph, not subgraph (memory optimization)

## Known Limitations

### CRITICAL: Only searches WETH-denominated cycles

The searcher ONLY finds cycles starting and ending at WETH. It misses:
- Stablecoin triangles: USDC -> DAI -> USDT -> USDC
- Wrapped BTC cycles: WBTC -> cbBTC -> WBTC
- Any cycle that doesn't touch WETH

This is the same as Janos's original implementation but should be expanded.
To fix: run BF from multiple source tokens (top N by connectivity).

### Other Limitations

- No cycle merging (shared edges save gas). Janos's code has this (Phase 3).
- No biconnected component decomposition (parallelize independent subgraphs).
- No state overrides for re-simulation (pools visited twice get stale state).
- No on-chain execution / calldata encoding.
- Only tested with UniV2 + UniV3 + SushiV2. Not tested with Curve, Balancer,
  PancakeSwap, Ekubo, or UniV4.
- Only tested on Ethereum. Not tested on Base or Unichain.
- Gas price assumes 1:1 for WETH source token. Non-WETH sources would need
  token gas price conversion (like the solver's TokenGasPrices).

## TODO (Next Steps)

### Before Merging

- [ ] PR #43 (BellmanFord solver) must merge first, or rebase this on top
- [ ] Push branch and open PR
- [ ] Address remaining "Consider" items from review if desired

### Follow-up Features

- [ ] Multi-source token support (run BF from WETH, USDC, WBTC, etc.)
- [ ] Cycle merging (Phase 3 from Janos's algorithm)
- [ ] State override re-simulation for repeated pools
- [ ] More protocol coverage (Curve, Balancer, UniV4)
- [ ] Base / Unichain testing
- [ ] Benchmark: cycles found per block over 1000+ blocks

## References

- Janos's tycho-searcher: https://github.com/jtapolcai/tycho-searcher
- Paper writeup: https://www.overleaf.com/read/ksqhzzmndmqh
- FC 2026 paper on arbitrage market saturation (accepted)
- Fynd PR #43 (BellmanFord solver): https://github.com/propeller-heads/fynd/pull/43
- Notion design doc: https://www.notion.so/Routing-Algorithm-Design-20a2ed0857018035b34cc66c8d01ce6e
- Telegram chat: "Janos < > Propeller" (chat ID 4944590475)
- Plan doc: /Users/markusschmitt/Documents/llm-output/2026-03-09-fynd-atomic-searcher-plan.md

## How to Resume

```bash
cd /Users/markusschmitt/Documents/GitHub/fynd
git checkout ms/atomic-searcher-showcase
# Branch is based on ms/bellman-ford-algorithm (PR #43)

# Run tests
cargo test --example atomic_searcher

# Run live
cargo run --example atomic_searcher -- \
  --max-hops 4 --seed-eth 0.5 \
  --protocols uniswap_v2,uniswap_v3,sushiswap_v2 \
  --blacklist blacklist.toml

# Check compilation
cargo check --example atomic_searcher
```

## Key Design Decisions

1. **Example, not library feature**: Searchers are autonomous agents, Fynd's core is
   order-driven. Keeping it as a showcase maintains clean separation.

2. **Reuse BF from PR #43**: Rather than porting Janos's code directly, we adapted
   the solver's BF (already integrated with Fynd's abstractions) for cycle detection.

3. **GSS in f64 space**: Acceptable for a showcase. Production would need integer-space
   optimization. Capped at 1000 ETH to stay in f64 safe range.

4. **Token-level blacklist**: Pool-level blacklisting is insufficient for rebase tokens
   like AMPL that have many pool variants. Token blacklist catches all paths through
   the problematic token.
