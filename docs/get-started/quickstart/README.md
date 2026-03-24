---
icon: rocket-launch
layout:
  width: default
  title:
    visible: true
  description:
    visible: true
  tableOfContents:
    visible: true
  outline:
    visible: true
  pagination:
    visible: true
  metadata:
    visible: true
  tags:
    visible: true
---

# Quickstart

Execute a swap with Fynd in three steps.

## Prerequisites

* **Tycho API key** (set as `TYCHO_API_KEY`, [get one here](https://t.me/fynd_portal_bot))
* **Rust 1.92+** ([install via rustup](https://rustup.rs/))

## Step 1 — Start Fynd

```bash
export TYCHO_API_KEY=your-api-key
export RUST_LOG=info
cargo run --release -- serve
```

## Step 2 — Execute a swap

{% tabs %}
{% tab title="curl" %}
```bash
# Wait until healthy
curl http://localhost:3000/v1/health
# → {"healthy":true,...}

# Request a quote — 1000 USDC → WETH
curl -X POST http://localhost:3000/v1/quote \
  -H "Content-Type: application/json" \
  -d '{
    "orders": [
      {
        "token_in":  "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "token_out": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "amount":    "1000000000",
        "side":      "sell",
        "sender":    "0x0000000000000000000000000000000000000001"
      }
    ],
    "options": {
      "timeout_ms": 5000,
      "min_responses": 1
    }
  }'
```

This quotes 1000 USDC (6 decimals → 1 000 000 000 atomic units) for WETH.
{% endtab %}

{% tab title="TypeScript" %}
From [`clients/typescript/examples/tutorial/main.ts`](https://github.com/propeller-heads/fynd/blob/main/clients/typescript/examples/tutorial/main.ts):

```typescript
const client = new FyndClient({
  baseUrl: FYND_URL,
  chainId: mainnet.id,
  sender: account.address,
  provider: viemProvider(publicClient, account.address),
});

// 1. Quote
const quote = await client.quote({
  order: { tokenIn: USDC, tokenOut: WETH, amount: SELL_AMOUNT, side: 'sell', sender: account.address },
  options: { encodingOptions: encodingOptions(0.005) },
});
console.log(`amount_out: ${quote.amountOut}`);

// 2. Approve if needed (checks on-chain allowance, skips if sufficient)
const approvalPayload = await client.approval({ token: USDC, amount: SELL_AMOUNT, checkAllowance: true });
if (approvalPayload !== null) {
  const sig = await walletClient.signMessage({ message: { raw: approvalSigningHash(approvalPayload) } });
  await client.executeApproval({ tx: approvalPayload.tx, signature: sig });
}

// 3. Sign and execute swap
const payload = await client.swapPayload(quote);
const sig = await walletClient.signMessage({ message: { raw: swapSigningHash(payload) } });
const settled = await (await client.executeSwap(assembleSignedSwap(payload, sig))).settle();
console.log(`gas: ${settled.gasCost}`);
```
{% endtab %}

{% tab title="Rust" %}
From [`clients/rust/examples/swap_erc20.rs`](../../../clients/rust/examples/swap_erc20.rs):

```rust
let client = FyndClientBuilder::new(FYND_URL, RPC_URL)
    .with_sender(sender)
    .build()
    .await?;

// 1. Quote
let quote = client
    .quote(QuoteParams::new(
        Order::new(
            Bytes::copy_from_slice(sell_token.as_slice()),
            Bytes::copy_from_slice(buy_token.as_slice()),
            BigUint::from(SELL_AMOUNT),
            OrderSide::Sell,
            Bytes::copy_from_slice(sender.as_slice()),
            None,
        ),
        QuoteOptions::default()
            .with_timeout_ms(5_000)
            .with_encoding_options(EncodingOptions::new(SLIPPAGE)),
    ))
    .await?;
println!("amount_out: {}", quote.amount_out());

// 2. Approve if needed (checks on-chain allowance, skips if sufficient)
if let Some(approval_payload) = client
    .approval(
        &ApprovalParams::new(
            Bytes::copy_from_slice(sell_token.as_slice()),
            BigUint::from(SELL_AMOUNT),
            true,
        ),
        &SigningHints::default(),
    )
    .await?
{
    let sig = signer.sign_hash(&approval_payload.signing_hash()).await?;
    client
        .execute_approval(SignedApproval::assemble(approval_payload, sig))
        .await?
        .await?;
}

// 3. Sign and execute swap
let payload = client.swap_payload(quote, &SigningHints::default()).await?;
let sig = signer.sign_hash(&payload.signing_hash()).await?;
let receipt = client
    .execute_swap(SignedSwap::assemble(payload, sig), &ExecutionOptions::default())
    .await?
    .await?;
println!("gas: {}", receipt.gas_cost());
```

Run with: `cargo run --example swap_erc20 -p fynd-client`
{% endtab %}
{% endtabs %}

## Next steps

* [Server configuration](../../guides/server-configuration.md)
* [Custom algorithm](../../guides/custom-algorithm.md)
* [Benchmarking](../../guides/benchmarking.md)
* [Swap CLI](../../guides/swap-cli.md)
