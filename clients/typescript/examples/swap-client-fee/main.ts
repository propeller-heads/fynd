// Example: sell 1 WETH for USDC with a client fee.
//
// The EIP-712 `ClientFee` message covers swap-specific fields (`amountIn`, `tokenIn`,
// `tokenOut`, `expectedAmountOut`, `minAmountOut`, `receiver`, `bytes swaps`) that are only
// known once the server has encoded the transaction. The server encodes a zeroed 65-byte
// signature placeholder and reports its byte offset, so one quote request is enough:
//
//   1. Quote with unsigned client fee params.
//   2. Sign the 11-field EIP-712 hash using `swapsHash` from the response.
//   3. Patch the signature into the calldata with `patchClientFeeSignature`.
//   4. Execute.
//
// Two keys are used: the dev key as the sender, and a random ephemeral key as the fee
// receiver (in production this is the integrator's key). The fee receiver needs no funds —
// fees accrue to its vault balance, and `maxContribution: 0n` means it subsidizes nothing.
//
// Run with Anvil (mocked accounts), which needs TYCHO_API_KEY (and optionally TYCHO_URL):
//   ./scripts/run-all-examples.sh
//
// Run against a wallet of your own, with a Fynd server already running:
//   PRIVATE_KEY=0x... npx tsx main.ts

import { createPublicClient, http, parseUnits } from 'viem';
import { generatePrivateKey, privateKeyToAccount } from 'viem/accounts';
import { mainnet } from 'viem/chains';
import {
  FyndClient,
  approvalSigningHash,
  assembleSignedSwap,
  clientFeeSigningHash,
  encodingOptions,
  patchClientFeeSignature,
  swapSigningHash,
  viemProvider,
  withClientFee,
} from '@kayibal/fynd-client';
import type { ClientFeeParams } from '@kayibal/fynd-client';

const WETH = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2';
const USDC = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48';
const SELL_AMOUNT = parseUnits('1', 18); // 1 WETH
const SLIPPAGE = 0.01; // 1%
const FEE_BPS = 50; // 0.5% client fee

const fyndUrl = process.env['FYND_URL'] ?? 'http://localhost:3000';
const rpcUrl = process.env['RPC_URL'] ?? 'http://localhost:8545';
const account = privateKeyToAccount(process.env['PRIVATE_KEY'] as `0x${string}`);

// Separate fee receiver key — in production this is the integrator's key.
const feeAccount = privateKeyToAccount(generatePrivateKey());

const publicClient = createPublicClient({ chain: mainnet, transport: http(rpcUrl) });
const client = new FyndClient({
  baseUrl: fyndUrl,
  sender: account.address,
  provider: viemProvider(publicClient, account.address),
  fetchRevertReason: true,
});

const info = await client.info();
if (info.routerAddress === null) {
  throw new Error('example requires a chain with encoding support');
}
const routerAddress = info.routerAddress;
const chainId = info.chainId;

// Approve the router to spend WETH if the current allowance is insufficient.
const approvalPayload = await client.approval({
  token: WETH,
  amount: SELL_AMOUNT,
  checkAllowance: true,
});
if (approvalPayload !== null) {
  console.log('Approving router to spend WETH...');
  const approvalSig = await account.sign({ hash: approvalSigningHash(approvalPayload) });
  await client.executeApproval({ tx: approvalPayload.tx, signature: approvalSig });
  console.log('Approved.');
}

// [doc:start client-fee-typescript]
// Step 1: request a quote using unsigned client fee params. The server encodes the full
// calldata with a 65-byte signature placeholder and returns `swapsHash` in the fee breakdown
// plus `clientFeeSignatureOffset` in the transaction, so the client can patch the real
// signature in.
const feeParams: ClientFeeParams = {
  bps: FEE_BPS,
  receiver: feeAccount.address,
  maxContribution: 0n, // no vault subsidy
  deadline: Math.floor(Date.now() / 1000) + 3600,
};
const quote = await client.quote({
  order: {
    tokenIn: WETH,
    tokenOut: USDC,
    amount: SELL_AMOUNT,
    side: 'sell',
    sender: account.address,
  },
  options: { encodingOptions: withClientFee(encodingOptions(SLIPPAGE), feeParams) },
});

const feeBreakdown = quote.feeBreakdown;
if (feeBreakdown?.swapsHash === undefined) {
  throw new Error('no swapsHash — server must support client fee signing');
}

// Step 2: sign the full 11-field EIP-712 ClientFee hash with the fee receiver's key.
// `quote.receiver` defaults to the sender when the order has no explicit receiver.
const hash = clientFeeSigningHash(feeParams, chainId, routerAddress, {
  amountIn: quote.amountIn,
  tokenIn: WETH,
  tokenOut: USDC,
  expectedAmountOut: quote.amountOut,
  minAmountOut: feeBreakdown.minAmountReceived,
  receiver: quote.receiver,
  swapsHash: feeBreakdown.swapsHash,
});
// `sign` signs the digest as-is. `signMessage` would add the EIP-191 prefix and the router
// would recover the wrong signer.
const signature = await feeAccount.sign({ hash });

// Step 3: patch the real signature into the calldata — no second quote request.
const signed = patchClientFeeSignature(quote, signature);
// [doc:end client-fee-typescript]

console.log(`amount_in:  ${signed.amountIn}`);
console.log(`amount_out: ${signed.amountOut}`);
console.log(`client_fee: ${feeBreakdown.clientFee}`);

// Sign and execute the swap.
const payload = await client.swapPayload(signed, { simulate: true });
const txSig = await account.sign({ hash: swapSigningHash(payload) });
const settled = await (await client.executeSwap(assembleSignedSwap(payload, txSig))).settle();
console.log(`settled:    ${settled.settledAmount}, gas: ${settled.gasCost}`);
