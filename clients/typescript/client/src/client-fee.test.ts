import { describe, it, expect } from 'vitest';
import { clientFeeSigningHash, patchClientFeeSignature, withClientFee } from './client-fee.js';
import type { ClientFeeSwapContext } from './client-fee.js';
import { encodingOptions } from './permit2.js';
import { FyndError } from './error.js';
import type { Address, ClientFeeParams, Hex, Quote } from './types.js';

const ROUTER = '0x3333333333333333333333333333333333333333' as Address;
const FEE_RECEIVER = '0x4444444444444444444444444444444444444444' as Address;

function baseParams(): ClientFeeParams {
  return { bps: 100, receiver: FEE_RECEIVER, maxContribution: 0n, deadline: 1893456000 };
}

function baseSwap(): ClientFeeSwapContext {
  return {
    amountIn: 1000000000000000000n,
    tokenIn: '0x1111111111111111111111111111111111111111' as Address,
    tokenOut: '0x2222222222222222222222222222222222222222' as Address,
    expectedAmountOut: 1010000n,
    minAmountOut: 1000000n,
    receiver: '0x7777777777777777777777777777777777777777' as Address,
    swapsHash: `0x${'11'.repeat(32)}` as Hex,
  };
}

describe('clientFeeSigningHash', () => {
  it('returns a 0x-prefixed 66-char hex string', () => {
    const hash = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    expect(hash).toMatch(/^0x[0-9a-f]{64}$/);
  });

  it('is deterministic for same inputs', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    expect(hash1).toBe(hash2);
  });

  it('differs when chainId changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash137 = clientFeeSigningHash(baseParams(), 137, ROUTER, baseSwap());
    expect(hash1).not.toBe(hash137);
  });

  it('differs when bps changes', () => {
    const hash100 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash200 = clientFeeSigningHash({ ...baseParams(), bps: 200 }, 1, ROUTER, baseSwap());
    expect(hash100).not.toBe(hash200);
  });

  it('differs when receiver changes', () => {
    const other = '0x5555555555555555555555555555555555555555' as Address;
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash({ ...baseParams(), receiver: other }, 1, ROUTER, baseSwap());
    expect(hash1).not.toBe(hash2);
  });

  it('differs when maxContribution changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(
      { ...baseParams(), maxContribution: 1000n },
      1,
      ROUTER,
      baseSwap()
    );
    expect(hash1).not.toBe(hash2);
  });

  it('differs when deadline changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(
      { ...baseParams(), deadline: 9999999999 },
      1,
      ROUTER,
      baseSwap()
    );
    expect(hash1).not.toBe(hash2);
  });

  it('differs when router address changes', () => {
    const other = '0x6666666666666666666666666666666666666666' as Address;
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(baseParams(), 1, other, baseSwap());
    expect(hash1).not.toBe(hash2);
  });

  it('differs when expectedAmountOut changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(baseParams(), 1, ROUTER, {
      ...baseSwap(),
      expectedAmountOut: 1020000n,
    });
    expect(hash1).not.toBe(hash2);
  });

  it('differs when minAmountOut changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(baseParams(), 1, ROUTER, {
      ...baseSwap(),
      minAmountOut: 999000n,
    });
    expect(hash1).not.toBe(hash2);
  });

  it('differs when swapsHash changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER, baseSwap());
    const hash2 = clientFeeSigningHash(baseParams(), 1, ROUTER, {
      ...baseSwap(),
      swapsHash: `0x${'22'.repeat(32)}` as Hex,
    });
    expect(hash1).not.toBe(hash2);
  });
});

describe('withClientFee', () => {
  const validParams: ClientFeeParams = {
    ...baseParams(),
    signature: `0x${'ab'.repeat(65)}` as Hex,
  };

  it('attaches client fee params to encoding options', () => {
    const base = encodingOptions(0.01);
    const result = withClientFee(base, validParams);
    expect(result.clientFeeParams).toBe(validParams);
    expect(result.slippage).toBe(0.01);
  });

  it('preserves existing encoding options fields', () => {
    const base = encodingOptions(0.005);
    const result = withClientFee(base, validParams);
    expect(result.slippage).toBe(0.005);
    expect(result.transferType).toBe('transfer_from');
  });

  it('accepts params without a signature', () => {
    const base = encodingOptions(0.01);
    const result = withClientFee(base, baseParams());
    expect(result.clientFeeParams?.signature).toBeUndefined();
  });

  it('throws on wrong signature length (too short)', () => {
    const base = encodingOptions(0.01);
    const badParams = { ...validParams, signature: '0xabcd' as Hex };
    expect(() => withClientFee(base, badParams)).toThrow(FyndError);
  });

  it('throws on wrong signature length (too long)', () => {
    const base = encodingOptions(0.01);
    const badParams = { ...validParams, signature: `0x${'ab'.repeat(66)}` as Hex };
    expect(() => withClientFee(base, badParams)).toThrow(FyndError);
  });

  it('accepts exactly 65 bytes (132 hex chars)', () => {
    const base = encodingOptions(0.01);
    const exactSig = `0x${'00'.repeat(65)}` as Hex;
    const params = { ...validParams, signature: exactSig };
    const result = withClientFee(base, params);
    expect(result.clientFeeParams?.signature).toBe(exactSig);
  });
});

