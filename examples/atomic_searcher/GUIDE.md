# Atomic Searcher Operator Guide

Cyclic arbitrage bot built on Fynd/Tycho. Uses Bellman-Ford cycle detection and golden section search to find and (optionally) execute profitable token cycles every block.

This is a showcase/educational tool, not production MEV software.

---

## 1. Quick Start

```bash
# Prerequisites: Rust toolchain, Tycho API key
export TYCHO_API_KEY="your-key"

# Log-only mode on Ethereum (safest starting point)
cargo run --example atomic_searcher

# With verbose logging
RUST_LOG=atomic_searcher=debug,fynd=info cargo run --example atomic_searcher
```

The searcher will sync with Tycho, build a token graph, and start scanning every block. You will see output like:

```
INFO  Starting atomic searcher chain=Ethereum ...
INFO  Initial sync complete, starting search block=21234567 nodes=3142 edges=9234
INFO  block search complete block=21234568 candidates=380 profitable=1 time_ms=45
INFO  [PROFITABLE] #0: 0xc02aaa39.. -[0x88e6a0c2..]-> 0xa0b86991.. -[0xb4e16d0..]-> 0xc02aaa39.. | in: 0.4200 ETH | gross: 0.003342 ETH | gas: 0.003200 ETH | net: 0.000142 ETH | hops: 2
INFO  [GROSS+] #1: ...
```

---

## 2. Configuration Reference

Every flag, its type, default, and what it does.

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `TYCHO_API_KEY` | Authentication key for the Tycho WebSocket feed. |

### Optional Environment Variables

| Variable | Description |
|----------|-------------|
| `RPC_URL` | Ethereum/Base/Unichain RPC endpoint. Required for simulation, execution, gas price fetching, and RPC verification. Without this, gas prices default to 30 gwei and non-WETH source tokens show `gas_cost=0`. |
| `PRIVATE_KEY` | Hex-encoded private key (with or without `0x` prefix). Required for `execute-public`, `execute-protected`, and `execute-flash` modes. |
| `FLASH_ARB_ADDRESS` | Address of deployed FlashArbExecutor contract. Required for `simulate-flash` and `execute-flash` modes. |
| `RUST_LOG` | Controls log verbosity. Default: `atomic_searcher=info,fynd=info`. Set to `atomic_searcher=debug` for GSS details and subgraph stats. |

### CLI Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--chain` | string | `ethereum` | Blockchain to search on. Supported: `ethereum`, `base`, `unichain`. |
| `--tycho-url` | string | `tycho-beta.propellerheads.xyz` | Tycho WebSocket endpoint (hostname only, no `wss://`). |
| `--protocols` | string | `uniswap_v2,uniswap_v3,sushiswap_v2` | Comma-separated list of protocols to index. |
| `--min-tvl` | f64 | `10.0` | Minimum pool TVL in native token units. Pools below this are filtered out. |
| `--max-hops` | usize | `4` | Maximum edges in a cycle (not counting the closing edge back to source). Controls BFS depth for subgraph extraction. |
| `--seed-eth` | f64 | `0.001` | Seed amount in ETH for the Bellman-Ford scan. Smaller values find more candidates because less price impact masks mispricings. GSS independently optimizes the actual trade amount for each candidate. |
| `--source-tokens` | string | (none; defaults to WETH) | Source token addresses, comma-separated hex. Each token gets its own BF scan per block. Use `all` for WETH + USDC + USDT + DAI + WBTC. |
| `--gss-tolerance` | f64 | `0.001` | Golden section search convergence tolerance (relative). |
| `--gss-max-iter` | usize | `30` | Maximum GSS iterations per candidate cycle. |
| `--blacklist` | string | `blacklist.toml` | Path to pool blacklist file (TOML). Pools listed are excluded from the graph. |
| `--blacklist-tokens` | string | `0xd46ba6d942050d489dbd938a2c909a5d5039a161` | Comma-separated token addresses to exclude. Default excludes AMPL (rebase token with broken simulations). |
| `--min-profit-bps` | i64 | `0` | Minimum net profit in basis points of `amount_in` to attempt execution. Cycles below this threshold are logged but not executed. |
| `--slippage-bps` | u32 | `50` | Slippage tolerance in basis points for encoding `checked_amount`. The minimum output is `amount_out * (10000 - slippage_bps) / 10000`. |
| `--bribe-pct` | u32 | `100` | Priority fee multiplier as a percentage of network-suggested `maxPriorityFeePerGas`. 100 = suggested fee, 200 = 2x (aggressive), 50 = 0.5x (passive). |
| `--execution-mode` | string | `log-only` | How to handle profitable cycles. See Section 4 for details. |
| `--verify-top-n` | usize | `0` | Number of top candidates to verify via RPC (`eth_call` + `estimate_gas`) before execution. 0 = disabled. When > 0, simulates the top N gross-positive cycles and picks the first that passes on-chain simulation. |
| `--force-execute` | bool | `false` | Force execution on the best gross-positive cycle even if net-unprofitable after gas. Useful for testing the encoding/execution pipeline. |
| `--private-key` | string | (none) | Same as `PRIVATE_KEY` env var. CLI flag takes precedence. |
| `--rpc-url` | string | (none) | Same as `RPC_URL` env var. CLI flag takes precedence. |
| `--flash-arb-address` | string | (none) | Same as `FLASH_ARB_ADDRESS` env var. CLI flag takes precedence. |
| `--deploy-flash-arb` | bool | `false` | Deploy a new FlashArbExecutor contract and print the address. Requires `--rpc-url` and `--private-key`. Exits after deployment. |

