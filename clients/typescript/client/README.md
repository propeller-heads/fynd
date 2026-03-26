# @kayibal/fynd-client

TypeScript client for the [Fynd](https://fynd.xyz) DEX router.

Request swap quotes, build signable transaction payloads, and broadcast signed orders through
the Fynd RPC API — all from a single typed interface.

For documentation, guides, and API reference visit **<https://docs.fynd.xyz/>**.

## Installation

```bash
npm install @kayibal/fynd-client
# or
pnpm add @kayibal/fynd-client
```

## Quick start

```typescript
import { FyndClient, encodingOptions } from "@kayibal/fynd-client";
import { createPublicClient, http } from "viem";
import { mainnet } from "viem/chains";

// Start a local Fynd instance first: https://docs.fynd.xyz/get-started/quickstart
const client = new FyndClient({
  baseUrl: "http://localhost:3000",
  chainId: mainnet.id,
  sender: "0xYourAddress",
  rpcUrl: "https://reth-ethereum.ithaca.xyz/rpc",
});

// 1. Quote
const quote = await client.quote({
  order: {
    tokenIn:  "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", // WETH
    tokenOut: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", // USDC
    amount:   1_000_000_000_000_000_000n,                    // 1 WETH
    side:     "sell",
    sender:   "0xYourAddress",
  },
  options: { encodingOptions: encodingOptions(0.005) },
});
console.log("amount out:", quote.amountOut);

// 2. Approve if needed
const approval = await client.approval({
  token: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  amount: 1_000_000_000_000_000_000n,
  checkAllowance: true,
});

// 3. Sign and execute
const payload  = await client.swapPayload(quote);
const sig      = await wallet.sign({ hash: swapSigningHash(payload) });
const settled  = await (await client.executeSwap(assembleSignedSwap(payload, sig))).settle();
console.log("settled:", settled.settledAmount, "gas:", settled.gasCost);
```

See the [full example](examples/tutorial/main.ts) for the complete flow including approvals.
