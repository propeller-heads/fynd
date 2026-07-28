---
icon: coins
---

# Charge Fees on your Swaps

Use client fees to monetize your swap flow. Client fees are optional integrator fees set with `ClientFeeParams`.

The integrator keeps 80% of the client fee. Fynd keeps 20%.

Client fees are separate from [Fynd fees](router-fees.md), which still apply when no client fee is set.

## Fee breakdown with client fees

Quotes with encoding include a `fee_breakdown` with the exact amounts.

`amount_out` is the raw pre-fee swap output: what the route produces before router or client fees. It is **not** what the user receives. The user receives at least `fee_breakdown.min_amount_received` on-chain.

Fynd mirrors the on-chain `FeeCalculator` with identical integer arithmetic, then uses the result for `minAmountOut` in the encoded transaction.

Given `amount_out`, `router_fee_bps` (see [Fynd Fees](router-fees.md)), `client_fee_bps`, and `slippage`:

```
1. client_fee        = amount_out * client_fee_bps / 10,000
2. router_share      = amount_out * client_fee_bps * 2,000 / 100,000,000
3. client_portion    = client_fee - router_share
4. router_fee_output = amount_out * router_fee_bps / 10,000
5. router_fee        = router_share + router_fee_output
6. amount_after_fees = amount_out - client_portion - router_fee
7. max_slippage      = amount_after_fees * slippage
8. min_amount_received = amount_after_fees - max_slippage
```

All response fields use output token units:

| Field                 | Description                                                   |
| --------------------- | ------------------------------------------------------------- |
| `router_fee`          | Fynd fee + 20% of client fee                                  |
| `client_fee`          | Integrator's 80% share of the client fee                      |
| `max_slippage`        | Slippage allowance on the post-fee amount                     |
| `min_amount_received` | On-chain minimum the user receives (`minAmountOut` in the tx) |

Invariant: `amount_out = router_fee + client_fee + max_slippage + min_amount_received`

### Example

Example: 1,000,000 USDC output, 0.1 bps Fynd fee, 50 bps client fee, 1% slippage:

```
client_fee (total)   = 1,000,000 * 50 / 10,000         = 5,000
router_share         = 1,000,000 * 50 * 2,000 / 1e8    = 1,000
client_portion       = 5,000 - 1,000                    = 4,000
router_fee_output    = 1,000,000 * 0.1 / 10,000         = 10
router_fee           = 10 + 1,000                        = 1,010
amount_after_fees    = 1,000,000 - 4,000 - 1,010        = 994,990
max_slippage         = 994,990 * 0.01                    = 9,949
min_amount_received  = 994,990 - 9,949                   = 985,041
```

## Setting up client fees

1. Set a fee in basis points (e.g. `50` = 0.5%), a receiver address, and a `maxClientContribution`.
2. Have the fee receiver sign an EIP-712 `ClientFee` message authorizing these parameters.
3. Attach the signed params to `EncodingOptions.clientFeeParams`.
4. The router verifies the signature on-chain and deducts the fee. Fees go to the receiver's vault balance.

Without `ClientFeeParams`, no client fee is charged. [Fynd fees](router-fees.md) still apply.

### maxClientContribution

`maxClientContribution` caps how much the client can subsidize from their vault balance if slippage pushes the output below `minAmountOut`. If the shortfall exceeds the cap, the transaction reverts.

Set it to `0` to collect fees without covering slippage losses. This is the common case.

