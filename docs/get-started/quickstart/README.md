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

* **Tycho API key** — set as `TYCHO_API_KEY` ([get one here](https://app.gitbook.com/s/jrIe0oInIEt65tHqWn2w/for-solvers/indexer/tycho-client#authentication))
* **Rust 1.92+** — install via [rustup](https://rustup.rs/)

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
From [`clients/typescript/examples/tutorial/main.ts`](https://github.com/propeller-heads/fynd/blob/main/clients/typescript/examples/tutorial/main.ts):

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
From [`clients/rust/examples/quote.rs`](https://github.com/propeller-heads/fynd/blob/main/clients/rust/examples/quote.rs):

```rust
    let client = FyndClientBuilder::new(FYND_URL, FYND_URL)
        .build_quote_only()
        .expect("valid URL");

    // -----------------------------------------------------------------------
    // Health check
    // -----------------------------------------------------------------------
    let health = client.health().await?;
    println!("=== Health ===");
    println!("  healthy:            {}", health.healthy());
    println!("  last_update_ms:     {}", health.last_update_ms());
    println!("  num_solver_pools:   {}", health.num_solver_pools());
    println!("  derived_data_ready: {}", health.derived_data_ready());
    println!();

    // -----------------------------------------------------------------------
    // Quote 1: sell 1 WETH for USDC
    // -----------------------------------------------------------------------
    let quote = client
        .quote(QuoteParams::new(
            Order::new(addr(WETH), addr(USDC), one_ether(), OrderSide::Sell, addr(VITALIK), None),
            QuoteOptions::default(),
        ))
        .await?;

    println!("=== Quote: 1 WETH → USDC ===");
    println!("  order_id:      {}", quote.order_id());
    println!("  status:        {:?}", quote.status());
    println!("  amount_in:     {}", quote.amount_in());
    println!("  amount_out:    {}", quote.amount_out());
    println!("  gas_estimate:  {}", quote.gas_estimate());
    println!("  solve_time_ms: {}", quote.solve_time_ms());
    println!("  block:         #{} ({})", quote.block().number(), quote.block().hash());
    if let Some(route) = quote.route() {
        for (i, swap) in route.swaps().iter().enumerate() {
            println!(
                "  swap[{i}]: {} {} → {} (pool {})",
                swap.protocol(),
                swap.amount_in(),
                swap.amount_out(),
                swap.component_id(),
            );
        }
    }
    println!();
```

Run with: `cargo run --example quote`
{% endtab %}
{% endtabs %}

## Next steps

* [Server configuration](../../guides/server-configuration.md)
* [Custom algorithm](../../guides/custom-algorithm.md)
* [Benchmarking](../../guides/benchmarking.md)
* [Swap CLI](../../guides/swap-cli.md)
