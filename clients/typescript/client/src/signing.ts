import { keccak256, serializeTransaction } from 'viem';
import type { Address, Hex, Quote } from './types.js';

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

export interface FyndPayload {
  quote: Quote;   // carries tokenOut and receiver for settlement
  tx: Eip1559Transaction;
}

export type SignablePayload = { kind: 'fynd'; payload: FyndPayload };
export type PrimitiveSignature = `0x${string}`;

export interface SignedOrder {
  payload: SignablePayload;
  signature: PrimitiveSignature;
}

export interface SettledOrder {
  txHash?: Hex;          // absent for dry-run
  settledAmount?: bigint;
  gasCost: bigint;
}

export interface ExecutionReceipt {
  settle(): Promise<SettledOrder>;
}

/**
 * Computes the EIP-1559 signing hash for a signable payload.
 *
 * Equivalent to keccak256 of the unsigned serialized EIP-1559 transaction,
 * which is the hash the signer must sign.
 */
export function signingHash(payload: SignablePayload): Hex {
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
 * Wraps a payload and signature into a SignedOrder without any I/O.
 * Mirrors Rust's SignedOrder::assemble.
 */
export function assembleSignedOrder(
  payload: SignablePayload,
  signature: PrimitiveSignature,
): SignedOrder {
  return { payload, signature };
}