See [Tycho encoding docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution/encoding#encode) for vault details.

## EIP-712 signing

The fee receiver signs a typed data hash binding the fee params to the swap they were quoted for:

| Field                   | Type      | Description                                            |
| ----------------------- | --------- | ------------------------------------------------------ |
| `clientFeeBps`          | `uint32`  | Fee in fee units (100,000,000 = 100%; 1 bps = 10,000)  |
| `clientFeeReceiver`     | `address` | Address receiving the fee                              |
| `maxClientContribution` | `uint256` | Maximum subsidy from client vault                      |
| `deadline`              | `uint256` | Signature expiry (Unix timestamp)                      |
| `amountIn`              | `uint256` | Exact input amount from the order                      |
| `tokenIn`               | `address` | Input token                                            |
| `tokenOut`              | `address` | Output token                                           |
| `expectedAmountOut`     | `uint256` | Quoted output (`amount_out` of the unsigned quote)     |
| `minAmountOut`          | `uint256` | `fee_breakdown.min_amount_received`                    |
| `receiver`              | `address` | Address receiving the swap output                      |
| `swaps`                 | `bytes`   | Encoded swaps — hashed as `fee_breakdown.swaps_hash`   |

The API takes the client fee in basis points and scales it into the router's fee units, so the
client library helpers sign the scaled value rather than the raw bps.

**EIP-712 domain:**

| Field               | Value                        |
| ------------------- | ---------------------------- |
| `name`              | `TychoRouter`                |
| `version`           | `1`                          |
| `chainId`           | Target chain ID              |
| `verifyingContract` | TychoRouter contract address |

## Code examples

{% tabs %}
{% tab title="TypeScript" %}
```typescript
// Build fee params (without signature).
const feeParams: ClientFeeParams = {
    bps: 50,              // 0.5% fee
    receiver: feeReceiver,
    maxContribution: 0n,  // no vault subsidy
    deadline: 1893456000, // Unix timestamp
};

// Compute the EIP-712 hash and sign with the fee receiver's wallet. The swap context comes
// from a prior unsigned quote.
const hash = clientFeeSigningHash(feeParams, 1, routerAddress, {
    amountIn: quote.amountIn,
    tokenIn: sellToken,
    tokenOut: buyToken,
    expectedAmountOut: quote.amountOut,
    minAmountOut: quote.feeBreakdown.minAmountReceived,
    receiver: sender,
    swapsHash: quote.feeBreakdown.swapsHash,
});
const signature = await account.signMessage({message: {raw: hash}});

// Attach signature and wire into encoding options.
const opts = withClientFee(encodingOptions(0.005), {...feeParams, signature});
```
{% endtab %}

{% tab title="Rust" %}
```rust
    // Step 1: request a quote using unsigned client fee params.
    // The server encodes the full calldata and returns `swaps_hash`
    // in the fee breakdown and `signature_offset` in the transaction
    // so the client can patch the real signature in.
    let fee = ClientFeeParams::new(
        FEE_BPS,
        Bytes::copy_from_slice(fee_receiver.as_slice()),
        BigUint::ZERO,
        u64::MAX,
    );
    let order = Order::new(
        Bytes::copy_from_slice(sell_token.as_slice()),
        Bytes::copy_from_slice(buy_token.as_slice()),
        BigUint::from(SELL_AMOUNT),
        OrderSide::Sell,
        Bytes::copy_from_slice(sender.as_slice()),
        None,
    );
    let quote = client
        .quote(QuoteParams::new(
            order,
            QuoteOptions::default()
                .with_timeout_ms(5_000)
                .with_encoding_options(EncodingOptions::new(SLIPPAGE).with_client_fee(fee.clone())),
        ))
        .await?;

    let fee_breakdown = quote
        .fee_breakdown()
        .ok_or("no fee breakdown in quote")?;
    let swaps_hash = fee_breakdown
        .swaps_hash()
        .ok_or("no swaps_hash — server must support client fee signing")?;

    // Step 2: sign the full 11-field EIP-712 ClientFee hash.
    // receiver defaults to sender when the order has no explicit receiver.
    let hash = fee.eip712_signing_hash(
        chain_id,
        &router_address,
        quote.amount_in(),
        &Bytes::copy_from_slice(sell_token.as_slice()),
        &Bytes::copy_from_slice(buy_token.as_slice()),
        quote.amount_out(),
        fee_breakdown.min_amount_received(),
        &Bytes::copy_from_slice(sender.as_slice()),
        swaps_hash,
    )?;
    let sig = fee_signer
        .sign_hash(&B256::from(hash))
        .await?;

    // Step 3: patch the real signature into the calldata.
    let quote = quote.with_client_fee_signature(&sig.as_bytes()[..])?;
```

See the full working example: [`clients/rust/examples/swap_client_fee.rs`](../../clients/rust/examples/swap_client_fee.rs)
{% endtab %}
{% endtabs %}
