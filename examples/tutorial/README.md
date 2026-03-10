# Tutorial: Quote, Simulate & Execute Swaps

This example demonstrates how to interact with an already-running tycho-router solver to get swap quotes, and optionally
simulate or execute those swaps on-chain using tycho-execution.

## Prerequisites

1. **Tycho API key**: Required for loading token data from the Tycho indexer

2. **RPC URL**: Required for simulation and execution (Ethereum mainnet or other supported chain)

3. **Private key**: Required only for execution or Permit2 simulation. For basic simulation, use `--sender` instead.

## Environment Variables

| Variable              | Required     | Description                                                   |
|-----------------------|--------------|---------------------------------------------------------------|
| `TYCHO_URL`           | No           | Tycho indexer URL (defaults to tycho-beta.propellerheads.xyz) |
| `TYCHO_API_KEY`       | Yes          | API key for Tycho indexer                                     |
| `RPC_URL`             | For sim/exec | Ethereum RPC endpoint                                         |
| `PRIVATE_KEY`         | For exec     | Wallet private key (hex, no 0x prefix)                        |
| `TENDERLY_ACCESS_KEY` | For Tenderly | Tenderly API access key                                       |
| `TENDERLY_ACCOUNT`    | For Tenderly | Tenderly account slug                                         |
| `TENDERLY_PROJECT`    | For Tenderly | Tenderly project slug                                         |

## Tutorial

### 1. Start the solver

In one terminal:

```bash
export TYCHO_URL=tycho-beta.propellerheads.xyz
export TYCHO_API_KEY=your_api_key
cargo run --release -- \
  --tycho-url tycho-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3
```

`--rpc-url` defaults to `https://eth.llamarpc.com`. For production,
add `--rpc-url https://your-rpc-provider.com/v1/your_key`.

Wait for "Solver is ready and accepting requests".

### 2. Get a quote (no private key needed)

In another terminal:

```bash
export TYCHO_URL=tycho-beta.propellerheads.xyz
export TYCHO_API_KEY=your_api_key
cargo run --example tutorial -- --sell-amount 100
```

This will display a quote for swapping 100 USDC to WETH (default tokens). You can customize the in/out tokens with
`--sell-token` and `--buy-token` parameters

### 3. Simulate without private key

Use `--sender` to simulate as any address without exposing a private key:

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
cargo run --example tutorial -- \
  --sell-amount 100 \
  --simulate-only \
  --sender 0xYourAddressHere
```

This simulates using standard ERC-20 `transferFrom` (not Permit2). The simulation will fail if the sender lacks token
balance.

### 4. Simulate with private key (Permit2)

For full Permit2 simulation with signature:

```bash
export PRIVATE_KEY=your_private_key_hex
export RPC_URL=https://your-rpc-provider.com/v1/your_key
cargo run --example tutorial -- \
  --sell-amount 100 \
  --simulate-only
```

### 5. Simulate with Tenderly

```bash
export TENDERLY_ACCESS_KEY=your_tenderly_key
export TENDERLY_ACCOUNT=your_account
export TENDERLY_PROJECT=your_project
cargo run --example tutorial -- \
  --sell-amount 100 \
  --simulate-only \
  --use-tenderly
```

### 6. Execute a swap (mainnet!)

```bash
export PRIVATE_KEY=your_private_key_hex
cargo run --example tutorial -- --sell-amount 10
```

You'll be prompted to choose: Simulate, Execute, or Cancel.

### 7. Custom tokens and protocols

```bash
cargo run --example tutorial -- \
  --sell-token 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount 1000 \
  --protocols "uniswap_v2,uniswap_v3"
```

## CLI Reference

| Flag              | Default               | Description                                                          |
|-------------------|-----------------------|----------------------------------------------------------------------|
| `--sell-token`    | USDC address          | Token address to sell                                                |
| `--buy-token`     | WETH address          | Token address to buy                                                 |
| `--sell-amount`   | 10.0                  | Amount to sell (human readable)                                      |
| `--chain`         | ethereum              | Blockchain (currently only ethereum is available)                    |
| `--solver-url`    | http://localhost:3000 | Solver API URL                                                       |
| `--tvl-threshold` | 10.0                  | Min pool TVL in ETH                                                  |
| `--simulate-only` | false                 | Only simulate, don't prompt for execution                            |
| `--use-tenderly`  | false                 | Use Tenderly instead of eth_simulate                                 |
| `--slippage-bps`  | 50                    | Slippage tolerance (50 = 0.5%)                                       |
| `--protocols`     | (all available)       | Comma-separated protocol systems (fetched from API if not specified) |
| `--sender`        | -                     | Sender address for simulation (use with --simulate-only)             |

## Security Notes

1. **Never expose your private key**: Always use environment variables, never CLI arguments

2. **Use simulate-only first**: Always test with `--simulate-only` before executing real transactions

3. **Slippage protection**: The default 0.5% slippage may not be suitable for large trades or volatile markets.
   Adjust `--slippage-bps` as needed

4. **Mainnet warning**: Executing swaps sends real transactions. Start with small amounts

5. **Verify routes**: Always review the displayed route before execution. Multi-hop routes through low-liquidity pools
   may result in worse execution

## Troubleshooting

**"Solver is not healthy"**: Wait for the solver to finish loading market data. Check the solver terminal for progress.

**"Sell/buy token not found"**: Ensure the token address is correct and the token exists on Tycho's indexer.

**"Simulation failed"**: Your RPC provider may not support `eth_simulate`. Try `--use-tenderly` or a different RPC.

**"No route found"**: The solver couldn't find a path between your tokens. Try adjusting `--tvl-threshold` or check that
both tokens have liquidity.

**"Cyclical swaps are only allowed..."**: This is a limitation in tycho-execution. The encoder doesn't support certain
multi-hop routes where tokens repeat. Try limiting protocols with `--protocols uniswap_v3` or using a smaller amount
that routes through fewer hops.

**"Swap encoder not found for protocol"**: The route uses a protocol not included in `--protocols`. Either add the
protocol or the component wasn't fetched from the API.
