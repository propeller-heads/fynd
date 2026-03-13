// Tutorial: Quote, Sign & Execute Swaps with the Fynd TypeScript Client
//
// This example demonstrates how to:
// 1. Create a FyndClient pointing at a running solver
// 2. Get a swap quote with server-side encoding (encoding options)
// 3. Sign the transaction with Permit2 authorization
// 4. Simulate or execute the swap on-chain
//
// Prerequisites:
//   - A running fynd solver (see the Rust tutorial README for setup)
//   - Environment variables (see below)
//
// Environment variables:
//   SOLVER_URL    - Solver API URL (default: http://localhost:3000)
//   RPC_URL       - Ethereum RPC endpoint (required for simulation/execution)
//   PRIVATE_KEY   - Wallet private key, hex without 0x prefix (required)
//   CHAIN_ID      - Chain ID (default: 1 for Ethereum mainnet)

import {
  type Address,
  type Hex,
  type Quote,
  FyndClient,
  encodingOptions,
  withPermit2,
  permit2SigningHash,
  signingHash,
  assembleSignedOrder,
} from "@fynd/client";
import {
  createPublicClient,
  createWalletClient,
  http,
  formatUnits,
  parseUnits,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { mainnet } from "viem/chains";

// Well-known token addresses on Ethereum mainnet
const USDC: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const WETH: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

// Canonical Permit2 address (same on all chains)
const PERMIT2: Address = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

// Token metadata for display formatting
const TOKEN_INFO: Record<string, { symbol: string; decimals: number }> = {
  [USDC.toLowerCase()]: { symbol: "USDC", decimals: 6 },
  [WETH.toLowerCase()]: { symbol: "WETH", decimals: 18 },
};

// ============================================================================
// Configuration
// ============================================================================

interface Config {
  solverUrl: string;
  rpcUrl: string;
  privateKey: Hex;
  chainId: number;
  sellToken: Address;
  buyToken: Address;
  sellAmount: string;
  slippageBps: number;
  simulateOnly: boolean;
}

function loadConfig(): Config {
  const privateKey = process.env["PRIVATE_KEY"];
  if (privateKey === undefined) {
    throw new Error(
      "PRIVATE_KEY environment variable is required. " +
        "Set it to your wallet private key (hex, no 0x prefix)."
    );
  }
  const rpcUrl = process.env["RPC_URL"];
  if (rpcUrl === undefined) {
    throw new Error(
      "RPC_URL environment variable is required for simulation/execution."
    );
  }

  return {
    solverUrl: process.env["SOLVER_URL"] ?? "http://localhost:3000",
    rpcUrl,
    privateKey: `0x${privateKey.replace(/^0x/, "")}` as Hex,
    chainId: Number(process.env["CHAIN_ID"] ?? "1"),
    sellToken: (process.env["SELL_TOKEN"] as Address | undefined) ?? USDC,
    buyToken: (process.env["BUY_TOKEN"] as Address | undefined) ?? WETH,
    sellAmount: process.env["SELL_AMOUNT"] ?? "100",
    slippageBps: Number(process.env["SLIPPAGE_BPS"] ?? "50"),
    simulateOnly: process.env["SIMULATE_ONLY"] === "true",
  };
}

// ============================================================================
// Display helpers
// ============================================================================

function tokenSymbol(address: Address): string {
  return TOKEN_INFO[address.toLowerCase()]?.symbol ?? address.slice(0, 10);
}

function tokenDecimals(address: Address): number {
  return TOKEN_INFO[address.toLowerCase()]?.decimals ?? 18;
}

function formatAmount(raw: bigint, address: Address): string {
  return formatUnits(raw, tokenDecimals(address));
}

function displayQuote(
  quote: Quote,
  sellToken: Address,
  buyToken: Address,
): void {
  console.log("\n========== Quote ==========");
  console.log(`Status: ${quote.status}`);

  if (quote.status !== "success") {
    console.log("No route found.");
    console.log("============================\n");
    return;
  }

  const formattedIn = formatAmount(quote.amountIn, sellToken);
  const formattedOut = formatAmount(quote.amountOut, buyToken);
  console.log(
    `Swap: ${formattedIn} ${tokenSymbol(sellToken)}` +
      ` -> ${formattedOut} ${tokenSymbol(buyToken)}`
  );

  const decIn = Number(formattedIn);
  const decOut = Number(formattedOut);
  if (decIn > 0) {
    const price = decOut / decIn;
    console.log(
      `Price: ${price.toFixed(6)} ${tokenSymbol(buyToken)}` +
        ` per ${tokenSymbol(sellToken)}`
    );
  }

  console.log(`Gas estimate: ${quote.gasEstimate}`);

  if (quote.priceImpactBps !== undefined) {
    console.log(`Price impact: ${(quote.priceImpactBps / 100).toFixed(2)}%`);
  }

  if (quote.route !== undefined) {
    console.log(`\nRoute (${quote.route.swaps.length} hops):`);
    for (const [i, swap] of quote.route.swaps.entries()) {
      console.log(
        `  ${i + 1}. ${tokenSymbol(swap.tokenIn)}` +
          ` -> ${tokenSymbol(swap.tokenOut)}` +
          ` via ${swap.protocol} (pool: ${swap.poolId})`
      );
    }
  }

  if (quote.transaction !== undefined) {
    console.log(`\nTransaction encoded:`);
    console.log(`  to: ${quote.transaction.to}`);
    console.log(`  value: ${quote.transaction.value}`);
    console.log(
      `  data: ${quote.transaction.data.slice(0, 42)}...` +
        ` (${(quote.transaction.data.length - 2) / 2} bytes)`
    );
  }

  console.log("============================\n");
}

// ============================================================================
// Main flow
// ============================================================================

async function main(): Promise<void> {
  const config = loadConfig();

  // Step 1: Set up viem account and providers
  const account = privateKeyToAccount(config.privateKey);
  console.log(`Wallet address: ${account.address}`);

  const publicClient = createPublicClient({
    chain: mainnet,
    transport: http(config.rpcUrl),
  });

  const walletClient = createWalletClient({
    account,
    chain: mainnet,
    transport: http(config.rpcUrl),
  });

  // Step 2: Create FyndClient with an EthProvider adapter for viem
  const fyndClient = new FyndClient({
    baseUrl: config.solverUrl,
    chainId: config.chainId,
    sender: account.address,
    provider: {
      async getTransactionCount({ address }) {
        return publicClient.getTransactionCount({ address });
      },
      async estimateFeesPerGas() {
        const fees = await publicClient.estimateFeesPerGas();
        return {
          maxFeePerGas: fees.maxFeePerGas ?? 0n,
          maxPriorityFeePerGas: fees.maxPriorityFeePerGas ?? 0n,
        };
      },
      async call(tx) {
        const result = await publicClient.call({
          to: tx.to,
          data: tx.data,
          value: tx.value,
          gas: tx.gas,
          maxFeePerGas: tx.maxFeePerGas,
          maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
        });
        const hex = result.data as Hex | undefined;
        return hex !== undefined ? { data: hex } : {};
      },
      async estimateGas(tx) {
        return publicClient.estimateGas({
          account: account.address,
          to: tx.to,
          data: tx.data,
          value: tx.value,
          maxFeePerGas: tx.maxFeePerGas,
          maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
        });
      },
      async sendRawTransaction(rawTx) {
        return publicClient.sendRawTransaction({ serializedTransaction: rawTx });
      },
      async getTransactionReceipt({ hash }) {
        const receipt = await publicClient.getTransactionReceipt({ hash });
        return {
          transactionHash: receipt.transactionHash,
          gasUsed: receipt.gasUsed,
          effectiveGasPrice: receipt.effectiveGasPrice,
          logs: receipt.logs.map((log) => ({
            address: log.address as Address,
            topics: log.topics as readonly Hex[],
            data: log.data as Hex,
          })),
        };
      },
    },
  });

  // Step 3: Check solver health
  console.log(`\nChecking solver health at ${config.solverUrl}...`);
  const health = await fyndClient.health();
  console.log(
    `Solver healthy: ${String(health.healthy)},` +
      ` last update: ${health.lastUpdateMs}ms ago,` +
      ` ${health.numSolverPools} pools`
  );
  if (!health.healthy) {
    throw new Error("Solver is not healthy. Wait for market data to load.");
  }

  // Step 4: Build Permit2 encoding options with signature
  //
  // The solver needs the Permit2 permit + signature included in the
  // encoding options so it can encode the calldata server-side.
  // We construct the permit, sign it via EIP-712, and attach both to
  // the quote request.
  const sellDecimals = tokenDecimals(config.sellToken);
  const amountIn = parseUnits(config.sellAmount, sellDecimals);
  const slippage = config.slippageBps / 10_000;

  // Read the Permit2 nonce for the sell token from on-chain.
  // For simplicity we use nonce 0; a production app would read
  // the current nonce from the Permit2 contract.
  const permit = {
    details: {
      token: config.sellToken,
      amount: amountIn,
      expiration: BigInt(Math.floor(Date.now() / 1000) + 3600),
      nonce: 0n,
    },
    spender: "0x0000000000000000000000000000000000000000" as Address,
    sigDeadline: BigInt(Math.floor(Date.now() / 1000) + 3600),
  };

  // Sign the Permit2 message (EIP-712)
  const permitHash = permit2SigningHash(permit, config.chainId, PERMIT2);
  const permitSig = await account.signMessage({
    message: { raw: permitHash },
  });

  // Build encoding options: slippage + permit2
  const encOpts = withPermit2(
    encodingOptions(slippage),
    permit,
    permitSig,
  );

  // Step 5: Request a quote with encoding options
  //
  // When encoding options are provided, the solver returns a Transaction
  // object with pre-built calldata. Without encoding options you only
  // get the route/pricing.
  console.log(
    `\nGetting quote: ${config.sellAmount}` +
      ` ${tokenSymbol(config.sellToken)}` +
      ` -> ${tokenSymbol(config.buyToken)}`
  );

  const quote = await fyndClient.quote({
    order: {
      tokenIn: config.sellToken,
      tokenOut: config.buyToken,
      amount: amountIn,
      side: "sell",
      sender: account.address,
    },
    options: {
      encodingOptions: encOpts,
    },
  });

  displayQuote(quote, config.sellToken, config.buyToken);

  if (quote.status !== "success") {
    throw new Error(`Quote failed with status: ${quote.status}`);
  }
  if (quote.transaction === undefined) {
    throw new Error(
      "Quote has no transaction data. " +
        "Ensure encodingOptions were provided in the quote request."
    );
  }

  // Step 6: Build a signable payload (EIP-1559 transaction)
  //
  // signablePayload fills in nonce, gas fees, and gas limit from the
  // provider, then wraps everything into a structure ready for signing.
  console.log("Building signable payload...");
  const payload = await fyndClient.signablePayload(quote, {
    simulate: config.simulateOnly,
  });

  // Step 7: Sign the EIP-1559 transaction
  const txHash = signingHash(payload);
  const txSig = await account.signMessage({
    message: { raw: txHash },
  });
  const signedOrder = assembleSignedOrder(payload, txSig);

  if (config.simulateOnly) {
    console.log("\nSimulation passed (via signablePayload simulate hint).");
    console.log("Set SIMULATE_ONLY=false to execute on-chain.");
    return;
  }

  // Step 8: Execute the swap
  //
  // execute() serializes the signed EIP-1559 transaction and submits it
  // via sendRawTransaction. The returned receipt has a settle() method
  // that polls for confirmation.
  console.log("\nSubmitting transaction...");
  const receipt = await fyndClient.execute(signedOrder);
  console.log("Transaction submitted. Waiting for confirmation...");

  const settled = await receipt.settle();
  console.log("\nSwap executed!");
  if (settled.txHash !== undefined) {
    console.log(`  TX hash: ${settled.txHash}`);
  }
  console.log(`  Gas cost: ${settled.gasCost} wei`);
  if (settled.settledAmount !== undefined) {
    console.log(
      `  Received: ${formatAmount(settled.settledAmount, config.buyToken)}` +
        ` ${tokenSymbol(config.buyToken)}`
    );
  }
}

main().catch((err: unknown) => {
  console.error("Fatal:", err);
  process.exit(1);
});