describe('patchClientFeeSignature', () => {
  const SIG = `0x${'ab'.repeat(65)}` as Hex;
  const PREFIX = 'dead';
  const SUFFIX = 'beef';
  /** Calldata with a zeroed 65-byte placeholder at byte offset 2. */
  const CALLDATA = `0x${PREFIX}${'00'.repeat(65)}${SUFFIX}` as Hex;

  function quoteWithOffset(offset: number | undefined, data: Hex = CALLDATA): Quote {
    return {
      orderId: 'f47ac10b-58cc-4372-a567-0e02b2c3d479',
      status: 'success',
      backend: 'fynd',
      amountIn: 1000n,
      amountOut: 2000n,
      gasEstimate: 150000n,
      block: { number: 21000000, hash: '0xabcdef', timestamp: 1730000000 },
      tokenOut: '0x2222222222222222222222222222222222222222' as Address,
      receiver: '0x7777777777777777777777777777777777777777' as Address,
      transaction: {
        to: ROUTER,
        value: 0n,
        data,
        ...(offset !== undefined ? { clientFeeSignatureOffset: offset } : {}),
      },
    };
  }

  function quoteWithoutTransaction(): Quote {
    const { transaction: _tx, ...rest } = quoteWithOffset(2);
    return rest;
  }

  it('replaces the placeholder at the reported offset', () => {
    const patched = patchClientFeeSignature(quoteWithOffset(2), SIG);
    expect(patched.transaction?.data).toBe(`0x${PREFIX}${SIG.slice(2)}${SUFFIX}`);
  });

  it('leaves the input quote untouched', () => {
    const quote = quoteWithOffset(2);
    patchClientFeeSignature(quote, SIG);
    expect(quote.transaction?.data).toBe(CALLDATA);
  });

  it('preserves the other transaction fields', () => {
    const patched = patchClientFeeSignature(quoteWithOffset(2), SIG);
    expect(patched.transaction?.to).toBe(ROUTER);
    expect(patched.transaction?.clientFeeSignatureOffset).toBe(2);
    expect(patched.amountOut).toBe(2000n);
  });

  it('patches at offset 0', () => {
    const patched = patchClientFeeSignature(quoteWithOffset(0), SIG);
    expect(patched.transaction?.data).toBe(`0x${SIG.slice(2)}0000${SUFFIX}`);
  });

  it('patches at the last offset that fits', () => {
    // Calldata holds 69 bytes, so a 65-byte signature starting at byte 4 ends exactly at the end.
    const patched = patchClientFeeSignature(quoteWithOffset(4), SIG);
    expect(patched.transaction?.data).toBe(`0x${PREFIX}0000${SIG.slice(2)}`);
  });

  const rejected: [string, Quote, Hex][] = [
    ['the offset is absent', quoteWithOffset(undefined), SIG],
    ['the offset is negative', quoteWithOffset(-1), SIG],
    ['the signature does not fit the calldata', quoteWithOffset(5), SIG],
    ['the quote has no transaction', quoteWithoutTransaction(), SIG],
    ['the signature is the wrong length', quoteWithOffset(2), '0xabcd' as Hex],
    ['the signature is not hex', quoteWithOffset(2), `0x${'zz'.repeat(65)}` as Hex],
    ['the calldata is not hex', quoteWithOffset(2, `0x${'zz'.repeat(69)}` as Hex), SIG],
    ['the calldata has an odd digit count', quoteWithOffset(2, `0x${'00'.repeat(69)}0` as Hex), SIG],
  ];

  it.each(rejected)('throws when %s', (_case, quote, signature) => {
    expect(() => patchClientFeeSignature(quote, signature)).toThrow(FyndError);
  });
});
