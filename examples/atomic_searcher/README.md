# Atomic Searcher Showcase

Autonomous cyclic arbitrage searcher built on Fynd's market data infrastructure.

Based on [Janos Tapolcai's simulation-based Bellman-Ford arbitrage searcher](https://github.com/jtapolcai/tycho-searcher).

## What it does

Unlike Fynd's solver (which finds A-to-B routes for incoming orders), the atomic searcher **proactively scans** for profitable arbitrage cycles:

```
WETH -> Token_1 -> Token_2 -> ... -> WETH
```

If it ends up with more WETH than it started with (after gas), the cycle is profitable.

## Algorithm

1. **Subscribe** to Tycho market updates via Fynd's `TychoFeed`
2. **Extract subgraph** around WETH (BFS to `max_hops` depth)
3. **Bellman-Ford cycle detection**: Layered BF relaxation with SPFA optimization, calling `get_amount_out()` on actual pool simulations. After relaxation, check all edges pointing back to the source for cycles where `amount_out > amount_in`
4. **Golden section search**: For each candidate cycle, optimize the input amount to maximize `profit = amount_out - amount_in - gas_cost`
5. **Log results**: Print all cycles with path, optimal amount, profit, and gas cost

### Solver vs Searcher

| | Solver (PR #43) | Atomic Searcher |
|---|---|---|
| **Trigger** | External order (token_in, token_out, amount) | Autonomous, every block |
| **Objective** | Maximize `amount_out - gas` for A to B | Maximize `amount_out - amount_in - gas` for A to A |
| **Input amount** | Fixed (from order) | Optimized via golden section search |
| **Output** | Single best route | All profitable cycles |

## Usage

```bash
# Set your Tycho API key
export TYCHO_API_KEY="your-key"

# Run with defaults (Ethereum, WETH, 4 hops, 1 ETH seed)
cargo run --example atomic_searcher

# Customize
cargo run --example atomic_searcher -- \
  --chain ethereum \
  --tycho-url tycho-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3,sushiswap_v2 \
  --max-hops 4 \
  --seed-eth 1.0 \
  --min-tvl 10.0

# Verbose logging
RUST_LOG=atomic_searcher=debug,fynd=info cargo run --example atomic_searcher
```

## Output

```
INFO  Starting atomic searcher chain=Ethereum source_token=c02a...6cc2 max_hops=4
INFO  Initial sync complete, starting search block=21234567 nodes=1842 edges=9234
INFO  block search complete block=21234568 candidates=3 profitable=1 time_ms=12
INFO  [PROFITABLE] #0: c02a.. -> 6b17.. -> c02a.. | optimal_in: 0.4200 ETH | net_profit: 0.000142 ETH | gas: 0.003200 ETH | hops: 3
INFO  [unprofitable] #1: c02a.. -> a0b8.. -> c02a.. | optimal_in: 0.1500 ETH | net_profit: -0.001200 ETH | gas: 0.004100 ETH | hops: 2
```

## Architecture

```
examples/atomic_searcher/
  main.rs              Entry point, CLI, Tycho feed, block loop
  cycle_detector.rs    Modified BF for cycle detection
  amount_optimizer.rs  Golden section search for optimal trade size
  types.rs             CycleCandidate, EvaluatedCycle, BlockSearchResult
```

The example reuses Fynd library components (`TychoFeed`, `SharedMarketData`, `PetgraphStableDiGraphManager`) but implements its own search loop since it is not responding to external orders.

## References

- Janos Tapolcai's original implementation: https://github.com/jtapolcai/tycho-searcher
- Fynd BellmanFord solver (A-to-B routing): `src/algorithm/bellman_ford.rs`
- Paper: https://www.overleaf.com/read/ksqhzzmndmqh

## Limitations

This is a **showcase/educational tool**, not production MEV software.

- No on-chain execution (detection and logging only)
- No cycle merging (shared-edge gas optimization, planned as follow-up)
- No flashbots/MEV-Share integration
- In current saturated markets, profitable cycles are rare (see Tapolcai et al., FC 2026)
