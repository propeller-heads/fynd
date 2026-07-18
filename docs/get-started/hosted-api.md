---
description: Get routing quotes in minutes from Propeller Heads' hosted Fynd API — no local setup required.
icon: cloud
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

# Hosted Fynd API (Beta)

{% hint style="warning" %}
**Beta.** The hosted Fynd API is in beta. Limits, supported chains, and the surface area may change as we scale the service. There is no SLA during beta — status and incidents are posted in [our Telegram group](https://t.me/+B4CNQwv7dgIyYTJl). For production workloads with strict uptime or throughput requirements, see [Scaling beyond the hosted tiers](#scaling-beyond-the-hosted-tiers).
{% endhint %}

Fynd is an [open-source](https://github.com/propeller-heads/fynd) DEX aggregator: you send it a token pair and amount, it finds the best on-chain route across the liquidity venues it tracks, and returns the expected output plus a ready-to-submit transaction. You can run Fynd on your own hardware (see [Self-host quickstart](../quickstart/README.md)) — **or you can use our hosted API and skip the setup entirely.** This page covers the hosted path: get an API key, send a request, get a route.

The hosted API runs the **latest released Fynd** with **routing settings tuned per chain by the Fynd team**, so you always track the newest routing improvements without managing releases or worker pools yourself.

{% hint style="info" %}
**Prefer a UI?** A hosted web frontend is coming soon. Today the API is the integration surface.
{% endhint %}

## What you get

* **One endpoint, all chains.** `https://fynd-api.propellerheads.xyz` routes `/v1/{chain}/…` to a per-chain Fynd backend. No per-chain infrastructure on your side.
* **Latest Fynd version.** The hosted backends track the newest Fynd release; you don't pin or upgrade anything. (During beta, the surface area may change — see the [FAQ](#faq) for version and deprecation notes.)
* **Optimized routing.** Worker-pool and algorithm configuration are tuned per chain by the Fynd team. Hosted backends run the full default protocol set — see the [list of supported protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).
* **Same quote API as self-hosted.** The per-chain request/response bodies are identical to running `fynd serve` yourself — see [API reference](../reference/api.md). The hosted gateway adds a `/{chain}` path segment and API-key auth; a self-hosted instance serves one chain at `/v1/…` with no auth.

## Get an API key

1. Open [@FyndPortalBot](https://t.me/fynd_portal_bot) on Telegram.
2. Run `/start` and follow the prompts. You'll receive a **Fynd API key** (also valid for the [Tycho](https://docs.propellerheads.xyz/tycho) liquidity indexer that feeds Fynd).
3. Save the key immediately. The bot won't show it again, and you get **3 self-service revocations** (rotations of your key) before you need to contact support in [our Telegram group](https://t.me/+B4CNQwv7dgIyYTJl).

```bash
export FYND_API_KEY=your-api-key
```

{% hint style="warning" %}
**Keep the key server-side.** The `Authorization` header is a raw secret. Never ship it in browser/client code. Proxy Fynd requests through your backend; do not call the hosted API directly from a frontend. (A hosted web frontend is coming soon — until then, all API access is server-to-server.)
{% endhint %}

The key is sent as the **raw `Authorization` header value** on every request — **no `Bearer` prefix**:

```bash
curl -H "Authorization: $FYND_API_KEY" https://fynd-api.propellerheads.xyz/v1/ethereum/health
```

## Quickstart

### 1. Health check

```bash
curl -i -H "Authorization: $FYND_API_KEY" \
  https://fynd-api.propellerheads.xyz/v1/ethereum/health
```

```json
{
  "healthy": true,
  "last_update_ms": 4000,
  "num_solver_pools": 2,
  "derived_data_ready": true,
  "gas_price_age_ms": 16263
}
```

The `-i` flag also prints the response headers, including `X-User-Plan: fynd-basic` (your current plan) and `Retry-After` on 429s.

**Health fields:**

| Field | Meaning |
| --- | --- |
| `healthy` | Overall solver readiness. |
| `last_update_ms` | Milliseconds since the last market-state update from Tycho. `0` means **no update received yet** (the stream is stalled — quotes will return `no_route_found`); non-zero and small is healthy. |
| `num_solver_pools` | Number of parallel algorithm worker pools running. Not liquidity pools — internal solver workers. |
| `derived_data_ready` | Whether derived graph data (token pairs, routes) is built and ready to serve. |
| `gas_price_age_ms` | Age of the cached gas price estimate, in milliseconds. |

### 2. Instance info

```bash
curl -H "Authorization: $FYND_API_KEY" \
  https://fynd-api.propellerheads.xyz/v1/ethereum/info
```

```json
{
  "chain_id": 1,
  "router_address": "0xda892c989d07a18b5dd3f392d949f00df15c5736",
  "permit2_address": "0x000000000022d473030f116ddee9f6b43ac78ba3"
}
```

* `router_address` — the on-chain contract that executes swaps. Submit your encoded swap transaction to this address (it's already set as `transaction.to` in an encoded quote — you don't set it yourself). It is **chain-specific**: each chain returns its own router address.
* `permit2_address` — the [Permit2](https://uniswap.org/blog/permit2) contract address. Used for permit-based token approvals (see [Approvals](#approvals) below).

### 3. Request a quote

Sell 1 WETH for USDC on Ethereum. Amounts are in the token's **smallest unit** (wei for WETH, micro-units for USDC):

```bash
curl -X POST https://fynd-api.propellerheads.xyz/v1/ethereum/quote \
  -H "Authorization: $FYND_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "orders": [
      {
        "token_in":  "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "amount":    "1000000000000000000",
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

{% hint style="info" %}
**Sell orders only.** Fynd currently supports `side: "sell"` (exact input). Buy orders (exact output) are not yet supported.
{% endhint %}

**`sender`** — the address that will submit the swap transaction. The quote (and any encoded calldata) is bound to this sender. The `0x0000…0001` above is a dummy; **replace it with your wallet address** for any quote you intend to execute.

**Quote options** (`options` field — all optional):

| Option | Default | Meaning |
| --- | --- | --- |
| `timeout_ms` | server default | Max time the solver spends finding a route. On timeout, the server returns the best route found so far (or a `timeout` status if none). |
| `min_responses` | server default | Minimum number of solver pools that must respond before the server returns. `1` returns as soon as one pool has a route (fastest). Higher values wait for more pools to compete, improving price at the cost of latency. |
| `encoding_options` | `null` | If set, the response includes a ready-to-submit `transaction` object. See [step 4](#4-encode-and-approve). |
| `max_gas` | `null` | Cap on gas units the route may use. Routes exceeding it are rejected. |

The response contains the route, the expected `amount_out`, gas estimate, and the `solve_time_ms` the server spent finding the route:

```json
{
  "orders": [
    {
      "order_id": "4092e006-67b4-4bcc-b7fd-baacd6f7259d",
      "status": "success",
      "route": {
        "swaps": [
          {
            "component_id": "0x7c85004568584fbf3665f41ebe85146ee0483587d65d9ea5a56c79816bb720d0",
            "protocol": "vm:fermiswap",
            "token_in":  "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "token_out": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "amount_in":  "1000000000000000000",
            "amount_out": "1842438485",
            "gas_estimate": "141239",
            "split": "0"
          }
        ]
      },
      "amount_in":  "1000000000000000000",
      "amount_out": "1842438485",
      "amount_out_net_gas": "1842418197",
      "gas_estimate": "141239",
      "price_impact_bps": 0,
      "block": { "number": 25560140, "hash": "0x7144c8a53f70cf4875864b085055423493671536f494826c7bc62cdd60171013", "timestamp": 1784384351 },
      "gas_price": "78012237",
      "transaction": null
    }
  ],
  "total_gas_estimate": "141239",
  "solve_time_ms": 26
}
```

**Response fields:**

| Field | Meaning |
| --- | --- |
| `solve_time_ms` | Server-side time spent finding the route (excludes network/proxy overhead). |
| `amount_out` | Expected output, in the `token_out`'s smallest unit. |
| `amount_out_net_gas` | `amount_out` minus the gas cost of executing the route, denominated in `token_out`. Use this to compare routes. |
| `gas_estimate` | Gas units the swap will consume. |
| `gas_price` | Gas price (wei) used for the `amount_out_net_gas` calculation. |
| `price_impact_bps` | Price impact of the swap in basis points (100 bps = 1%). |
| `route.swaps[].component_id` | Internal ID for the liquidity venue used (pool/curve/vault). Not directly an on-chain address. |
| `route.swaps[].protocol` | Protocol name. The `vm:` prefix denotes a virtual-machine venue. See [supported protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols). |
| `route.swaps[].split` | Fraction of the input amount routed through this swap. `"0"` means "the remainder of the input" (the last leg of a split group), not 0%. A single-swap route shows `split: "0"`. |
| `block` | The block the quote is valid against. Submit the encoded transaction promptly — see [Quote validity](#quote-validity). |
| `transaction` | `null` unless `encoding_options` is set. See step 4. |
| `order_id` | Correlation ID for this quote. Not queryable after the fact. |

One order per request is the supported path today. The `orders` array shape exists for future batch quoting; for now, send exactly one order.

### 4. Encode and approve

To get a ready-to-submit transaction, pass `encoding_options` with your slippage tolerance (a fraction, as a **string**, e.g. `"0.005"` = 0.5%):

```bash
curl -X POST https://fynd-api.propellerheads.xyz/v1/ethereum/quote \
  -H "Authorization: $FYND_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "orders": [
      {
        "token_in":  "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "amount":    "1000000000000000000",
        "side":      "sell",
        "sender":    "0xYourWalletAddress"
      }
    ],
    "options": {
      "timeout_ms": 5000,
      "min_responses": 1,
      "encoding_options": { "slippage": "0.005" }
    }
  }'
```

{% hint style="warning" %}
**Replace `sender` with your wallet address.** The encoded calldata is bound to the `sender` you provide — a transaction encoded for `0xYourWalletAddress` will revert if submitted by any other address.
{% endhint %}

The response now includes a populated `transaction` and a `fee_breakdown`:

```json
{
  "orders": [
    {
      "status": "success",
      "amount_in":  "1000000000000000000",
      "amount_out": "1844002345",
      "amount_out_net_gas": "1843958233",
      "gas_estimate": "219652",
      "price_impact_bps": 3,
      "block": { "number": 25560394, "hash": "0x17aeb9d6c8d38c46e0e9d6871fcd4bfe7cf6e6cb4fb79eca2fbfe39a0f1d9837", "timestamp": 1784387423 },
      "gas_price": "108908625",
      "route": { "swaps": [ /* ... */ ] },
      "transaction": {
        "to":   "0xda892c989d07a18b5dd3f392d949f00df15c5736",
        "value": "0",
        "data": "0xce25e49e..."
      },
      "fee_breakdown": {
        "router_fee": "18440",
        "client_fee": "0",
        "max_slippage": "9219919",
        "min_amount_received": "1834763986"
      }
    }
  ],
  "total_gas_estimate": "219652",
  "solve_time_ms": 25
}
```

* `transaction.to` is the chain's `router_address` (already set — submit as-is).
* `transaction.data` is the calldata for the swap.
* `transaction.gas` may or may not be populated — if absent, estimate gas separately with `eth_estimateGas` before submitting.
* `fee_breakdown.router_fee` — the fee Fynd charges on the swap, in `token_out` units. The default Fynd fee is **0.1 bps (0.001%)** of swap output; quotes are free. See [Fynd Fees](../guides/router-fees.md) for volume discounts and the full fee arithmetic.
* `fee_breakdown.client_fee` — integrator fee (0 unless you set `client_fee_params` in `encoding_options`; see [Charge Fees on your Swaps](../guides/client-fees.md)).
* `fee_breakdown.max_slippage` — the slippage allowance in `token_out` units, applied to the post-fee amount. `min_amount_received` = `amount_out − router_fee − client_fee − max_slippage`. Verify the settled output is ≥ `min_amount_received` after the transaction confirms.

#### Approvals

Before submitting the swap transaction, the `token_in` must be spendable by the router. The path depends on the `transfer_type` (default `"transfer_from"`):

1. **`transfer_from` (default) — standard ERC-20 approval.** Call `approve(router_address, amount)` on the `token_in` contract, granting the Fynd router an allowance ≥ `amount_in`. The router pulls the exact `amount_in` at execution.
2. **`transfer_from_permit2` — gasless Permit2 signature.** Sign a `PermitSingle` off-chain and pass it via `encoding_options.permit` + `permit2_signature`. No on-chain `approve()` to the router is needed, **but the token must first be approved to the Permit2 contract** (`permit2_address`), not the router. See [Encoding Options](../guides/encoding-options.md) for the full Permit2 flow.

**Native-token sells skip approval entirely.** If `token_in` is the zero address (`0x0000000000000000000000000000000000000000`), the router wraps native ETH/BNB/POL for you — no approval needed.

Quick approval with `cast` (Foundry) — for the default `transfer_from` path:

```bash
# Approve the ROUTER to spend WETH for the swap (default transfer_from path)
cast send 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 \
  "approve(address,uint256)" \
  0xda892c989d07a18b5dd3f392d949f00df15c5736 \
  1000000000000000000 \
  --rpc-url $RPC_URL --private-key $PRIVATE_KEY
```

With approval in place, sign and broadcast `transaction` (the `{to, value, data}` object) from your `sender` wallet. If `transaction.gas` is absent, estimate with `eth_estimateGas` first.

#### Quote validity

The quote is valid against the `block` in the response. Encoded calldata includes a deadline, but market state moves every block — **re-quote within ~1 block (12s on Ethereum, faster on L2s) before submitting.** For execution-critical flows, quote → sign → submit in a single sequence; don't cache quotes for later.

### 5. Sign and execute with a Fynd client

For the full approve → sign → submit → settle flow, the Fynd clients (`@kayibal/fynd-client` for TypeScript, `fynd-client` for Rust) support the hosted API directly — pass your API key and chain to the client builder and the rest is handled.

{% hint style="info" %}
The typed clients expose **camelCase** equivalents of the REST fields (`amountOut`, `solveTimeMs`, `amountOutNetGas`, etc.). The REST reference uses snake_case (`amount_out`, `solve_time_ms`).
{% endhint %}

{% tabs %}
{% tab title="TypeScript" %}
```bash
npm install @kayibal/fynd-client viem
```

```typescript
import {
  FyndClient,
  approvalSigningHash,
  swapSigningHash,
  assembleSignedSwap,
  viemProvider,
} from '@kayibal/fynd-client';
import { createPublicClient, http } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';
import { mainnet } from 'viem/chains';

const WETH  = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2' as const;
const USDC  = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48' as const;
const SELL_AMOUNT = 1000000000000000000n;
const RPC_URL = 'https://eth.llamarpc.com'; // use a dedicated RPC in production

const account = privateKeyToAccount(process.env.PRIVATE_KEY as `0x${string}`);
const publicClient = createPublicClient({ chain: mainnet, transport: http(RPC_URL) });

const client = new FyndClient({
  baseUrl: 'https://fynd-api.propellerheads.xyz',
  apiKey: process.env.FYND_API_KEY!, // sent raw as the Authorization header
  chain: 'ethereum',                 // routes to /v1/ethereum/*
  sender: account.address,
  provider: viemProvider(publicClient, account.address),
  fetchRevertReason: true,
});

// 1. Quote with encoding
const quote = await client.quote({
  order: { tokenIn: WETH, tokenOut: USDC, amount: SELL_AMOUNT, side: 'sell', sender: account.address },
  options: { encodingOptions: { slippage: '0.005' } },
});

// 2. Approve if needed (checks on-chain allowance, skips if sufficient)
const approvalPayload = await client.approval({ token: WETH, amount: SELL_AMOUNT, checkAllowance: true });
if (approvalPayload !== null) {
  const sig = await account.sign({ hash: approvalSigningHash(approvalPayload) });
  await client.executeApproval({ tx: approvalPayload.tx, signature: sig });
}

// 3. Sign and execute swap
const payload = await client.swapPayload(quote);
const sig = await account.sign({ hash: swapSigningHash(payload) });
const settled = await (await client.executeSwap(assembleSignedSwap(payload, sig))).settle();
console.log(`settled: ${settled.settledAmount}, gas: ${settled.gasCost}`);
```

Full example: [`clients/typescript/examples/tutorial/main.ts`](https://github.com/propeller-heads/fynd/blob/main/clients/typescript/examples/tutorial/main.ts).
{% endtab %}

{% tab title="Rust" %}
```toml
# Cargo.toml
[dependencies]
fynd-client = "0.92"
tokio = { version = "1", features = ["full"] }
anyhow = "1"
alloy = { version = "0.3", features = ["full"] }
num-bigint = "0.4"
```

```rust
use fynd_client::{
    FyndClientBuilder, QuoteParams, Order, OrderSide, QuoteOptions, EncodingOptions,
    ApprovalParams, SigningHints, SignedApproval, SignedSwap, ExecutionOptions,
};
use alloy::{
    primitives::{Address, Bytes},
    signers::local::PrivateKeySigner,
    signers::Signer,
};
use num_bigint::BigUint;
use std::str::FromStr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token_in:  Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse()?;
    let token_out: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse()?;
    let sell_amount = BigUint::from(1_000_000_000_000_000_000u64);
    let rpc_url = std::env::var("RPC_URL")?;
    let sender: Address = "0xYourWalletAddress".parse()?;
    let signer = PrivateKeySigner::from_str(&std::env::var("PRIVATE_KEY")?)?;

    let client = FyndClientBuilder::new("https://fynd-api.propellerheads.xyz")
        .with_api_key(std::env::var("FYND_API_KEY")?) // sent raw, no Bearer prefix
        .with_chain("ethereum")                       // routes to /v1/ethereum/*
        .with_rpc_url(&rpc_url)
        .with_sender(sender)
        .build()
        .await?;

    // 1. Quote with encoding
    let quote = client
        .quote(QuoteParams::new(
            Order::new(
                Bytes::copy_from_slice(token_in.as_slice()),
                Bytes::copy_from_slice(token_out.as_slice()),
                sell_amount.clone(),
                OrderSide::Sell,
                sender,
                None, // receiver: None means same as sender
            ),
            QuoteOptions::default()
                .with_timeout_ms(5_000)
                .with_encoding_options(EncodingOptions::new("0.005")),
        ))
        .await?;

    // 2. Approve if needed (checks on-chain allowance, skips if sufficient)
    if let Some(approval_payload) = client
        .approval(
            &ApprovalParams::new(
                Bytes::copy_from_slice(token_in.as_slice()),
                sell_amount,
                true, // check allowance first
            ),
            &SigningHints::default(),
        )
        .await?
    {
        let sig = signer.sign_hash(&approval_payload.signing_hash()).await?;
        client.execute_approval(SignedApproval::assemble(approval_payload, sig)).await?.await?;
    }

    // 3. Sign and execute swap
    let payload = client.swap_payload(quote, &SigningHints::default()).await?;
    let sig = signer.sign_hash(&payload.signing_hash()).await?;
    let receipt = client
        .execute_swap(SignedSwap::assemble(payload, sig), &ExecutionOptions::default())
        .await?
        .await?;
    println!("gas: {}", receipt.gas_cost());
    Ok(())
}
```

Full example: [`clients/rust/examples/swap_erc20.rs`](https://github.com/propeller-heads/fynd/blob/main/clients/rust/examples/swap_erc20.rs).
{% endtab %}
{% endtabs %}

## Supported chains

Each chain is served from its own Fynd backend behind the shared gateway. Send the chain name in the URL path (`/v1/{chain}/…`). With the typed clients, set the `chain` option (TypeScript) or `.with_chain("…")` (Rust).

| Chain | `chain` path segment | Chain ID | Native token |
| --- | --- | ---: | --- |
| Ethereum | `ethereum` | 1 | ETH |
| Base | `base` | 8453 | ETH |
| Arbitrum | `arbitrum` | 42161 | ETH |
| BNB Smart Chain | `bsc` | 56 | BNB |
| Polygon | `polygon` | 137 | POL |
| Unichain | `unichain` | 130 | ETH |

A request to `/v1/{chain}/…` for a chain that isn't configured returns `404` with `{"error": "unknown_chain", ...}`.

### Native token swaps

Use the zero address (`0x0000000000000000000000000000000000000000`) as `token_in` or `token_out` to swap the chain's native gas token (ETH on Ethereum/Base/Arbitrum/Unichain, BNB on BSC, POL on Polygon). Native-token `token_in` skips the approval step — the router wraps native gas for you.

## API key limits by tier

Your API key belongs to a **plan** that sets your rate limit. Limits are enforced per key by a token-bucket limiter at the gateway.

| Plan | Requests / second | Burst | How to get |
| --- | ---: | ---: | --- |
| **fynd-basic** (default) | 10 | 2× (20) | Self-service via [@FyndPortalBot](https://t.me/fynd_portal_bot) |
| **scale** | 25 | 2× (50) | Request via [@tanay_j](https://t.me/tanay_j) or [our Telegram group](https://t.me/+B4CNQwv7dgIyYTJl) |
| **transition** | 50 | 4× (200) | Request via [@tanay_j](https://t.me/tanay_j) or [our Telegram group](https://t.me/+B4CNQwv7dgIyYTJl) |

{% hint style="info" %}
**Burst** is the bucket capacity = `rps × burst_multiplier`. A `fynd-basic` key can briefly sustain 20 requests in one second before being throttled back to the steady-state 10 rps. **All requests count against the bucket** — `/health`, `/info`, and `/quote` alike. The bucket is **per key, shared across all chains**.
{% endhint %}

When you exceed the limit you get `429 Too Many Requests` with a `Retry-After` header (seconds). The hosted API is **REST-only** today (no WebSocket); if you need streaming quotes, run your own stack (see below) or talk to us about a dedicated instance.

**Pricing:** The hosted API is **free to use during beta** — no platform fee on top of Fynd's standard 0.1 bps router fee on executed swaps (see [Fynd Fees](../guides/router-fees.md)). Quotes are always free. `scale` and `transition` are allocated case-by-case — reach out to discuss volume and needs.

### Checking your plan

The gateway returns your plan in the `X-User-Plan` response header on every authenticated request (use `curl -i` to see it). If you're unsure which plan your key is on, message [@FyndPortalBot](https://t.me/fynd_portal_bot) or ask in [our Telegram group](https://t.me/+B4CNQwv7dgIyYTJl).

## Errors

| HTTP status | Meaning | Body format |
| ---: | --- | --- |
| 400 | Malformed request body, bad token address, or invalid `slippage` type | JSON: `{"error": "...", "code": "BAD_REQUEST"}` |
| 401 | Missing or invalid API key, or plan not recognized | Plain text: `Unauthorized` or `Missing authorization token` |
| 403 | Your plan doesn't allow this service | Plain text: `Forbidden` |
| 404 | Unknown chain path segment | JSON: `{"error": "unknown_chain", ...}` |
| 429 | Rate limit exceeded | Plain text: `Too Many Requests`, plus `Retry-After: <seconds>` header |
| 5xx | Backend or gateway failure | varies — check health and retry with backoff |

{% hint style="warning" %}
**Error body formats differ by layer.** Gateway errors (401, 403, 429) return plain text; backend errors (400, 404, 5xx) return JSON. If your client assumes JSON on every non-2xx, it will throw a parse error on the auth and rate-limit paths you most need to handle. Parse defensively.
{% endhint %}

A `200` with `orders[0].status: "no_route_found"` is **not** an HTTP error — it means the solver ran but couldn't find a profitable route for the pair at the requested size. Check `/v1/{chain}/health` (`last_update_ms: 0` means the Tycho stream isn't delivering live state yet), try a different token pair or size, or confirm the tokens have [Tycho-indexed liquidity](https://docs.propellerheads.xyz/tycho) on that chain.

## Scaling beyond the hosted tiers

The hosted API is a shared, beta-rate-limited service designed for getting started and for moderate-volume integrations. If you need:

* **Higher or unlimited rate limits** — run Fynd on your own hardware. The [self-host quickstart](../quickstart/README.md) has you serving in minutes, and there's no rate limiter in front of your own instance.
* **Custom algorithms, custom protocol sets, or RFQ integration** — self-host and pass `--protocols` / your algorithm config. See [Server Configuration](../guides/server-configuration.md) and [Custom Algorithm](../guides/custom-algorithm.md).
* **Streaming quotes over WebSocket** — self-host and use `/v1/ws`; the hosted API is REST-only today.
* **A dedicated hosted instance with guaranteed capacity and SLA** — reach out to our business team. Message [@tanay_j](https://t.me/tanay_j) on Telegram, or email us at [Propeller Heads](https://www.propellerheads.xyz).

What the hosted API buys you (vs. self-hosting): no infrastructure to run, no Tycho indexer endpoint to source, no release tracking, and per-chain routing tuned by the Fynd team. Self-hosting gives you ~1,000 RPS on commodity hardware (see [Performance](../reference/benchmark-results.md)) and full control — pick the tradeoff that fits your scale.

## FAQ

**Is the hosted API the same software as self-hosted Fynd?**
Yes. Identical binary, same quote API. The hosted gateway adds a `/{chain}` path segment, API-key auth, and rate limiting — none of which exist in the self-hosted binary. The per-chain request/response bodies are identical.

**How do I know which Fynd version is running?**
The unauthenticated endpoint `https://fynd-api.propellerheads.xyz/api-docs/openapi.json` exposes the version under `info.version`. We update the hosted backends shortly after each release. During beta the surface area may change; we post breaking changes in [our Telegram group](https://t.me/+B4CNQwv7dgIyYTJl) ahead of rollout.

**Can I use one key for all chains?**
Yes. A single API key works against every `/v1/{chain}/…` path. The rate-limit bucket is shared across chains.

**Why did my quote return `no_route_found`?**
The solver found no profitable route for the pair at the requested size on that chain. Common causes: thin [Tycho](https://docs.propellerheads.xyz/tycho)-indexed liquidity for the pair, an amount too large or too small, or the chain's backend is still warming up. Check `/v1/{chain}/health` — `last_update_ms: 0` indicates the Tycho stream isn't delivering live state yet.

**Can I bring my own RPC and Tycho endpoint with the hosted API?**
No — the hosted API uses Propeller Heads' Tycho endpoints. To use your own, self-host.

## Next steps

* [API reference](../reference/api.md) — full OpenAPI spec
* [Encoding Options](../guides/encoding-options.md) — turn a quote into a submittable transaction (Permit2, transfer types, price guard)
* [Fynd Fees](../guides/router-fees.md) — fees Fynd charges on executed swaps
* [Charge Fees on your Swaps](../guides/client-fees.md) — add an integrator fee
* [Self-host Fynd](../quickstart/README.md) — when you're ready to outgrow the hosted tiers