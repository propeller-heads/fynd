/**
 * Example: quote and execute a USDC → WETH swap using ERC-20 `transferFrom`.
 *
 * Checks on-chain allowance and submits an approval transaction if needed,
 * then signs and executes the swap.
 *
 * Requires a funded wallet and a running Fynd instance.
 */

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
const RPC_URL = 'https://eth.llamarpc.com';
const USDC = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48';
const WETH = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2';
const SELL_AMOUNT = parseUnits('1000', 6); // 1000 USDC

const account = privateKeyToAccount(process.env.PRIVATE_KEY as `0x${string}`);

const publicClient = createPublicClient({ chain: mainnet, transport: http(RPC_URL) });
const walletClient = createWalletClient({ account, chain: mainnet, transport: http(RPC_URL) });

const client = new FyndClient({
  baseUrl: FYND_URL,
  chainId: mainnet.id,
  sender: account.address,
  provider: viemProvider(publicClient, account.address),
});

// 1. Quote
const quote = await client.quote({
  order: {
    tokenIn: USDC,
    tokenOut: WETH,
    amount: SELL_AMOUNT,
    side: 'sell',
    sender: account.address,
  },
  options: { encodingOptions: encodingOptions(0.005) },
});
console.log(`amount_out: ${quote.amountOut}`);

// 2. Approve if needed (checks on-chain allowance, skips if sufficient)
const approvalPayload = await client.approval({
  token: USDC,
  amount: SELL_AMOUNT,
  checkAllowance: true,
});
if (approvalPayload !== null) {
  const approvalHash = approvalSigningHash(approvalPayload);
  const approvalSig = await walletClient.signMessage({ message: { raw: approvalHash } });
  await client.executeApproval({ tx: approvalPayload.tx, signature: approvalSig });
  console.log('approval confirmed');
}

// 3. Sign and execute swap
const payload = await client.swapPayload(quote);
const swapHash = swapSigningHash(payload);
const sig = await walletClient.signMessage({ message: { raw: swapHash } });
const receipt = await client.executeSwap(assembleSignedSwap(payload, sig));
const settled = await receipt.settle();
console.log(`gas: ${settled.gasCost}`);
