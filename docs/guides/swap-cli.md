---
description: Use fynd-swap-cli to dry-run and execute swaps against a running Fynd server.
icon: rectangle-terminal
---

# Swap CLI

`fynd-swap-cli` is a CLI binary for quoting, simulating, and executing swaps. It's useful for quick testing from the terminal without writing any code.

## Prerequisites

1. **Running Fynd server** — start `fynd serve` first. See the [quickstart.md](../get-started/quickstart.md "mention") if you haven't.
2. **RPC URL** — required for simulation and on-chain execution. The default public endpoint (`https://eth.llamarpc.com`) does not support state overrides, so you must supply your own.

## Build

```bash
cargo build --release -p fynd-swap-cli
# binary: target/release/fynd-swap-cli
```

## Dry-run a swap (ERC-20)

By default, `fynd-swap-cli` runs a **dry-run**: it generates an ephemeral key and injects ERC-20 storage overrides so the simulation succeeds without any real funds or wallet approvals.

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key

./target/release/fynd-swap-cli \
  --sell-token  0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token   0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount 1000000000
```

The output prints the quote (amount\_in, amount\_out, gas estimate, route) followed by the simulation result.

{% hint style="info" %}
`--sell-amount` is in raw atomic units. 1 000 000 000 = 1000 USDC (6 decimals). 1 000 000 000 000 000 000 = 1 WETH (18 decimals).
{% endhint %}

## Dry-run a swap (Permit2)

Add `--transfer-type transfer-from-permit2` and supply the TychoRouter address with `--router`. The dry-run uses nonce 0 and maximum deadlines — no chain reads for Permit2 state are needed.

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key

./target/release/fynd-swap-cli \
  --sell-token    0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token     0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount   1000000000 \
  --transfer-type transfer-from-permit2 \
  --router        <TychoRouter address>
```