---

## 3. Chain Setup

### Ethereum

```bash
export TYCHO_API_KEY="your-key"
export RPC_URL="https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"

cargo run --example atomic_searcher -- \
  --chain ethereum \
  --tycho-url tycho-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4,sushiswap_v2,pancakeswap_v2,pancakeswap_v3,vm:balancer_v2,vm:curve,vm:maverick_v2,fluid_v1,ekubo_v2
```

Available Ethereum protocols: `uniswap_v2`, `uniswap_v3`, `uniswap_v4`, `uniswap_v4_hooks`, `vm:balancer_v2`, `vm:curve`, `sushiswap_v2`, `pancakeswap_v2`, `pancakeswap_v3`, `ekubo_v2`, `vm:maverick_v2`, `fluid_v1`. Also listed but not yet in the SDK: `cowamm`, `ekubo_v3`.

### Base

```bash
export TYCHO_API_KEY="your-key"
export RPC_URL="https://base-mainnet.g.alchemy.com/v2/YOUR_KEY"

cargo run --example atomic_searcher -- \
  --chain base \
  --tycho-url tycho-base-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4,pancakeswap_v3,aerodrome_slipstreams
```

Note: `sushiswap_v2` is NOT available on Base. The default `--protocols` value includes it, so you must override protocols explicitly.

### Unichain

```bash
export TYCHO_API_KEY="your-key"
export RPC_URL="https://unichain-mainnet.g.alchemy.com/v2/YOUR_KEY"

cargo run --example atomic_searcher -- \
  --chain unichain \
  --tycho-url tycho-unichain-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4
```

### Tycho Endpoints Summary

| Chain | Endpoint |
|-------|----------|
| Ethereum | `tycho-beta.propellerheads.xyz` |
| Base | `tycho-base-beta.propellerheads.xyz` |
| Unichain | `tycho-unichain-beta.propellerheads.xyz` |

---

## 4. Execution Modes

Set via `--execution-mode`. Modes escalate from safe to real money at risk.

### `log-only` (default)

No on-chain interaction. Finds and logs cycles only. Safe for monitoring and tuning parameters.

```bash
cargo run --example atomic_searcher
```

### `simulate`

Encodes the cycle into calldata via TychoRouter, then simulates via `eth_simulate`. Sends two simulated transactions (ERC-20 approval + swap). Uses state overrides to give the sender 1000 ETH so the simulation works regardless of actual balance.

Requires: `RPC_URL`. Optional: `PRIVATE_KEY` (uses `Address::ZERO` if missing).

```bash
cargo run --example atomic_searcher -- \
  --execution-mode simulate
```

### `execute-public`

Signs and sends both the approval and swap transactions to the public mempool. Your transaction is visible to everyone, including MEV searchers who may frontrun you.

