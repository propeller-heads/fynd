# Tutorial: Quote and Execute Swaps via FyndClient

This example demonstrates how to interact with a running Fynd solver to get swap quotes and
execute them — either as a dry-run simulation (default) or as a real on-chain transaction.

## Prerequisites

1. **Running Fynd solver**: The example talks to a solver over HTTP (`--fynd-url`)

2. **RPC URL**: Required for simulation and execution (Ethereum mainnet or other supported chain)

3. **Private key**: Required only for on-chain execution (`--execute`)

## Environment Variables

| Variable      | Required          | Description                       |
|---------------|-------------------|-----------------------------------|
| `RPC_URL`     | Yes               | Ethereum JSON-RPC endpoint        |
| `PRIVATE_KEY` | With `--execute`  | Wallet private key (0x-prefixed)  |

## Tutorial

### 1. Start the solver

```bash
cargo run --release -- \
  --tycho-url tycho-beta.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3
```

Wait for "Solver is ready and accepting requests".

### 2. Get a quote and dry-run simulate (no private key needed)

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
cargo run --example tutorial -- --sell-amount 1000000000
```

This quotes 1000 USDC → WETH (default tokens) and simulates the swap using ERC-20 storage
overrides, so no real funds are required. You'll see the settled amount and gas cost.

### 3. Execute on-chain

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
export PRIVATE_KEY=0x...
cargo run --example tutorial -- --sell-amount 1000000000 --execute
```

If the router hasn't been approved yet, the example prints the exact `cast send` command needed:

```
Error: insufficient sell-token allowance for the Fynd router.
  Token:     0xa0b86991...
  Router:    0xabc...
  Allowance: 0
  Required:  1000000000

Approve the router with:
  cast send 0xa0b8... "approve(address,uint256)" 0xabc... 1000000000 \
    --rpc-url $RPC_URL --private-key $PRIVATE_KEY
```

### 4. Custom tokens

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
cargo run --example tutorial -- \
  --sell-token 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token  0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount 1000000000 \
  --slippage-bps 50
```

## CLI Reference

| Flag            | Default               | Description                                        |
|-----------------|-----------------------|----------------------------------------------------|
| `--sell-token`  | USDC address          | ERC-20 address of the token to sell                |
| `--buy-token`   | WETH address          | ERC-20 address of the token to buy                 |
| `--sell-amount` | 1000000000            | Amount to sell in raw atomic units (no decimals)   |
| `--fynd-url`    | http://localhost:3000 | Fynd solver API URL                                |
| `--slippage-bps`| 50                    | Slippage tolerance in basis points (50 = 0.5%)     |
| `--execute`     | false                 | Submit on-chain instead of dry-running             |

## Security Notes

1. **Never expose your private key**: Always use environment variables, never CLI arguments

2. **Dry-run first**: The default mode simulates the swap without spending funds — verify the
   output before adding `--execute`

3. **Exact approvals**: When prompted to approve, the suggested command approves only the exact
   amount needed for the swap, not an unlimited allowance

4. **Slippage protection**: The default 0.5% slippage may not be suitable for large trades or
   volatile markets. Adjust `--slippage-bps` as needed

5. **Mainnet warning**: `--execute` sends a real transaction. Start with small amounts

## Troubleshooting

**"Solver is not healthy"**: Wait for the solver to finish loading market data.

**"Quote has no calldata"**: The solver did not return encoded calldata. Ensure the solver
supports encoding and that `--fynd-url` points to the correct endpoint.

**"insufficient sell-token allowance"**: Run the printed `cast send approve(...)` command, then
retry with `--execute`.

**"No route found"**: The solver couldn't find a path between your tokens. Check that both tokens
have liquidity on the connected protocols.

**"timed out waiting for transaction to be mined"**: The transaction was submitted but not mined
within 120 seconds. Check the mempool for the pending tx.