Find the TychoRouter address for your chain in the [Tycho contract addresses](https://docs.propellerheads.xyz/tycho/for-solvers/execution/contract-addresses).

## Execute on-chain (ERC-20)

Add `--execute` and set `PRIVATE_KEY`. The CLI will verify that the router has sufficient allowance before submitting and will print the required `cast send` approval command if it doesn't.

{% hint style="warning" %}
This sends a real transaction. Ensure your wallet has the sell token and has approved the TychoRouter to spend it before running with `--execute`.
{% endhint %}

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
export PRIVATE_KEY=your_private_key_hex   # no 0x prefix

./target/release/fynd-swap-cli \
  --sell-token  0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token   0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount 1000000000 \
  --execute
```

## Execute on-chain (Permit2)

Add `--execute --transfer-type transfer-from-permit2 --router <addr>`. The CLI reads the current Permit2 nonce from the contract, builds the EIP-712 permit, signs it with your key, and submits the swap in one step.

Your wallet must have already approved the Permit2 contract (`0x000000000022D473030F116dDEE9F6B43aC78BA3`) to spend the sell token. The CLI prints the required `cast send` command if that approval is missing.

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
export PRIVATE_KEY=your_private_key_hex   # no 0x prefix

./target/release/fynd-swap-cli \
  --sell-token    0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --buy-token     0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --sell-amount   1000000000 \
  --transfer-type transfer-from-permit2 \
  --router        <TychoRouter address> \
  --execute
```

## Embedded server (no separate `fynd serve` needed)

{% hint style="info" %}
If you don't want to run `fynd serve` separately, pass `--tycho-url` and `fynd-swap-cli` will spawn its own embedded solver automatically.

```bash
export TYCHO_API_KEY=your_api_key
export RPC_URL=https://your-rpc-provider.com/v1/your_key

./target/release/fynd-swap-cli \
  --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
  --sell-amount 1000000000
```

This is convenient for a one-off swap but slow to initialize on every run (the solver must sync protocol state from Tycho before it can serve quotes). For repeated use, keep `fynd serve` running and omit `--tycho-url`.
{% endhint %}

## CLI Reference

| Flag                    | Env var         | Default                                      | Description                                                                   |
| ----------------------- | --------------- | -------------------------------------------- | ----------------------------------------------------------------------------- |
| `--sell-token`          | —               | USDC (mainnet)                               | Token address to sell                                                         |
| `--buy-token`           | —               | WETH (mainnet)                               | Token address to buy                                                          |
| `--sell-amount`         | —               | `1000000000`                                 | Amount to sell in raw atomic units                                            |
| `--slippage-bps`        | —               | `50` (0.5%)                                  | Slippage tolerance in basis points                                            |
| `--fynd-url`            | —               | `http://localhost:3000`                      | Fynd server URL (ignored when `--tycho-url` is set)                           |
| `--transfer-type`       | —               | `transfer-from`                              | `transfer-from` or `transfer-from-permit2`                                    |
| `--execute`             | —               | false (dry-run)                              | Submit the swap on-chain. Requires `PRIVATE_KEY`.                             |
| `--router`              | —               | —                                            | TychoRouter address. Required for `transfer-from-permit2`.                    |
| `--permit2`             | —               | `0x000000000022D473030F116dDEE9F6B43aC78BA3` | Permit2 contract address                                                      |
| `--rpc-url`             | `RPC_URL`       | `https://eth.llamarpc.com`                   | Ethereum RPC endpoint                                                         |
| `--chain`               | —               | `Ethereum`                                   | Target chain                                                                  |
| `--tycho-url`           | —               | —                                            | If set, spawns an embedded Fynd solver connecting to this Tycho WebSocket URL |
| `--tycho-api-key`       | `TYCHO_API_KEY` | —                                            | Tycho API key (required when `--tycho-url` is set)                            |
| `--disable-tls`         | —               | false                                        | Disable TLS for the Tycho WebSocket connection                                |
| `--protocols`           | —               | (all on-chain, fetched from Tycho)           | Comma-separated protocols to index. Only used with `--tycho-url`.             |
| `--worker-pools-config` | —               | —                                            | Path to worker pools TOML config. Only used with `--tycho-url`.               |
| `--http-port`           | —               | `3000`                                       | HTTP port for the embedded solver. Only used with `--tycho-url`.              |

## Security Notes

1. **Never expose your private key.** Use the `PRIVATE_KEY` environment variable, never a CLI argument. Run `unset HISTFILE` before setting it to prevent it from being saved to your shell history.
2. **Dry-run first.** The default mode (no `--execute`) simulates the full swap with storage overrides — no funds needed. Confirm the output looks correct before adding `--execute`.
3. **Slippage protection.** The default 0.5% slippage may not be sufficient for large trades or volatile markets. Adjust `--slippage-bps` accordingly.
4. **Mainnet warning.** `--execute` sends a real transaction. Start with small amounts. All routes execute through the [Tycho Router](https://docs.propellerheads.xyz/tycho/for-solvers/execution/contract-addresses) contract.
5. **Verify routes.** The CLI prints the full route before executing. Multi-hop routes through low-liquidity pools can result in worse execution.
6. **Prices are indicative.** Quotes reflect the best route at query time but are not guaranteed on-chain. Pool states change every block, and the longer you wait to execute, the more the price may drift.

## Troubleshooting

**"Solver is not healthy"**: Wait for the solver to finish loading market data. Check the `fynd serve` terminal for progress, or poll `curl http://localhost:3000/v1/health`.

**"Sell/buy token not found"**: Ensure the token address is correct and [the token exists on Tycho's indexer](https://docs.propellerheads.xyz/tycho/for-solvers/indexer/tycho-rpc#post-v1-tokens).

**"No route found"**: Fynd couldn't find a path between your tokens. Check that both tokens have enough on-chain liquidity.

**"Insufficient allowance"**: The CLI prints the exact `cast send` approval command needed. Run it, then retry.