Requires: `RPC_URL`, `PRIVATE_KEY`. The signing wallet must hold the source token (e.g., WETH) and ETH for gas.

```bash
cargo run --example atomic_searcher -- \
  --execution-mode execute-public
```

### `execute-protected`

Signs the swap transaction locally and submits it to Flashbots Protect (`https://rpc.flashbots.net`) instead of the public mempool. This prevents frontrunning. Sends only the swap transaction (no approval); the bot must have a pre-existing persistent approval to the TychoRouter.

Requires: `RPC_URL`, `PRIVATE_KEY`. Ethereum only.

```bash
cargo run --example atomic_searcher -- \
  --execution-mode execute-protected
```

### `simulate-flash`

Encodes the cycle as a flash loan transaction through the FlashArbExecutor contract, then simulates via `eth_call`. Zero upfront capital required. Auto-selects between Tier 1 (UniV2 flash swap, near-zero extra gas) and Tier 2 (Balancer V2 flash loan, ~100k extra gas overhead).

Requires: `RPC_URL`, `FLASH_ARB_ADDRESS`. Optional: `PRIVATE_KEY`.

```bash
cargo run --example atomic_searcher -- \
  --execution-mode simulate-flash \
  --flash-arb-address 0x1C62E62a6e6D604B0743870B20cc5921155eDD52
```

### `execute-flash`

Signs and sends a real flash loan transaction. The FlashArbExecutor borrows tokens, executes the cycle via TychoRouter, repays the loan, and sweeps profit to the contract owner.

Requires: `RPC_URL`, `PRIVATE_KEY`, `FLASH_ARB_ADDRESS`. The signer must be the contract owner and hold ETH for gas (no source token balance needed).

```bash
cargo run --example atomic_searcher -- \
  --execution-mode execute-flash \
  --flash-arb-address 0x1C62E62a6e6D604B0743870B20cc5921155eDD52
```

### Flash Loan Tier Selection

The searcher automatically picks the best flash strategy for each cycle:

- **Tier 1 (UniV2 Flash Swap)**: Used when the first pool in the cycle is `uniswap_v2` or `sushiswap_v2`. Borrows the intermediate token from the pair itself, executes remaining hops via TychoRouter, then repays. Near-zero extra gas overhead.
- **Tier 2 (Balancer V2 Flash Loan)**: Fallback for all other routes. Borrows the source token from Balancer Vault, executes all hops, repays principal + fee. Adds ~80-100k gas overhead.

### Deploying FlashArbExecutor

Flash modes require a deployed FlashArbExecutor contract. Deploy once per chain:

```bash
cargo run --release --example atomic_searcher -- \
  --deploy-flash-arb \
  --rpc-url "https://base-mainnet.g.alchemy.com/v2/YOUR_KEY" \
  --private-key "0x..."
```

This deploys the contract with the Balancer V2 Vault as the flash loan provider, prints the contract address, and exits. Save the address for subsequent runs:

```bash
export FLASH_ARB_ADDRESS="0x..."  # printed by --deploy-flash-arb
```

The deploying wallet becomes the contract owner and the only address that can call `executeFlashSwapV2` / `executeFlashLoan` / `rescueTokens`.

### Recommended: execute-flash

`execute-flash` is the recommended execution mode. It requires zero token capital (only gas ETH), handles approvals atomically inside the contract, and avoids the per-trade approval overhead of `execute-public` / `execute-protected`.

```bash
cargo run --release --example atomic_searcher -- \
  --chain base \
  --tycho-url tycho-base-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3 \
  --execution-mode execute-flash \
  --flash-arb-address $FLASH_ARB_ADDRESS \
  --rpc-url $RPC_URL
```

### Mode Requirements Summary

| Mode | RPC_URL | PRIVATE_KEY | FLASH_ARB_ADDRESS | Real Funds |
|------|---------|-------------|-------------------|------------|
| `log-only` | -- | -- | -- | No |
| `simulate` | Required | Optional | -- | No |
| `execute-public` | Required | Required | -- | Yes |
| `execute-protected` | Required | Required | -- | Yes |
| `simulate-flash` | Required | Optional | Required | No |
| `execute-flash` | Required | Required | Required | Gas only |

---

## 5. Understanding Output

### Log Line Anatomy

