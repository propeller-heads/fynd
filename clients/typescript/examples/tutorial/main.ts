// Example: ERC-20 swap with the Fynd TypeScript client.
//
// Run with:
//   RPC_URL=https://eth.llamarpc.com PRIVATE_KEY=0x... npx tsx main.ts

import { createPublicClient, createWalletClient, http, parseUnits } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';
import { mainnet } from 'viem/chains';
import {
  FyndClient,
  approvalSigningHash,
  assembleSignedSwap,
  encodingOptions,
  swapSigningHash,
  viemProvider,
} from '@fynd/client';

const FYND_URL = 'http://localhost:3000';
const USDC = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48';
const WETH = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2';
const SELL_AMOUNT = parseUnits('1000', 6); // 1000 USDC

const account = privateKeyToAccount(process.env['PRIVATE_KEY'] as `0x${string}`);
const rpcUrl = process.env['RPC_URL'] ?? 'https://eth.llamarpc.com';

const publicClient = createPublicClient({ chain: mainnet, transport: http(rpcUrl) });
const walletClient = createWalletClient({ account, chain: mainnet, transport: http(rpcUrl) });

// [doc:start swap-typescript]
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
console.log(`settled: ${settled.settledAmount}, gas: ${settled.gasCost}`);
// [doc:end swap-typescript]
