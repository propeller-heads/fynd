# Quickstart: Quote, Simulate & Execute Swaps

This example demonstrates how to interact with an already-running tycho-router solver to get swap quotes, and optionally simulate or execute those swaps on-chain using tycho-execution.

## Prerequisites

1. **Running solver**: Start the tycho-router solver first:
   ```bash
   cargo run --example solver
   ```

2. **Tycho API key**: Required for loading token data from the Tycho indexer

3. **RPC URL**: Required for simulation and execution (Ethereum mainnet or other supported chain)

4. **Private key**: Required only for simulation/execution (not needed for quotes)

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `TYCHO_URL` | No | Tycho indexer URL (defaults to tycho-beta.propellerheads.xyz) |
| `TYCHO_API_KEY` | Yes | API key for Tycho indexer |
| `RPC_URL` | For sim/exec | Ethereum RPC endpoint |
| `PRIVATE_KEY` | For sim/exec | Wallet private key (hex, no 0x prefix) |
| `TENDERLY_ACCESS_KEY` | For Tenderly | Tenderly API access key |
| `TENDERLY_ACCOUNT` | For Tenderly | Tenderly account slug |
| `TENDERLY_PROJECT` | For Tenderly | Tenderly project slug |

## Quick Start

### 1. Start the solver

In one terminal:
```bash
export TYCHO_URL=tycho-beta.propellerheads.xyz
export TYCHO_API_KEY=your_api_key
export RPC_URL=https://eth-mainnet.g.alchemy.com/v2/your_key
cargo run --example solver
```

Wait for "Solver is ready and accepting requests".

### 2. Get a quote (no private key needed)

In another terminal:
```bash
export TYCHO_URL=tycho-beta.propellerheads.xyz
export TYCHO_API_KEY=your_api_key
cargo run --example quickstart -- \
  --sell-token 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount 100
```

This will display a quote for swapping 100 USDC to WETH.

### 3. Simulate a swap

Add your private key and RPC URL:
```bash
export PRIVATE_KEY=your_private_key_hex
export RPC_URL=https://eth-mainnet.g.alchemy.com/v2/your_key
cargo run --example quickstart -- \
  --sell-amount 100 \
  --simulate-only
```

### 4. Simulate with Tenderly

```bash
export TENDERLY_ACCESS_KEY=your_tenderly_key
export TENDERLY_ACCOUNT=your_account
export TENDERLY_PROJECT=your_project
cargo run --example quickstart -- \
  --sell-amount 100 \
  --simulate-only \
  --use-tenderly
```

### 5. Execute a swap (mainnet!)

```bash
cargo run --example quickstart -- --sell-amount 10
```

You'll be prompted to choose: Simulate, Execute, or Cancel.

## CLI Reference

| Flag | Default | Description |
|------|---------|-------------|
| `--sell-token` | USDC | Token address to sell |
| `--buy-token` | WETH | Token address to buy |
| `--sell-amount` | 10.0 | Amount to sell (human readable) |
| `--chain` | ethereum | Blockchain (ethereum, base, unichain) |
| `--solver-url` | http://localhost:3000 | Solver API URL |
| `--tvl-threshold` | 100.0 | Min pool TVL in USD |
| `--simulate-only` | false | Only simulate, don't prompt for execution |
| `--use-tenderly` | false | Use Tenderly instead of eth_simulate |
| `--slippage-bps` | 50 | Slippage tolerance (50 = 0.5%) |

## Security Notes

1. **Never expose your private key**: Always use environment variables, never CLI arguments

2. **Use simulate-only first**: Always test with `--simulate-only` before executing real transactions

3. **Slippage protection**: The default 0.5% slippage may not be suitable for large trades or volatile markets. Adjust `--slippage-bps` as needed

4. **Mainnet warning**: Executing swaps sends real transactions. Start with small amounts

5. **Verify routes**: Always review the displayed route before execution. Multi-hop routes through low-liquidity pools may result in worse execution

## Troubleshooting

**"Solver is not healthy"**: Wait for the solver to finish loading market data. Check the solver terminal for progress.

**"Sell/buy token not found"**: Ensure the token address is correct and the token exists on Tycho's indexer.

**"Simulation failed"**: Your RPC provider may not support `eth_simulate`. Try `--use-tenderly` or a different RPC.

**"No route found"**: The solver couldn't find a path between your tokens. Try increasing `--tvl-threshold` or check that both tokens have liquidity.
