---
description: Use fynd-swap-cli to dry-run and execute swaps against a running Fynd server.
icon: rectangle-terminal
---

# Swap CLI

`fynd-swap-cli` is a CLI binary for quoting, simulating, and executing swaps. It's useful for quick testing from the terminal without writing any code.

## Setup

{% tabs %}
{% tab title="Docker Compose" %}
**Prerequisites:** Docker

The [docker-compose.swap.yml](https://github.com/propeller-heads/fynd/blob/main/docker-compose.swap.yml) file in the repo root starts a Fynd server and drops you into a shell with `fynd-swap-cli` pre-installed — no local build required.

```bash
export TYCHO_API_KEY=your_tycho_api_key

docker compose -f docker-compose.swap.yml run --rm fynd-shell
```

This also starts `fynd-serve` automatically. Run swaps with:

```bash
# Inside the fynd-shell container:
fynd-swap-cli
```

{% hint style="info" %}
For on-chain execution, pass `PRIVATE_KEY` at startup: `docker compose -f docker-compose.swap.yml run --rm -e PRIVATE_KEY=your_key fynd-shell`
{% endhint %}

When done, stop and remove the server container:

```bash
docker compose -f docker-compose.swap.yml down
```
{% endtab %}

{% tab title="Build from source" %}
**Prerequisites:** A running Fynd server — start `fynd serve` first. See the [quickstart](../get-started/quickstart/ "mention") if you haven't.

```bash
cargo install --path tools/fynd-swap-cli
```

{% endtab %}
{% endtabs %}

---

## Dry-run a swap (ERC-20)

By default, `fynd-swap-cli` runs a **dry-run**: it uses a well-funded sender address and injects ERC-20 storage overrides so the simulation succeeds without any real funds or wallet approvals.

```bash
fynd-swap-cli
```

This sells 1 WETH for USDC using the defaults. Pass explicit tokens and amounts to customise:

```bash
fynd-swap-cli \
  --sell-token  0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  --buy-token   0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --sell-amount 2000000000000000000
```

The output prints the quote (amount\_in, amount\_out, gas estimate, route) followed by the simulation result.

{% hint style="info" %}
`--sell-amount` is in raw atomic units. 1 000 000 000 = 1000 USDC (6 decimals). 1 000 000 000 000 000 000 = 1 WETH (18 decimals).
{% endhint %}

## Dry-run a swap (Permit2)

Add `--transfer-type transfer-from-permit2`. The router address is fetched automatically from the Fynd server. The dry-run uses nonce 0 and maximum deadlines, so no chain reads are needed.

```bash
fynd-swap-cli --transfer-type transfer-from-permit2
```

## Execute on-chain (ERC-20)

Add `--execute` and set `PRIVATE_KEY`. The CLI checks the router allowance automatically and submits an approval transaction first if one is needed.

{% hint style="warning" %}
This sends real transactions. Ensure your wallet has the sell token before running with `--execute`.
{% endhint %}

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
export PRIVATE_KEY=your_private_key_hex   # no 0x prefix

fynd-swap-cli --execute
```

## Execute on-chain (Permit2)

Add `--execute --transfer-type transfer-from-permit2`. The router address is resolved automatically from the Fynd server. The CLI checks the Permit2 allowance and submits an approval transaction first if needed, then reads the current nonce, builds the EIP-712 permit, signs it, and submits the swap.

```bash
export RPC_URL=https://your-rpc-provider.com/v1/your_key
export PRIVATE_KEY=your_private_key_hex   # no 0x prefix

fynd-swap-cli --transfer-type transfer-from-permit2 --execute
```

## Swap using vault funds

If tokens are already deposited in the Tycho Router vault, use `--transfer-type use-vaults-funds`. No ERC-20 approval or Permit2 signature is needed.

```bash
fynd-swap-cli --transfer-type use-vaults-funds
```

---

## CLI Reference

| Flag              | Env var   | Default                                      | Description                                                     |
| ----------------- | --------- | -------------------------------------------- | --------------------------------------------------------------- |
| `--sell-token`    | —         | WETH (mainnet)                               | Token address to sell                                           |
| `--buy-token`     | —         | USDC (mainnet)                               | Token address to buy                                            |
| `--sell-amount`   | —         | `1000000000000000000` (1 WETH)               | Amount to sell in raw atomic units                              |
| `--slippage-bps`  | —         | `50` (0.5%)                                  | Slippage tolerance in basis points                              |
| `--fynd-url`      | `FYND_URL` | `http://localhost:3000`                     | Fynd server URL                                                 |
| `--transfer-type` | —         | `transfer-from`                              | `transfer-from`, `transfer-from-permit2`, or `use-vaults-funds` |
| `--execute`       | —         | false (dry-run)                              | Submit the swap on-chain. Requires `PRIVATE_KEY`.               |
| `--permit2`       | —         | `0x000000000022D473030F116dDEE9F6B43aC78BA3` | Permit2 contract address                                        |
| `--rpc-url`       | `RPC_URL` | `https://reth-ethereum.ithaca.xyz/rpc`       | Ethereum RPC endpoint (must support `eth_call` state overrides) |

## Security Notes

1. **Never expose your private key.** Use the `PRIVATE_KEY` environment variable, never a CLI argument. Run `unset HISTFILE` before setting it to prevent it from being saved to your shell history.
2. **Dry-run first.** The default mode (no `--execute`) simulates the full swap with storage overrides — no funds needed. Confirm the output looks correct before adding `--execute`.
3. **Slippage protection.** The default 0.5% slippage may not be sufficient for large trades or volatile markets. Adjust `--slippage-bps` accordingly.
4. **Mainnet warning.** `--execute` may send multiple transactions (approval + swap). Start with small amounts. All routes execute through the [Tycho Router](https://docs.propellerheads.xyz/tycho/for-solvers/execution/contract-addresses) contract.
5. **Verify routes.** The CLI prints the full route before executing. Multi-hop routes through low-liquidity pools can result in worse execution.
6. **Prices are indicative.** Quotes reflect the best route at query time but are not guaranteed on-chain. Pool states change every block, and the longer you wait to execute, the more the price may drift.

## Troubleshooting

**"Solver is not healthy"**: Wait for the solver to finish loading market data. Check the `fynd serve` terminal for progress, or poll `curl http://localhost:3000/v1/health`.

**"Sell/buy token not found"**: Ensure the token address is correct and [the token exists on Tycho's indexer](https://docs.propellerheads.xyz/tycho/for-solvers/indexer/tycho-rpc#post-v1-tokens).

**"No route found"**: Fynd couldn't find a path between your tokens. Check that both tokens have enough on-chain liquidity.
