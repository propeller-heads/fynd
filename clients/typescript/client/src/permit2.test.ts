import { describe, it, expect } from 'vitest';
import {
  permit2SigningHash,
  encodingOptions,
  withPermit2,
  withNoTransfer,
} from './permit2.js';
import { FyndError } from './error.js';
import type { Address, Hex, PermitSingle } from './types.js';

const PERMIT2_ADDRESS = '0x000000000022D473030F116dDEE9F6B43aC78BA3' as Address;
const TOKEN = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2' as Address;
const SPENDER = '0x1111111111111111111111111111111111111111' as Address;

const basePermit: PermitSingle = {
  details: {
    token: TOKEN,
    amount: 1000000000000000000n,
    expiration: 1893456000n,
    nonce: 0n,
  },
  spender: SPENDER,
  sigDeadline: 1893456000n,
};

describe('permit2SigningHash', () => {
  it('returns a 0x-prefixed 66-char hex string', () => {
    const hash = permit2SigningHash(basePermit, 1, PERMIT2_ADDRESS);
    expect(hash).toMatch(/^0x[0-9a-f]{64}$/);
  });

  it('is deterministic for same inputs', () => {
    const hash1 = permit2SigningHash(basePermit, 1, PERMIT2_ADDRESS);
    const hash2 = permit2SigningHash(basePermit, 1, PERMIT2_ADDRESS);
    expect(hash1).toBe(hash2);
  });

  it('differs when chainId changes', () => {
    const hash1 = permit2SigningHash(basePermit, 1, PERMIT2_ADDRESS);
    const hash137 = permit2SigningHash(basePermit, 137, PERMIT2_ADDRESS);
    expect(hash1).not.toBe(hash137);
  });

  it('differs when nonce changes', () => {
    const hash0 = permit2SigningHash(basePermit, 1, PERMIT2_ADDRESS);
    const permit1: PermitSingle = {
      ...basePermit,
      details: { ...basePermit.details, nonce: 1n },
    };
    const hash1 = permit2SigningHash(permit1, 1, PERMIT2_ADDRESS);
    expect(hash0).not.toBe(hash1);
  });

  it('differs when spender changes', () => {
    const hash1 = permit2SigningHash(basePermit, 1, PERMIT2_ADDRESS);
    const altPermit: PermitSingle = {
      ...basePermit,
      spender: '0x2222222222222222222222222222222222222222' as Address,
    };
    const hash2 = permit2SigningHash(altPermit, 1, PERMIT2_ADDRESS);
    expect(hash1).not.toBe(hash2);
  });
});

describe('encodingOptions', () => {
  it('returns default transfer type transfer_from', () => {
    const opts = encodingOptions(0.005);
    expect(opts.slippage).toBe(0.005);
    expect(opts.transferType).toBe('transfer_from');
    expect(opts.permit).toBeUndefined();
    expect(opts.permit2Signature).toBeUndefined();
  });
});

describe('withPermit2', () => {
  it('sets transfer type and permit fields', () => {
    const base = encodingOptions(0.01);
    const sig = `0x${'ab'.repeat(65)}` as Hex;
    const result = withPermit2(base, basePermit, sig);
    expect(result.transferType).toBe('transfer_from_permit2');
    expect(result.permit).toBe(basePermit);
    expect(result.permit2Signature).toBe(sig);
    expect(result.slippage).toBe(0.01);
  });

  it('throws on wrong signature length (too short)', () => {
    const base = encodingOptions(0.01);
    const shortSig = '0xabcd' as Hex;
    expect(() => withPermit2(base, basePermit, shortSig)).toThrow(FyndError);
  });

  it('throws on wrong signature length (too long)', () => {
    const base = encodingOptions(0.01);
    const longSig = `0x${'ab'.repeat(66)}` as Hex;
    expect(() => withPermit2(base, basePermit, longSig)).toThrow(FyndError);
  });

  it('accepts exactly 65 bytes (132 hex chars)', () => {
    const base = encodingOptions(0.01);
    const exactSig = `0x${'00'.repeat(65)}` as Hex;
    const result = withPermit2(base, basePermit, exactSig);
    expect(result.permit2Signature).toBe(exactSig);
  });
});

describe('withNoTransfer', () => {
  it('sets transfer type to none', () => {
    const base = encodingOptions(0.01);
    const result = withNoTransfer(base);
    expect(result.transferType).toBe('none');
    expect(result.slippage).toBe(0.01);
  });
});
