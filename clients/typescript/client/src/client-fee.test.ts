import { describe, it, expect } from 'vitest';
import { clientFeeSigningHash, withClientFee } from './client-fee.js';
import { encodingOptions } from './permit2.js';
import { FyndError } from './error.js';
import type { Address, ClientFeeParams, Hex } from './types.js';

const ROUTER = '0x3333333333333333333333333333333333333333' as Address;
const FEE_RECEIVER = '0x4444444444444444444444444444444444444444' as Address;

function baseParams(): ClientFeeParams {
  return { bps: 100, receiver: FEE_RECEIVER, maxContribution: 0n, deadline: 1893456000 };
}

describe('clientFeeSigningHash', () => {
  it('returns a 0x-prefixed 66-char hex string', () => {
    const hash = clientFeeSigningHash(baseParams(), 1, ROUTER);
    expect(hash).toMatch(/^0x[0-9a-f]{64}$/);
  });

  it('is deterministic for same inputs', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash2 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    expect(hash1).toBe(hash2);
  });

  it('differs when chainId changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash137 = clientFeeSigningHash(baseParams(), 137, ROUTER);
    expect(hash1).not.toBe(hash137);
  });

  it('differs when bps changes', () => {
    const hash100 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash200 = clientFeeSigningHash({ ...baseParams(), bps: 200 }, 1, ROUTER);
    expect(hash100).not.toBe(hash200);
  });

  it('differs when receiver changes', () => {
    const other = '0x5555555555555555555555555555555555555555' as Address;
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash2 = clientFeeSigningHash({ ...baseParams(), receiver: other }, 1, ROUTER);
    expect(hash1).not.toBe(hash2);
  });

  it('differs when maxContribution changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash2 = clientFeeSigningHash({ ...baseParams(), maxContribution: 1000n }, 1, ROUTER);
    expect(hash1).not.toBe(hash2);
  });

  it('differs when deadline changes', () => {
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash2 = clientFeeSigningHash({ ...baseParams(), deadline: 9999999999 }, 1, ROUTER);
    expect(hash1).not.toBe(hash2);
  });

  it('differs when router address changes', () => {
    const other = '0x6666666666666666666666666666666666666666' as Address;
    const hash1 = clientFeeSigningHash(baseParams(), 1, ROUTER);
    const hash2 = clientFeeSigningHash(baseParams(), 1, other);
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

  it('throws when signature is missing', () => {
    const base = encodingOptions(0.01);
    expect(() => withClientFee(base, baseParams())).toThrow(FyndError);
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
