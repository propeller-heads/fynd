import { concatHex, encodeAbiParameters, keccak256, stringToHex } from 'viem';
import { FyndError } from './error.js';
import type { Address, ClientFeeParams, EncodingOptions, Hex } from './types.js';

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
 * Pass the returned hash to the fee receiver's signer, then set the
 * 65-byte signature on the `ClientFeeParams` before passing to `withClientFee`.
 *
 * The hash covers all 11 `ClientFee` fields: the fee params plus the swap they are bound to,
 * which come from a prior unsigned quote request.
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
 * Validates that signature is present and exactly 65 bytes (130 hex chars + '0x' prefix).
 */
export function withClientFee(
  opts: EncodingOptions,
  params: ClientFeeParams,
): EncodingOptions {
  if (params.signature === undefined) {
    throw FyndError.config('Client fee signature is required');
  }
  if (params.signature.length !== 132) {
    throw FyndError.config(
      `Client fee signature must be exactly 65 bytes (132 hex chars), got ${String(params.signature.length)} chars`
    );
  }
  return { ...opts, clientFeeParams: params };
}
