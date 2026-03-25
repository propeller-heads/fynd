---
icon: coins
---

# Client Fees

Fynd supports optional client fees — a percentage of the swap output charged on behalf of an integrator.
Fees are configured per-request via `ClientFeeParams` in the encoding options.

## How it works

1. The integrator chooses a fee in basis points (e.g. `50` = 0.5%), a receiver address, and a `maxClientContribution`.
2. The fee receiver signs an EIP-712 `ClientFee` message authorizing the fee parameters.
3. The signed params are attached to the quote request via `EncodingOptions.clientFeeParams`.
4. The router contract verifies the signature on-chain and deducts the fee from the swap output. Fees are credited to
   the receiver's vault balance (not transferred directly).

When no `ClientFeeParams` are provided, no client fee is charged.

### maxClientContribution

`maxClientContribution` is the maximum amount (in output token units) the client is willing to subsidize from their
vault balance if slippage causes the swap output to fall below `minAmountOut`. If the shortfall exceeds this limit, the
transaction reverts.

Set to `0` if you don't want to subsidize slippage. This is the common case — the client collects fees but doesn't cover
losses.

See [Tycho encoding docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution/encoding#encode) for more details
on the vault mechanism.

## EIP-712 signing

The fee receiver must sign a typed data hash with the following structure:

| Field                   | Type      | Description                       |
|-------------------------|-----------|-----------------------------------|
| `clientFeeBps`          | `uint16`  | Fee in basis points (0–10,000)    |
| `clientFeeReceiver`     | `address` | Address receiving the fee         |
| `maxClientContribution` | `uint256` | Maximum subsidy from client vault |
| `deadline`              | `uint256` | Signature expiry (Unix timestamp) |

**EIP-712 domain:**

| Field               | Value                        |
|---------------------|------------------------------|
| `name`              | `TychoRouter`                |
| `version`           | `1`                          |
| `chainId`           | Target chain ID              |
| `verifyingContract` | TychoRouter contract address |

## Code examples

Both the Rust and TypeScript clients provide helper functions to compute the signing hash.

{% tabs %}
{% tab title="Rust" %}

```rust
    // Build the fee params (without signature).
    let fee = ClientFeeParams::new(
        FEE_BPS,
        Bytes::copy_from_slice(fee_receiver.as_slice()),
        BigUint::ZERO,
        u64::MAX,
    );

    // Compute the EIP-712 signing hash and sign it with the fee receiver's key.
    let hash = fee.eip712_signing_hash(1, &router_address)?; // chainId = Ethereum mainnet
    let sig = fee_signer
        .sign_hash(&B256::from(hash))
        .await?;

    // Attach the signature and wire it into encoding options.
    let fee = fee.with_signature(Bytes::copy_from_slice(&sig.as_bytes()[..]));
    let encoding_options = EncodingOptions::new(SLIPPAGE).with_client_fee(fee);
```

See the full working
example: [`clients/rust/examples/swap_client_fee.rs`](https://github.com/propeller-heads/fynd/blob/main/clients/rust/examples/swap_client_fee.rs)
{% endtab %}

{% tab title="TypeScript" %}

```typescript
// Build fee params (without signature).
const feeParams: ClientFeeParams = {
  bps: 50,              // 0.5% fee
  receiver: feeReceiver,
  maxContribution: 0n,  // no vault subsidy
  deadline: 1893456000, // Unix timestamp
};

// Compute the EIP-712 hash and sign with the fee receiver's wallet.
const hash = clientFeeSigningHash(feeParams, 1, routerAddress);
const signature = await account.signMessage({ message: { raw: hash } });

// Attach signature and wire into encoding options.
const opts = withClientFee(encodingOptions(0.005), { ...feeParams, signature });
```

{% endtab %}
{% endtabs %}
