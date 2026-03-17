import { hashTypedData } from 'viem';
import { FyndError } from './error.js';
import type { Address, EncodingOptions, Hex, PermitSingle } from './types.js';

const PERMIT_DETAILS_TYPE = {
  PermitDetails: [
    { name: 'token', type: 'address' },
    { name: 'amount', type: 'uint160' },
    { name: 'expiration', type: 'uint48' },
    { name: 'nonce', type: 'uint48' },
  ],
} as const;

const PERMIT_SINGLE_TYPE = {
  PermitSingle: [
    { name: 'details', type: 'PermitDetails' },
    { name: 'spender', type: 'address' },
    { name: 'sigDeadline', type: 'uint256' },
  ],
  ...PERMIT_DETAILS_TYPE,
} as const;

/**
 * Computes the EIP-712 signing hash for a Permit2 PermitSingle.
 *
 * Pass the returned hash to your signer's signMessage/signHash method,
 * then supply the 65-byte signature to EncodingOptions.permit2Signature.
 *
 * The canonical Permit2 address across all chains is:
 * 0x000000000022D473030F116dDEE9F6B43aC78BA3
 */
export function permit2SigningHash(
  permit: PermitSingle,
  chainId: number,
  permit2Address: Address,
): Hex {
  return hashTypedData({
    domain: {
      name: 'Permit2',
      chainId,
      verifyingContract: permit2Address,
    },
    types: PERMIT_SINGLE_TYPE,
    primaryType: 'PermitSingle',
    message: {
      details: {
        token: permit.details.token,
        amount: permit.details.amount,
        expiration: Number(permit.details.expiration),
        nonce: Number(permit.details.nonce),
      },
      spender: permit.spender,
      sigDeadline: permit.sigDeadline,
    },
  });
}

/**
 * Create encoding options with slippage tolerance.
 * Default transfer type is 'transfer_from'.
 */
export function encodingOptions(slippage: number): EncodingOptions {
  return { slippage, transferType: 'transfer_from' };
}

/**
 * Add Permit2 authorization to encoding options.
 * Validates that signature is exactly 65 bytes (130 hex chars + '0x' prefix).
 */
export function withPermit2(
  opts: EncodingOptions,
  permit: PermitSingle,
  signature: Hex,
): EncodingOptions {
  if (signature.length !== 132) {
    throw FyndError.config(
      `Permit2 signature must be exactly 65 bytes (132 hex chars), got ${String(signature.length)} chars`
    );
  }
  return {
    ...opts,
    transferType: 'transfer_from_permit2',
    permit,
    permit2Signature: signature,
  };
}

/**
 * Set transfer type to 'none' (funds already in router).
 */
export function withNoTransfer(opts: EncodingOptions): EncodingOptions {
  return { ...opts, transferType: 'none' };
}