Each block produces a summary line and up to 5 detail lines for the top cycles:

```
INFO  block search complete block=21234568 candidates=380 profitable=1 time_ms=45
```

- `candidates`: Number of cycles found by Bellman-Ford (before GSS optimization).
- `profitable`: Number of cycles where `net_profit > 0` after gas.
- `time_ms`: Total wall-clock time for all BF scans + GSS optimizations this block.

### Cycle Detail Lines

```
INFO  [PROFITABLE] #0: 0xc02aaa39.. -[0x88e6a0c2..]-> 0xa0b86991.. -[0xb4e16d0..]-> 0xc02aaa39.. | in: 0.4200 ETH | gross: 0.003342 ETH | gas: 0.003200 ETH | net: 0.000142 ETH | hops: 2
INFO    pools: ["0x88e6a0c2bdd44b...", "0xb4e16d0168e52d..."]
```

- **Status prefix**: `PROFITABLE` (net > 0), `GROSS+` (gross > 0 but net < 0 after gas), `no-arb` (gross <= 0).
- **Path**: Shows token address prefixes and pool ID prefixes for each hop.
- **in**: Optimal input amount found by GSS.
- **gross**: `amount_out - amount_in` (before gas).
- **gas**: Estimated gas cost converted to source token units.
- **net**: `gross - gas`. This is what you would actually earn.
- **hops**: Number of edges in the cycle.

