---
icon: coins
---

# Client Fees

The Tycho Router supports optional client fees — a percentage of the swap output charged on behalf of an integrator.
Fees are configured per-request via `ClientFeeParams` in the encoding options.

## How it works

1. The integrator chooses a fee in basis points (e.g. `50` = 0.5%) and a receiver address.
2. The fee receiver signs an EIP-712 `ClientFee` message authorizing the fee parameters.
3. The signed params are attached to the quote request via `EncodingOptions.clientFeeParams`.
4. The router verifies the signature on-chain and deducts the fee from the swap output.

When no `ClientFeeParams` are provided, no client fee is charged.

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
    // Compute the EIP-712 signing hash for the client fee.
    let hash = ClientFeeParams::eip712_signing_hash(
        FEE_BPS,
        &fee_receiver_bytes,
        &max_contribution,
        &deadline,
        1, // chainId = Ethereum mainnet
        &router_address,
    )?;

    // Sign the hash with the fee receiver's key.
    let sig = signer
        .sign_hash(&alloy::primitives::B256::from(hash))
        .await?;
    let signature = Bytes::copy_from_slice(&sig.as_bytes()[..]);

    // Build encoding options with the client fee attached.
    let fee =
        ClientFeeParams::new(FEE_BPS, fee_receiver_bytes, max_contribution, deadline, signature);
    let encoding_options = EncodingOptions::new(SLIPPAGE).with_client_fee(fee);
```

See the full working example: [`clients/rust/examples/swap_client_fee.rs`](https://github.com/propeller-heads/fynd/blob/main/clients/rust/examples/swap_client_fee.rs)
{% endtab %}

{% tab title="TypeScript" %}
```typescript
  // Compute the EIP-712 hash
  const hash = clientFeeSigningHash(
    50,             // 0.5% fee
    feeReceiver,    // address
    0n,             // no vault subsidy
    1893456000n,    // deadline
    1,              // chain ID
    routerAddress,  // TychoRouter address
  );

  // Sign with the fee receiver's wallet
  const signature = await account.signMessage({ message: { raw: hash } });

  // Attach to encoding options
  const opts = withClientFee(encodingOptions(0.005), {
    bps: 50,
    receiver: feeReceiver,
    maxContribution: 0n,
    deadline: 1893456000n,
    signature,
  });
```
{% endtab %}
{% endtabs %}
