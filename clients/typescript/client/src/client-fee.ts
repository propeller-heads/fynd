import {
  bytesToHex,
  concatHex,
  encodeAbiParameters,
  hexToBytes,
  isHex,
  keccak256,
  stringToHex,
} from 'viem';
import { FyndError } from './error.js';
import { assertSignatureLength, SIGNATURE_BYTES } from './signing.js';
import type { Address, ClientFeeParams, EncodingOptions, Hex, Quote } from './types.js';

/**
 * Fee units per basis point in the router's `ClientFeeParams.clientFeeBps`.
 *
 * Fynd's API takes the client fee in basis points, while the router takes it in the
 * FeeCalculator's fee units (`MAX_BPS` = 100,000,000 = 100%). The signature must cover the
 * scaled value that ends up in the calldata.
 */
const CLIENT_FEE_UNITS_PER_BPS = 10_000n;

/** Must match `CLIENT_FEE_TYPEHASH` in `TychoRouter.sol`. */
const CLIENT_FEE_TYPEHASH = keccak256(
  stringToHex(
    'ClientFee(uint32 clientFeeBps,address clientFeeReceiver,' +
      'uint256 maxClientContribution,uint256 deadline,' +
      'uint256 amountIn,address tokenIn,address tokenOut,' +
      'uint256 expectedAmountOut,uint256 minAmountOut,address receiver,bytes swaps)'
  )
);

const EIP712_DOMAIN_TYPEHASH = keccak256(
  stringToHex(
    'EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)'
  )
);

/** Swap-specific inputs the router binds the client fee signature to. */
export interface ClientFeeSwapContext {
  /** Exact input amount from the order. */
  amountIn: bigint;
  /** Input token address. */
  tokenIn: Address;
  /** Output token address. */
  tokenOut: Address;
  /** Quoted output amount — the `amountOut` of the unsigned quote. */
  expectedAmountOut: bigint;
  /** Minimum output after fees — `feeBreakdown.minAmountReceived`. */
  minAmountOut: bigint;
  /** Address receiving the swap output. */
  receiver: Address;
  /** keccak256 of the encoded swaps bytes — `feeBreakdown.swapsHash`. */
  swapsHash: Hex;
}

/**
 * Computes the EIP-712 signing hash for client fee params.
 *
 * Pass the returned hash to the fee receiver's signer, then patch the 65-byte signature into
 * the quote's calldata with `patchClientFeeSignature`.
 *
 * The hash covers all 11 `ClientFee` fields: the fee params plus the swap they are bound to,
 * which come from the quote requested with unsigned params.
 *
 * `routerAddress` is the TychoRouter contract address.
 */
export function clientFeeSigningHash(
  params: ClientFeeParams,
  chainId: number,
  routerAddress: Address,
  swap: ClientFeeSwapContext
): Hex {
  const domainSeparator = keccak256(
    encodeAbiParameters(
      [
        { type: 'bytes32' },
        { type: 'bytes32' },
        { type: 'bytes32' },
        { type: 'uint256' },
        { type: 'address' },
      ],
      [
        EIP712_DOMAIN_TYPEHASH,
        keccak256(stringToHex('TychoRouter')),
        keccak256(stringToHex('1')),
        BigInt(chainId),
        routerAddress,
      ]
    )
  );

  const structHash = keccak256(
    encodeAbiParameters(
      [
        { type: 'bytes32' },
        { type: 'uint256' },
        { type: 'address' },
        { type: 'uint256' },
        { type: 'uint256' },
        { type: 'uint256' },
        { type: 'address' },
        { type: 'address' },
        { type: 'uint256' },
        { type: 'uint256' },
        { type: 'address' },
        { type: 'bytes32' },
      ],
      [
        CLIENT_FEE_TYPEHASH,
        BigInt(params.bps) * CLIENT_FEE_UNITS_PER_BPS,
        params.receiver,
        params.maxContribution,
        BigInt(params.deadline),
        swap.amountIn,
        swap.tokenIn,
        swap.tokenOut,
        swap.expectedAmountOut,
        swap.minAmountOut,
        swap.receiver,
        swap.swapsHash,
      ]
    )
  );

  return keccak256(concatHex(['0x1901', domainSeparator, structHash]));
}

/**
 * Attach client fee configuration to encoding options.
 *
 * `params.signature` is optional: the signature covers the quoted swap, so the usual flow is
 * to quote with unsigned params and patch the signature into the returned calldata with
 * `patchClientFeeSignature`. When a signature is set it must be exactly 65 bytes
 * (130 hex chars + '0x' prefix).
 */
export function withClientFee(
  opts: EncodingOptions,
  params: ClientFeeParams,
): EncodingOptions {
  if (params.signature !== undefined) {
    assertSignatureLength(params.signature, 'Client fee');
  }
  return { ...opts, clientFeeParams: params };
}

/**
 * Overwrite the client fee signature placeholder in a quote's calldata.
 *
 * The server encodes the transaction with a zeroed 65-byte placeholder and reports its byte
 * offset as `transaction.clientFeeSignatureOffset`, so one quote request is enough: sign the
 * hash from `clientFeeSigningHash`, patch it in here, and submit the transaction. The Rust
 * client calls this same step `Quote::with_client_fee_signature`.
 *
 * Returns a new quote; the input is left untouched. Throws when the quote carries no encoded
 * transaction (set `encodingOptions` on the request), when it carries no signature offset
 * (set `clientFeeParams` too), when `signature` is not 65 bytes, when either the signature or
 * the calldata is not valid hex, or when the offset does not fit the calldata.
 */
export function patchClientFeeSignature(quote: Quote, signature: Hex): Quote {
  const tx = quote.transaction;
  if (tx === undefined) {
    throw FyndError.config(
      'Quote has no transaction to patch — set encodingOptions on the quote request'
    );
  }
  const offset = tx.clientFeeSignatureOffset;
  if (offset === undefined) {
    throw FyndError.config(
      'Quote has no clientFeeSignatureOffset — set clientFeeParams on the quote request'
    );
  }
  assertSignatureLength(signature, 'Client fee');
  const calldata = toBytes(tx.data, 'Quote calldata');
  if (offset < 0 || offset + SIGNATURE_BYTES > calldata.length) {
    throw FyndError.config(
      `Client fee signature at offset ${String(offset)} does not fit ${String(calldata.length)}-byte calldata`
    );
  }
  calldata.set(toBytes(signature, 'Client fee signature'), offset);
  return { ...quote, transaction: { ...tx, data: bytesToHex(calldata) } };
}

/**
 * Converts a hex string to bytes, rejecting anything `hexToBytes` would mangle.
 *
 * viem throws its own error type on invalid characters and silently left-pads an odd number
 * of digits, which would shift every byte of the calldata by a nibble.
 */
function toBytes(value: Hex, label: string): Uint8Array {
  if (!isHex(value, { strict: true }) || value.length % 2 !== 0) {
    throw FyndError.config(`${label} is not valid hex: ${value.slice(0, 12)}`);
  }
  return hexToBytes(value);
}
