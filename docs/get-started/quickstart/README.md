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

Get a quote from Fynd in three steps.

## Prerequisites

* **Tycho API key** (set as `TYCHO_API_KEY`, [get one here](https://t.me/fynd_portal_bot))
* **Rust 1.92+** ([install via rustup](https://rustup.rs/))

## Step 1 — Start Fynd

```bash
export TYCHO_API_KEY=your-api-key
export RUST_LOG=info
cargo run --release -- serve
```

## Step 2 — Request a quote

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
From [`clients/typescript/examples/tutorial/main.ts`](../../../clients/typescript/examples/tutorial/main.ts):

```typescript
  // FyndClient accepts a viemProvider adapter — no manual wrapping needed.
  const fyndClient = new FyndClient({
    baseUrl: solverUrl,
    chainId,
    sender: account.address,
    provider: viemProvider(publicClient, account.address),
  });

  // Check solver health
  const health = await fyndClient.health();
  console.log(
    `Solver: healthy=${String(health.healthy)},` +
      ` pools=${health.numSolverPools}`
  );
  if (!health.healthy) {
    throw new Error("Solver is not healthy.");
  }

  // Build Permit2 encoding options
  const amountIn = parseUnits("100", 6); // 100 USDC
  const deadline = BigInt(Math.floor(Date.now() / 1000) + 3600);
  const permit = {
    details: {
      token: USDC,
      amount: amountIn,
      expiration: deadline,
      nonce: 0n,
    },
    spender: "0x0000000000000000000000000000000000000000" as Address,
    sigDeadline: deadline,
  };

  const permitHash = permit2SigningHash(permit, chainId, PERMIT2);
  const permitSig = await account.signMessage({
    message: { raw: permitHash },
  });

  const encOpts = withPermit2(
    encodingOptions(50 / 10_000),
    permit,
    permitSig,
  );

  // Request a quote with server-side encoding
  console.log("\nQuoting 100 USDC -> WETH...");
  const quote = await fyndClient.quote({
    order: {
      tokenIn: USDC,
      tokenOut: WETH,
      amount: amountIn,
      side: "sell",
      sender: account.address,
    },
    options: { encodingOptions: encOpts },
  });

  console.log(`Status: ${quote.status}`);
  if (quote.status !== "success") {
    throw new Error(`Quote failed: ${quote.status}`);
  }
  console.log(`Amount out: ${quote.amountOut}`);
  console.log(`Gas estimate: ${quote.gasEstimate}`);
```

The full tutorial (signing + on-chain execution) is at `clients/typescript/examples/tutorial/main.ts`.
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