For the top cycle (#0), full token and pool addresses are printed on separate lines for investigation.

### What the Numbers Mean in Practice

On Ethereum with proper gas accounting (~380 candidates/block is typical):
- Most candidates are `GROSS+` but not net-profitable after gas.
- Occasional `PROFITABLE` cycles appear, typically on 2-hop cycles with ~0.0001 ETH net profit.
- `no-arb` cycles are filtered out and not displayed.

---

## 6. Advanced Features

### Multi-Source Token Search

By default, the searcher only scans from WETH. Different source tokens anchor different liquidity clusters, so scanning from multiple tokens finds more opportunities.

```bash
# Scan from all 5 default tokens (WETH, USDC, USDT, DAI, WBTC)
cargo run --example atomic_searcher -- --source-tokens all

# Scan from specific tokens
cargo run --example atomic_searcher -- \
  --source-tokens 0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2,0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48
```

Each source token gets its own parallel BF scan per block. Results are merged and sorted by net profit across all sources.

The `all` shortcut expands to these Ethereum addresses:
- `0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2` (WETH)
- `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` (USDC)
- `0xdac17f958d2ee523a2206206994597c13d831ec7` (USDT)
- `0x6b175474e89094c44da98b954eedeac495271d0f` (DAI)
- `0x2260fac5e5542a773aa44fbcfedf7c193bc2c599` (WBTC)

### RPC Verification

Use `--verify-top-n` to pre-simulate candidates via RPC before execution. This filters out cycles where Tycho's off-chain simulation disagrees with on-chain state.

```bash
cargo run --example atomic_searcher -- \
  --execution-mode execute-flash \
  --flash-arb-address 0x1C62... \
  --verify-top-n 3
```

This simulates the top 3 gross-positive cycles via `eth_call` and picks the first that passes. Useful when Tycho estimates are optimistic due to stale state.

### Gas Price Fetcher

When `RPC_URL` is set, the searcher spawns a background gas price fetcher that polls the RPC for current gas prices. This provides accurate gas cost estimates for all source tokens.

Without `RPC_URL`, gas prices default to 30 gwei and non-WETH source tokens will show `gas_cost=0` because there is no token price oracle to convert ETH gas costs into the source token's denomination.

### Force Execute (Testing)

The `--force-execute` flag sends the best gross-positive cycle even if it is net-unprofitable. This is for testing the encoding and execution pipeline on mainnet where all arb is extracted and no cycle covers gas costs.

```bash
cargo run --example atomic_searcher -- \
  --execution-mode simulate \
  --force-execute
```

### Pool and Token Blacklists

**Pool blacklist** (`--blacklist`): A TOML file listing pool/component IDs to exclude from the graph entirely. Useful for known-broken pools.

**Token blacklist** (`--blacklist-tokens`): Comma-separated token addresses. Any cycle passing through a blacklisted token is filtered out. The default excludes AMPL (`0xd46ba6d942050d489dbd938a2c909a5d5039a161`), a rebase token with broken simulations.

```bash
cargo run --example atomic_searcher -- \
  --blacklist-tokens 0xd46ba6d942050d489dbd938a2c909a5d5039a161,0x1234...
```

### Tuning the Seed Amount

The `--seed-eth` parameter controls the initial BF scan amount. The tradeoff:

- **Smaller seed** (e.g. 0.001 ETH): Finds more candidates because small trades cause less price impact, revealing mispricings that large trades would mask. More compute per block.
- **Larger seed** (e.g. 1.0 ETH): Fewer false positives, faster per block, but misses opportunities that only appear at smaller sizes.

The seed does not affect the final trade size. GSS independently optimizes the actual input amount for each candidate found, searching up to ~1000 ETH.

---

## 7. Troubleshooting

### "blacklist not loaded, continuing without"

Not an error. If `blacklist.toml` does not exist in the working directory, the searcher proceeds without a pool blacklist. Create one if you need to exclude specific pools.

### "RPC_URL not set, gas price fetcher disabled"

The searcher runs fine without `RPC_URL` for log-only mode, but gas cost estimates will be inaccurate. Set `RPC_URL` for realistic profit calculations.

### "no valid source tokens parsed, falling back to WETH"

The `--source-tokens` value could not be parsed as hex addresses. Check formatting: addresses should be `0x`-prefixed, comma-separated, no spaces.

### "Missed N events (searcher too slow)"

The searcher's BF scan + GSS optimization took longer than the block time. Reduce `--max-hops`, reduce the number of `--protocols`, or increase `--min-tvl` to shrink the graph.

### "flash-arb-address required for flash mode"

You used `--execution-mode simulate-flash` or `execute-flash` without providing `--flash-arb-address` or `FLASH_ARB_ADDRESS`.

### "RPC_URL and PRIVATE_KEY required for execution"

Execution modes (`execute-public`, `execute-protected`, `execute-flash`) need both `RPC_URL` and `PRIVATE_KEY`. Simulation modes need at least `RPC_URL`.

### "swap encoder registry" or "encoder build" errors

The cycle contains a pool from a protocol that does not have a swap encoder registered in `tycho-execution`. This can happen with newer or unsupported protocol types.

### Simulation passes but execution reverts

Common causes:
- State changed between simulation and execution (someone else took the arb).
- Slippage tolerance too tight. Increase `--slippage-bps`.
- Gas estimate too low. The searcher adds a 20% buffer, but complex routes may need more.

### No profitable cycles found

This is normal. On Ethereum mainnet, professional MEV searchers extract most cyclic arb within milliseconds. The searcher will consistently find `GROSS+` cycles (profitable before gas) but rarely `PROFITABLE` ones (profitable after gas). This is expected behavior, not a bug.

---

## 8. Known Limitations

- **Not production MEV software.** This is a showcase/educational tool. Professional searchers use private transaction ordering, latency optimization, and custom smart contracts.
- **No MEV-Share integration.** The `execute-protected` mode uses Flashbots Protect (private mempool) but does not participate in MEV-Share auctions.
- **No cycle merging.** Cycles that share edges could share gas costs, but this optimization is not implemented.
- **Single-block horizon.** The searcher evaluates each block independently with no state prediction or multi-block strategies.
- **Flash Tier 1 availability varies by chain.** UniV2 flash swaps require the first pool in the cycle to be `uniswap_v2` or `sushiswap_v2`. On Base (no `sushiswap_v2`), Tier 1 only works when the first pool is `uniswap_v2`. Tier 2 (Balancer V2 flash loan) is available on Ethereum, Base, and most other chains where the Balancer V2 Vault is deployed (`0xBA12222222228d8Ba445958a75a0704d566BF2C8`).
- **GSS precision limit.** The golden section search caps at ~1000 ETH input to avoid f64 precision loss. Cycles requiring larger inputs are capped.
- **The `all` shortcut for `--source-tokens` uses hardcoded Ethereum addresses.** On Base or Unichain, specify token addresses explicitly.
- **Chain ID fallback.** Unsupported chains silently fall back to Ethereum mainnet (chain ID 1) for transaction encoding.
