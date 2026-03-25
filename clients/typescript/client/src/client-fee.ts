import { hashTypedData } from 'viem';
import { FyndError } from './error.js';
import type { Address, ClientFeeParams, EncodingOptions, Hex } from './types.js';

const CLIENT_FEE_TYPE = {
  ClientFee: [
    { name: 'clientFeeBps', type: 'uint16' },
    { name: 'clientFeeReceiver', type: 'address' },
    { name: 'maxClientContribution', type: 'uint256' },
    { name: 'deadline', type: 'uint256' },
  ],
} as const;

/**
 * Computes the EIP-712 signing hash for client fee params.
 *
 * Pass the returned hash to the fee receiver's signer, then supply the
 * 65-byte signature when constructing `ClientFeeParams`.
 *
 * `routerAddress` is the TychoRouter contract address.
 */
export function clientFeeSigningHash(
  bps: number,
  receiver: Address,
  maxContribution: bigint,
  deadline: bigint,
  chainId: number,
  routerAddress: Address,
): Hex {
  return hashTypedData({
    domain: {
      name: 'TychoRouter',
      version: '1',
      chainId,
      verifyingContract: routerAddress,
    },
    types: CLIENT_FEE_TYPE,
    primaryType: 'ClientFee',
    message: {
      clientFeeBps: bps,
      clientFeeReceiver: receiver,
      maxClientContribution: maxContribution,
      deadline,
    },
  });
}

/**
 * Attach client fee configuration to encoding options.
 * Validates that signature is exactly 65 bytes (130 hex chars + '0x' prefix).
 */
export function withClientFee(
  opts: EncodingOptions,
  params: ClientFeeParams,
): EncodingOptions {
  if (params.signature.length !== 132) {
    throw FyndError.config(
      `Client fee signature must be exactly 65 bytes (132 hex chars), got ${String(params.signature.length)} chars`
    );
  }
  return { ...opts, clientFeeParams: params };
}
