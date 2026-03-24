import { keccak256, serializeTransaction } from 'viem';
import type { Address, Hex, Quote } from './types.js';

/** An unsigned EIP-1559 `approve(spender, amount)` transaction. */
export interface ApprovalPayload {
  tx: Eip1559Transaction;
  token: Address;
  spender: Address;
  amount: bigint;
  /** Set only when {@link ApprovalParams.checkAllowance} was `true`. */
  isNeeded?: boolean;
}

/** A signed approval ready for {@link FyndClient.executeApproval}. */
export interface SignedApproval {
  tx: Eip1559Transaction;
  /** 65-byte hex signature over the EIP-1559 signing hash. */
  signature: Hex;
}

/** Receipt for a mined transaction (gas cost only, no settled-amount). */
export interface TxReceipt {
  txHash: Hex;
  gasCost: bigint;
}

/** An unsigned EIP-1559 transaction ready for signing. */
export interface Eip1559Transaction {
  chainId: number;
  nonce: number;
  maxFeePerGas: bigint;
  maxPriorityFeePerGas: bigint;
  gas: bigint;
  to: Address;
  value: bigint;
  data: Hex;
}

/** Internal payload pairing a quote with its unsigned transaction. */
export interface FyndPayload {
  /** Original quote; carries `tokenOut` and `receiver` needed for settlement parsing. */
  quote: Quote;
  tx: Eip1559Transaction;
}

/** Discriminated union of backend-specific swap payloads ready to be signed. */
export type SwapPayload = { kind: 'fynd'; payload: FyndPayload };

/** A 65-byte ECDSA signature encoded as a hex string. */
export type PrimitiveSignature = `0x${string}`;

/** A swap payload paired with its cryptographic signature, ready for on-chain submission. */
export interface SignedSwap {
  payload: SwapPayload;
  signature: PrimitiveSignature;
}

/** Result of a settled (or dry-run) swap execution. */
export interface SettledOrder {
  /** Transaction hash; absent for dry-run executions. */
  txHash?: Hex;
  /** Total output tokens received by the receiver, parsed from Transfer logs. */
  settledAmount?: bigint;
  /** Gas cost in wei (gasUsed * effectiveGasPrice, or estimated for dry-run). */
  gasCost: bigint;
}

/** Options for waiting on transaction settlement. */
export interface SettleOptions {
  /** Maximum time to wait for confirmation in milliseconds. Defaults to {@link DEFAULT_SETTLE_TIMEOUT_MS}. */
  timeoutMs?: number;
}

/** Default timeout for {@link ExecutionReceipt.settle} (120 seconds). */
export const DEFAULT_SETTLE_TIMEOUT_MS = 120_000;

/** Handle returned by {@link FyndClient.executeSwap} to await transaction settlement. */
export interface ExecutionReceipt {
  /** Polls for the transaction receipt and returns the settled result.
   * @throws {FyndError} With code `SETTLE_TIMEOUT` if the transaction does not confirm in time.
   */
  settle(options?: SettleOptions): Promise<SettledOrder>;
}

/**
 * Computes the EIP-1559 signing hash for a swap payload.
 *
 * Equivalent to keccak256 of the unsigned serialized EIP-1559 transaction,
 * which is the hash the signer must sign.
 */
export function swapSigningHash(payload: SwapPayload): Hex {
  const tx = payload.payload.tx;
  const serialized = serializeTransaction({
    type: 'eip1559',
    chainId: tx.chainId,
    nonce: tx.nonce,
    maxFeePerGas: tx.maxFeePerGas,
    maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
    gas: tx.gas,
    to: tx.to,
    value: tx.value,
    data: tx.data,
  });
  return keccak256(serialized);
}

/**
 * Wraps a swap payload and signature into a {@link SignedSwap} without any I/O.
 * Mirrors Rust's `SignedSwap::assemble`.
 */
export function assembleSignedSwap(
  payload: SwapPayload,
  signature: PrimitiveSignature,
): SignedSwap {
  return { payload, signature };
}

/**
 * Compute the EIP-1559 signing hash for an approval payload.
 * Sign this and pass the result to {@link FyndClient.executeApproval} via {@link SignedApproval}.
 */
export function approvalSigningHash(payload: ApprovalPayload): Hex {
  return keccak256(serializeTransaction({ type: 'eip1559', ...payload.tx })) as Hex;
}
