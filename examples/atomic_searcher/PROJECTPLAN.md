# Atomic Searcher Showcase - Project Plan

## Status: IN PROGRESS

Last updated: 2026-03-10
Branch: `ms/atomic-searcher-showcase` (based on `ms/bellman-ford-algorithm` / PR #43)
Repo: `/Users/markusschmitt/Documents/GitHub/fynd`

## Condition for Completion

An example that runs and settles trades on all chains we support, and a guide that
gets team approval. (from Searcher Showcase spec)

## What Was Built (Phase 0)

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

## Commits on Branch (6)

```
0c01353 docs: add project plan and review notes for atomic searcher
425ea7f test: add 11 unit tests for cycle detector and amount optimizer
9470f93 fix: address review findings from extreme code review
7545330 improve: show gross profit and cycle status labels in output
bb4310a fix: add blacklist support and token-level filtering to atomic searcher
f2a0ee7 feat: add atomic searcher showcase example
```

## Algorithm

1. Subscribe to Tycho market updates via Fynd's TychoFeed
2. On each block, extract subgraph around source token (BFS to max_hops),
   excluding blacklisted tokens from the market graph before search begins
3. Run layered Bellman-Ford relaxation (same as solver) but check edges back
   to source for cycles where amount_out > amount_in
4. For each candidate, run golden section search to find optimal input amount
5. Log results: PROFITABLE / GROSS+ / no-arb with amounts

Based on: https://github.com/jtapolcai/tycho-searcher
Paper: https://www.overleaf.com/read/ksqhzzmndmqh

---

## Feature Backlog

### Phase 1: Close the Loop (Execution)

Turn the searcher from "logs opportunities" into "actually trades." Uses
`tycho-execution` for calldata encoding and `alloy` for signing/broadcasting
(Option B: direct execution, not through `FyndClient`).

**Why not FyndClient?** The `fynd-client` (PR #58) is designed around A->B solver
orders. The atomic searcher finds its own cycles (A->...->A) and needs to encode
those specific swaps directly against the Tycho router contract. The client's
`signable_payload()` and `execute()` patterns are useful as reference for the
alloy signing/broadcasting code.

- [ ] **Execution via tycho-execution + alloy**: Encode cycle swaps as calldata
  for the RouterV3 contract. Build EIP-1559 tx, sign with a local wallet, submit
  via Flashbots Protect (BuilderNet) for MEV protection. Parse receipt and compute
  settled amount (reuse pattern from `fynd-client/signing.rs`).
  New file: `executor.rs`
  New deps: `tycho-execution`, `alloy`, `reqwest` (for Flashbots RPC)

- [ ] **Gas accounting in out-token**: Convert gas cost (in ETH) to the source
  token. For WETH source this is 1:1. For non-WETH sources, needs a price quoter.
  Start with WETH-only gas accounting; generalize when multi-source lands.
  Dependency: Mini Price Quoter (for non-WETH sources, later)

- [ ] **Profitability threshold (--min-profit-bps)**: CLI flag, default 0.
  Only execute trades above this threshold. Useful for testing: set slightly
  negative to force trades through even at a small loss.

- [ ] **Slippage parameter (--slippage-bps)**: Reduce expected amounts by this
  many basis points when encoding swaps. Only send trades still profitable after
  slippage.

- [ ] **Execution options (--execution-mode)**: Protected submission via
  Flashbots Protect / BuilderNet (default), or public mempool for testing.

- [ ] **Dynamic bribe**: Bid a configurable % of expected profit as gas tip
  (--bribe-pct, default 100). On Ethereum this means bidding the surplus as
  priority fee.

### Phase 2: Make It a Real Reference Bot

- [ ] **Multi-source token support (--source-tokens)**: Run BF from WETH, USDC,
  WBTC, etc. Addresses the CRITICAL limitation of WETH-only cycles. Catches
  stablecoin triangles, WBTC cycles, and any cycle not touching WETH.

- [ ] **Gas safeguard (--max-gas-per-hour)**: Cap total gas spend per hour.
  Prevents a buggy bot from burning through the wallet.

- [ ] **Gas warning (--gas-warn-threshold)**: Notify when wallet ETH balance
  drops below threshold.

### Phase 3: Performance and Strategy

- [ ] **Pre-enumerated cycle files**: Johnson's enumeration algorithm as an
  alternative to real-time BF. Generate offline, order by historical profitability,
  hot-load without downtime. Second mode: `--mode cycle-file` vs `--mode realtime-bf`.
  ("pre-enumeration of cycles with soft simulation was the basic way to just be
  really fast.")

- [ ] **Cycle file management**: Index by affected pools. Prune pools below
  configurable TVL threshold. Regenerate periodically. Hot-load (double-buffer)
  without stopping the bot.

- [ ] **Cycle merging**: Phase 3 from Janos's algorithm. Shared edges between
  cycles save gas by batching them into a single transaction.

- [ ] **State overrides for repeated pools**: When the same pool appears in
  multiple cycles in the same block, re-simulate with updated state after each
  execution.

- [ ] **O(1) address lookup**: Build HashMap for Address-to-NodeIndex instead of
  O(V) linear scan per edge in GSS. (Code review item #4.)

### Phase 4: The Guide

Comprehensive documentation (the "guide" from the Searcher Showcase spec).
Written as the example's README or as a separate guide in `docs/`.

- [ ] **Chain-specific strategies**: Three classes:
  - Ethereum mainnet (PBS/auction): gas optimization, longer compute budget
  - Avalanche (FCFS): pure latency race, sub-1ms target
  - Polygon (randomized ordering): spam/lottery strategy
  How to adapt the bot for each. When to use which execution mode.

- [ ] **How to stabilize your own protocol**: For dapp teams wanting a backstop
  bot. Integrate protocol via Tycho SDK, adjust the template, share with
  searchers. Reference: Moo's self-built liquidation bot.

- [ ] **Latency reduction guide**: Self-host Tycho, float arithmetic (soft
  simulation), analytical solutions vs numerical (link to papers), on-chain
  routing (move calculation to contract, revert if unprofitable), parallel/async,
  native implementations for slow protocols, co-location with validators.

- [ ] **Cycle file management docs**: How to generate, order by profitability,
  hot-load, index by affected pools, prune low-TVL pools. Johnson's enumeration
  reference.

- [ ] **Protected execution options**: BuilderNet, Flashbots Protect, MEVBlocker,
  public mempool. Dynamic bribing. Builder refunds. Per-chain execution
  differences. Bundling.

- [ ] **How Tycho fits event-driven architectures**: Module that plugs into
  Artemis-style setups. Shared protocol implementations via Tycho.

- [ ] **Finding arbitrage on every path**: How to stay current with every Tycho
  protocol. Day-one arb opportunities when protocols self-integrate before launch.
  Strategic advantage for new searchers.

### Phase 5: Polish and Expand

- [ ] More protocol coverage (Curve, Balancer, UniV4)
- [ ] Base / Unichain testing
- [ ] Benchmark: cycles found per block over 1000+ blocks
- [ ] Biconnected component decomposition (parallelize independent subgraphs)
- [ ] Selective recalculation: only recheck cycles whose pools were touched in
  the latest block update. Index cycles by pool; filter to affected cycles per
  block.
- [ ] Trade monitoring: record pending trades, block pools in active trades,
  log outcomes (success/fail, amounts, gas, profit).

### Considerations (Not Planned)

- **Event-driven architecture (Artemis-style)**: This pattern is common across
  MEV teams. Fynd's and Tycho's existing architecture already provides the
  event-driven data flow (TychoFeed block updates). A full Artemis-style refactor
  (events/handlers/strategies) would be a larger architectural change. Worth
  revisiting if the example grows into a standalone framework, but not needed for
  the showcase.

---

## Before Merging (Current PR)

- [ ] PR #43 (BellmanFord solver) must merge first, or rebase this on top
- [ ] Fix 2 dead-code warnings (`CycleCandidate.layer`, `EvaluatedCycle.amount_out`)
- [ ] Push branch and open PR

---

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

4. O(V) Address-to-NodeIndex lookup per edge in GSS (should build HashMap) -> Phase 3
5. No state overrides for repeated pools (solver has this, example doesn't) -> Phase 3
6. net_profit uses i128 with saturation (should use BigInt) -> Phase 1 (gas accounting)
7. Distance arrays sized by full graph, not subgraph (memory optimization) -> Phase 5

## References

- Searcher Showcase spec: /Users/markusschmitt/Documents/notes/Notes/Searcher Showcase.md
- Simple Atomic Arbitrage Showcase spec: /Users/markusschmitt/Documents/notes/Notes/Simple Atomic Arbitrage Showcase.md
- Atomic Searcher spec (older): /Users/markusschmitt/Documents/notes/Notes/Atomic Searcher.md
- Atomic MEV strategy call (2025-03-03)
- Janos's tycho-searcher: https://github.com/jtapolcai/tycho-searcher
- Paper writeup: https://www.overleaf.com/read/ksqhzzmndmqh
- FC 2026 paper on arbitrage market saturation (accepted)
- Fynd PR #43 (BellmanFord solver): https://github.com/propeller-heads/fynd/pull/43
- Fynd PR #58 (fynd-client): https://github.com/propeller-heads/fynd/pull/58
- Notion design doc: https://www.notion.so/Routing-Algorithm-Design-20a2ed0857018035b34cc66c8d01ce6e
- Telegram chat: "Janos < > Propeller" (chat ID 4944590475)

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

5. **Direct execution (Option B)**: Use `tycho-execution` + `alloy` directly for
   encoding and submitting cycle trades, rather than going through `FyndClient`.
   The client assumes A->B solver orders; the searcher has its own pre-computed
   cycles and needs full control over which exact swaps get executed.
