// Tutorial: Quote, Sign & Execute Swaps with the Fynd TypeScript Client
//
// Environment variables:
//   SOLVER_URL    - Solver API URL (default: http://localhost:3000)
//   RPC_URL       - Ethereum RPC endpoint (required)
//   PRIVATE_KEY   - Wallet private key, hex (required)
//   CHAIN_ID      - Chain ID (default: 1)

import {
  type Address,
  type Hex,
  FyndClient,
  encodingOptions,
  withPermit2,
  permit2SigningHash,
  signingHash,
  assembleSignedOrder,
  viemProvider,
} from "@fynd/client";
import { createPublicClient, http, parseUnits } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { mainnet } from "viem/chains";

const USDC: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const WETH: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const PERMIT2: Address = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

async function main(): Promise<void> {
  const rpcUrl = process.env["RPC_URL"];
  if (rpcUrl === undefined) {
    throw new Error("RPC_URL environment variable is required.");
  }
  const rawKey = process.env["PRIVATE_KEY"];
  if (rawKey === undefined) {
    throw new Error("PRIVATE_KEY environment variable is required.");
  }

  const solverUrl = process.env["SOLVER_URL"] ?? "http://localhost:3000";
  const chainId = Number(process.env["CHAIN_ID"] ?? "1");
  const privateKey = `0x${rawKey.replace(/^0x/, "")}` as Hex;
  const account = privateKeyToAccount(privateKey);
  console.log(`Wallet: ${account.address}`);

  const publicClient = createPublicClient({
    chain: mainnet,
    transport: http(rpcUrl),
  });

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

  if (quote.transaction === undefined) {
    throw new Error("No transaction data — ensure encodingOptions are set.");
  }

  // Build signable payload, sign, and assemble
  const payload = await fyndClient.signablePayload(quote);
  const txHash = signingHash(payload);
  const txSig = await account.signMessage({ message: { raw: txHash } });
  const signedOrder = assembleSignedOrder(payload, txSig);

  // Execute on-chain
  console.log("\nSubmitting transaction...");
  const receipt = await fyndClient.execute(signedOrder);
  const settled = await receipt.settle();

  console.log("Swap executed!");
  if (settled.txHash !== undefined) {
    console.log(`  TX: ${settled.txHash}`);
  }
  console.log(`  Gas cost: ${settled.gasCost} wei`);
  if (settled.settledAmount !== undefined) {
    console.log(`  Received: ${settled.settledAmount}`);
  }
}

main().catch((err: unknown) => {
  console.error("Fatal:", err);
  process.exit(1);
});
