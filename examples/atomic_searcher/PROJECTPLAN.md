# Atomic Searcher Showcase - Project Plan

## Status: PHASE 1.5 COMPLETE

Last updated: 2026-03-13
Branch: `ms/atomic-searcher-showcase`
Repo: `/Users/markusschmitt/Documents/GitHub/fynd`
PR: https://github.com/propeller-heads/fynd/pull/78
Note: Will be moved to its own repository (per Alan's feedback, fynd should stay focused on core library).

## Condition for Completion

An example that runs and settles trades on all chains we support, and a guide that
gets team approval. (from Searcher Showcase spec)

## What Was Built

### Phase 0: Core Algorithm (DONE)

Cyclic arbitrage searcher using Bellman-Ford cycle detection + golden section search.

```
examples/atomic_searcher/
  main.rs              - CLI, Tycho feed setup, per-block search loop
  cycle_detector.rs    - Layered BF for cycle detection + tests
  amount_optimizer.rs  - Golden section search + tests
  executor.rs          - Encoding + simulation + execution via TychoRouter
  flash_loan.rs        - Flash loan ABI, encoding, FlashTier enum
  types.rs             - CycleCandidate, EvaluatedCycle, BlockSearchResult
  contracts/
    FlashArbExecutor.sol     - Solidity: flash swap + flash loan
    FlashArbExecutor.bytecode - Compiled bytecode for deployment
  README.md            - Algorithm docs, usage, references
  TEST_PLAN.md         - Live execution test plan
  PROJECTPLAN.md       - This file
```

### Phase 1: Execution Pipeline (DONE)

Full end-to-end execution: encode -> simulate -> execute on mainnet.

**Completed items:**
- [x] Execution via tycho-execution + alloy (executor.rs)
- [x] Four execution modes: log-only, simulate, execute-public, execute-protected (Flashbots)
- [x] Gas accounting in WETH (1:1 for WETH source)
- [x] Profitability threshold (--min-profit-bps)
- [x] Slippage parameter (--slippage-bps)
- [x] Dynamic bribe (--bribe-pct, scales priority fee)
- [x] --force-execute flag for testing pipeline on non-profitable cycles
- [x] State overrides for simulation (1000 ETH balance override)
- [x] 2x base fee for EIP-1559 volatility handling

**Live test results (2026-03-10):**
- 3 successful mainnet cyclic arb swaps via public mempool
- Tx hashes: 0x092151..., 0xbc48f2..., 0x02e79c...
- Approval + swap both pass (212k gas total per cycle)
- Gross-profitable (WETH out > in), net-unprofitable after gas (expected on efficiently-arbed mainnet)

**Bug found:** Fluid Lite simulation mispricing (ENG-5696). Two Fluid Lite pools
(`0x32aa6f5c...`, `0x7f31b44f...`) produce 50-100x overstated outputs. Blacklisted
in blacklist.toml. Reported to Flo/Zach in #p-tycho.

**Simulation fixes committed:**
- 2x base fee in max_fee_per_gas (EIP-1559 volatility across search latency)
- State overrides giving sender 1000 ETH for simulation
- Disabled tx validation in simulation mode

---

### Phase 1.5: Flash Loans - Capital-Free Execution (DONE)

Zero-capital arbitrage via tiered flash strategy. Single atomic tx: borrow, swap, repay, keep profit.

**Architecture:** FlashArbExecutor contract + tiered Rust encoding.

```
examples/atomic_searcher/
  flash_loan.rs          - ABI, encode functions, FlashTier enum, bytecode
  contracts/
    FlashArbExecutor.sol     - Solidity (both tiers)
    FlashArbExecutor.bytecode - Compiled bytecode
```

**Two tiers, auto-selected per cycle:**
- **Tier 1: UniV2 flash swap** - When first pool is UniV2/SushiV2. Near-zero extra gas (callback overhead only). Skips first hop in TychoRouter calldata.
- **Tier 2: Balancer V2 flash loan** - Fallback for any route. ~80-100k extra gas overhead. 0% fee (governance-controlled).

**Contract:** `0x1C62E62a6e6D604B0743870B20cc5921155eDD52` on Ethereum mainnet.
- Deploy tx: `0x05695c6a87e15ee8d99b4fc9a2213936fe11e9f838052098dce703c8fd8f6963`
- Owner: `0xFEa8eAfEB242360627C41AcB1F5Fda247DEA163E`

**Completed items:**
- [x] FlashArbExecutor.sol with two-phase callback guard (_expectedCaller)
- [x] Tier 1: executeFlashSwapV2 + uniswapV2Call callback
- [x] Tier 2: executeFlashLoan + receiveFlashLoan callback (approve+TransferFrom pattern)
- [x] flash_loan.rs with ABI, encoding, FlashTier enum
- [x] select_flash_tier() auto-selects based on first pool's protocol
- [x] build_solution_flash_v2() for hops 2..N (Tier 1)
- [x] SimulateFlash / ExecuteFlash execution modes
- [x] CLI: --flash-arb-address, simulate-flash, execute-flash
- [x] Contract deployed and verified on mainnet

**Live results (2026-03-13):**
- Multiple successful eth_call simulations with Tier 2 (Balancer), gas: 360k-500k
- Tier selection correctly identifies V3 first pools and falls back to Tier 2
- Live mainnet tx: `0xaa27df37fb8c981b1a245bbbaf8d9a54c8e7c0540f6a858b2e8b48973d907fe1` (block 24651066)
  - Reverted as expected (force-execute on net-unprofitable cycle, used for pipeline validation)
  - Gas used: 439,285. Full encode -> sign -> send -> on-chain pipeline verified.

**Key design decisions:**
1. Approve+TransferFrom pattern (not direct transfer) to avoid TychoRouter underflow
2. Two-phase _expectedCaller guard instead of reentrancy lock (callbacks need to re-enter)
3. Full balance sweep for profit extraction (simpler than remainder math)
4. 1 wei safety margin on Tier 1 repay amount (K-invariant rounding)

---

## Feature Backlog

### Phase 2: Make It a Real Reference Bot

- [ ] **Multi-source token support (--source-tokens)**: Run BF from WETH, USDC,
  WBTC, etc. Catches stablecoin triangles, WBTC cycles.
- [ ] **Gas safeguard (--max-gas-per-hour)**: Cap total gas spend per hour.
- [ ] **Gas warning (--gas-warn-threshold)**: Notify when wallet ETH balance low.

### Phase 3: Performance and Strategy

- [ ] **Pre-enumerated cycle files**: Johnson's enumeration as alternative to real-time BF.
- [ ] **Cycle merging**: Shared edges between cycles save gas via batching.
- [ ] **State overrides for repeated pools**: Re-simulate with updated state.
- [ ] **O(1) address lookup**: HashMap for Address-to-NodeIndex.

### Phase 4: The Guide

- [ ] Chain-specific strategies (Ethereum PBS, Avalanche FCFS, Polygon random)
- [ ] How to stabilize your own protocol
- [ ] Latency reduction guide
- [ ] Protected execution options docs

### Phase 5: Polish and Expand

- [ ] More protocol coverage (Curve, Balancer, UniV4) - partially done (13 protocols tested)
- [ ] Base / Unichain testing
- [ ] Benchmark: cycles found per block over 1000+ blocks
- [ ] Selective recalculation: only recheck cycles whose pools were touched

### Considerations (Not Planned)

- **Event-driven architecture (Artemis-style)**: Worth revisiting if moved to standalone repo.
- **Separate repo**: Alan's feedback: fynd should stay focused on core library. This
  example will move to its own repo. Current PR merges for pipeline validation.

---

## Commits on Branch

```
2b010be Add --force-execute flag for testing encoding on non-profitable cycles
c0c4ab9 Blacklist broken Fluid Lite pools, add per-hop debug logging
5c4c2fb Fix simulation pipeline for atomic searcher live testing
0c01353 docs: add project plan and review notes for atomic searcher
425ea7f test: add 11 unit tests for cycle detector and amount optimizer
9470f93 fix: address review findings from extreme code review
7545330 improve: show gross profit and cycle status labels in output
bb4310a fix: add blacklist support and token-level filtering to atomic searcher
f2a0ee7 feat: add atomic searcher showcase example
(+ earlier execution commits)
```

## Algorithm

1. Subscribe to Tycho market updates via Fynd's TychoFeed
2. On each block, extract subgraph around source token (BFS to max_hops),
   excluding blacklisted tokens from the market graph before search begins
3. Run layered Bellman-Ford relaxation, check edges back to source for cycles
4. For each candidate, run golden section search to find optimal input amount
5. Encode via TychoRouter, simulate or execute on-chain

Based on: https://github.com/jtapolcai/tycho-searcher
Paper: https://www.overleaf.com/read/ksqhzzmndmqh

## Testing Summary

### Unit Tests (13, all passing)

Cycle detector (6): simple_cycle, no_cycle_unprofitable, empty_graph, multi_hop,
subgraph_depth, dedup_duplicates

Amount optimizer (5): gss_optimal, gss_unprofitable, gss_small_seed, evaluate_correct,
missing_nodes_none

Flash loan (2): encode_flash_loan_call_valid, encode_flash_swap_v2_call_valid

### Live Testing (Ethereum mainnet)

- 13 protocols, 3242 nodes, 8786 edges
- 4-93 BF candidates per block
- Encoding validated: approval (46k gas) + swap (166k gas) pass eth_simulate
- 3 successful mainnet trades via public mempool
- Fluid Lite simulation bug found and reported (ENG-5696)

## Key Design Decisions

1. **Example, not library feature**: Searchers are autonomous agents.
2. **Reuse BF from PR #43**: Adapted solver's BF for cycle detection.
3. **GSS in f64 space**: Acceptable for showcase. Capped at 1000 ETH.
4. **Token-level blacklist**: Pool-level insufficient for rebase tokens.
5. **Direct execution**: tycho-execution + alloy directly, not FyndClient.
6. **Will move to separate repo**: Per Alan's feedback.
7. **Approve+TransferFrom for flash loans**: Direct transfer causes TychoRouter underflow in _verifyAmountOutWasReceived. Contract approves router, router pulls via transferFrom.
8. **Tiered flash strategy**: V2 flash swap (near-zero gas) preferred, Balancer fallback for all routes. Auto-selected per cycle.

## How to Resume

```bash
cd /Users/markusschmitt/Documents/GitHub/fynd
git checkout ms/atomic-searcher-showcase

# Run tests
cargo test --example atomic_searcher

# Run live (simulate mode, requires capital)
RUST_LOG="atomic_searcher=debug,fynd=info" cargo run --example atomic_searcher -- \
  --execution-mode simulate --force-execute \
  --private-key "$PRIVATE_KEY" --rpc-url "$RPC_URL"

# Run live (flash loan mode, zero capital)
RUST_LOG="atomic_searcher=debug,fynd=info" cargo run --example atomic_searcher -- \
  --execution-mode simulate-flash --force-execute \
  --flash-arb-address "0x1C62E62a6e6D604B0743870B20cc5921155eDD52" \
  --private-key "$PRIVATE_KEY" --rpc-url "$RPC_URL"

# Execute flash arb for real (sends mainnet tx)
# --execution-mode execute-flash (same args as above)
```

## References

- PR: https://github.com/propeller-heads/fynd/pull/78
- Fluid Lite bug: https://propeller-heads.atlassian.net/browse/ENG-5696
- Janos's tycho-searcher: https://github.com/jtapolcai/tycho-searcher
- Paper: https://www.overleaf.com/read/ksqhzzmndmqh
- Fynd PR #43 (BellmanFord solver): https://github.com/propeller-heads/fynd/pull/43
