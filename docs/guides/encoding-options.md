---
icon: code
---

# Encoding Options

When you request a quote, you can include `encoding_options` to have Fynd encode the swap into a
ready-to-submit transaction. Without encoding options, you get a quote only (price, route, gas
estimate) but no transaction.

For full details on how the TychoRouter contract works, see the
[Tycho execution docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution).

## Fields

| Field               | Type              | Required | Default           | Description                                                                                                     |
|---------------------|-------------------|----------|-------------------|-----------------------------------------------------------------------------------------------------------------|
| `slippage`          | `float`           | yes      | —                 | Slippage tolerance as a fraction (e.g. `0.005` = 0.5%). Applied to the quoted output to compute `minAmountOut`. |
| `transfer_type`     | `string`          | no       | `"transfer_from"` | How the router receives your input tokens. See [transfer types](#transfer-types).                               |
| `permit`            | `PermitSingle`    | no       | —                 | Permit2 authorization. Required when `transfer_type` is `"transfer_from_permit2"`.                              |
| `permit2_signature` | `string`          | no       | —                 | Hex-encoded 65-byte signature over the permit. Required when `permit` is set.                                   |
| `client_fee_params` | `ClientFeeParams` | no       | —                 | Optional integrator fee. See the [client fees guide](client-fees.md).                                           |

## Transfer types

The `transfer_type` field controls how the TychoRouter contract receives your input tokens. For a
deeper explanation see the
[Tycho execution docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution).

### `transfer_from` (default)

Standard ERC-20 approval flow. Before submitting the transaction, the sender must have called
`approve()` on the input token granting the TychoRouter contract a sufficient allowance.

### `transfer_from_permit2`

Uses Uniswap's [Permit2](https://docs.propellerheads.xyz/tycho/for-solvers/execution) contract for
gasless approvals. The sender signs a `PermitSingle` off-chain and passes it along with the
signature in the quote request. No on-chain `approve()` needed (but the token must be approved to
the Permit2 contract).

When using this transfer type, both `permit` and `permit2_signature` are required.

### `use_vaults_funds`

Draws tokens from the sender's vault balance in the TychoRouter contract (ERC-6909). No approval or
permit needed — tokens must have been deposited into the vault beforehand. See the
[vault mechanism](https://docs.propellerheads.xyz/tycho/for-solvers/execution) in the Tycho docs.

## Slippage

The `slippage` value is a decimal fraction:

| Value   | Meaning |
|---------|---------|
| `0.001` | 0.1%    |
| `0.005` | 0.5%    |
| `0.01`  | 1%      |

Fynd computes `minAmountOut = quotedAmountOut * (1 - slippage)` and encodes it into the transaction.
If on-chain execution produces less than `minAmountOut`, the transaction reverts.

Typical values are `0.005` (0.5%) for stablecoin pairs and `0.01` (1%) for volatile pairs.

## The response transaction

When encoding options are present and the quote succeeds, the response includes a `transaction`
object:

| Field   | Type     | Description                                                                                                                                 |
|---------|----------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `to`    | `string` | The TychoRouter contract address. See [contract addresses](https://docs.propellerheads.xyz/tycho/for-solvers/execution/contract-addresses). |
| `value` | `string` | Native token value (wei). Non-zero only when the input token is the native token.                                                           |
| `data`  | `string` | Hex-encoded calldata. Submit this as the `data` field of your Ethereum transaction.                                                         |

Use `to`, `value`, and `data` directly in your transaction. Set `from` to the sender address from
your order, choose a gas limit (the quote's `gas_estimate` is a good starting point), and submit.
