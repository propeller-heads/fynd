import { describe, it, expect } from 'vitest';
import { FyndError } from './error.js';
import type { ErrorCode } from './error.js';

describe('FyndError.isRetryable', () => {
  const retryableCodes: ErrorCode[] = [
    'TIMEOUT',
    'QUEUE_FULL',
    'SERVICE_OVERLOADED',
    'STALE_DATA',
    'NOT_READY',
    'HTTP',
  ];

  const nonRetryableCodes: ErrorCode[] = [
    'BAD_REQUEST',
    'NO_ROUTE_FOUND',
    'INSUFFICIENT_LIQUIDITY',
    'INVALID_ORDER',
    'INTERNAL_ERROR',
    'ALGORITHM_ERROR',
    'DESERIALIZE',
    'CONFIG',
    'SIMULATE_FAILED',
  ];

  for (const code of retryableCodes) {
    it(`returns true for ${String(code)}`, () => {
      const err = new FyndError('test', code);
      expect(err.isRetryable()).toBe(true);
    });
  }

  for (const code of nonRetryableCodes) {
    it(`returns false for ${String(code)}`, () => {
      const err = new FyndError('test', code);
      expect(err.isRetryable()).toBe(false);
    });
  }

  it('returns false for UNKNOWN code object', () => {
    const err = new FyndError('test', { kind: 'UNKNOWN', raw: 'WHATEVER' });
    expect(err.isRetryable()).toBe(false);
  });
});

describe('FyndError.fromWireError', () => {
  it('maps BAD_REQUEST correctly', () => {
    const err = FyndError.fromWireError({ code: 'BAD_REQUEST', error: 'bad input' });
    expect(err.code).toBe('BAD_REQUEST');
    expect(err.isRetryable()).toBe(false);
    expect(err.message).toBe('bad input');
  });

  it('maps NO_ROUTE_FOUND correctly', () => {
    const err = FyndError.fromWireError({ code: 'NO_ROUTE_FOUND', error: 'no route' });
    expect(err.code).toBe('NO_ROUTE_FOUND');
    expect(err.isRetryable()).toBe(false);
  });

  it('maps INSUFFICIENT_LIQUIDITY correctly', () => {
    const err = FyndError.fromWireError({ code: 'INSUFFICIENT_LIQUIDITY', error: 'low liquidity' });
    expect(err.code).toBe('INSUFFICIENT_LIQUIDITY');
    expect(err.isRetryable()).toBe(false);
  });

  it('maps TIMEOUT as retryable', () => {
    const err = FyndError.fromWireError({ code: 'TIMEOUT', error: 'timed out' });
    expect(err.code).toBe('TIMEOUT');
    expect(err.isRetryable()).toBe(true);
  });

  it('maps QUEUE_FULL as retryable', () => {
    const err = FyndError.fromWireError({ code: 'QUEUE_FULL', error: 'queue full' });
    expect(err.code).toBe('QUEUE_FULL');
    expect(err.isRetryable()).toBe(true);
  });

  it('maps SERVICE_OVERLOADED as retryable', () => {
    const err = FyndError.fromWireError({ code: 'SERVICE_OVERLOADED', error: 'overloaded' });
    expect(err.code).toBe('SERVICE_OVERLOADED');
    expect(err.isRetryable()).toBe(true);
  });

  it('maps STALE_DATA as retryable', () => {
    const err = FyndError.fromWireError({ code: 'STALE_DATA', error: 'stale' });
    expect(err.code).toBe('STALE_DATA');
    expect(err.isRetryable()).toBe(true);
  });

  it('maps INVALID_ORDER correctly', () => {
    const err = FyndError.fromWireError({ code: 'INVALID_ORDER', error: 'invalid' });
    expect(err.code).toBe('INVALID_ORDER');
    expect(err.isRetryable()).toBe(false);
  });

  it('maps INTERNAL_ERROR correctly', () => {
    const err = FyndError.fromWireError({ code: 'INTERNAL_ERROR', error: 'internal' });
    expect(err.code).toBe('INTERNAL_ERROR');
    expect(err.isRetryable()).toBe(false);
  });

  it('maps NOT_READY as retryable', () => {
    const err = FyndError.fromWireError({ code: 'NOT_READY', error: 'not ready' });
    expect(err.code).toBe('NOT_READY');
    expect(err.isRetryable()).toBe(true);
  });

  it('maps ALGORITHM_ERROR correctly', () => {
    const err = FyndError.fromWireError({ code: 'ALGORITHM_ERROR', error: 'algo error' });
    expect(err.code).toBe('ALGORITHM_ERROR');
    expect(err.isRetryable()).toBe(false);
  });

  it('maps unknown code to UNKNOWN object', () => {
    const err = FyndError.fromWireError({ code: 'SOME_NEW_CODE', error: 'unknown' });
    expect(err.code).toEqual({ kind: 'UNKNOWN', raw: 'SOME_NEW_CODE' });
    expect(err.isRetryable()).toBe(false);
  });

  it('propagates details', () => {
    const details = { field: 'amount', reason: 'negative' };
    const err = FyndError.fromWireError({ code: 'BAD_REQUEST', error: 'bad', details });
    expect(err.details).toEqual(details);
  });
});

describe('FyndError.config', () => {
  it('creates a CONFIG error that is not retryable', () => {
    const err = FyndError.config('provider not set');
    expect(err.code).toBe('CONFIG');
    expect(err.isRetryable()).toBe(false);
    expect(err.message).toBe('provider not set');
    expect(err.name).toBe('FyndError');
  });
});

describe('FyndError.simulateFailed', () => {
  it('creates a SIMULATE_FAILED error that is not retryable', () => {
    const err = FyndError.simulateFailed('revert: ERC20: insufficient balance');
    expect(err.code).toBe('SIMULATE_FAILED');
    expect(err.isRetryable()).toBe(false);
    expect(err.message).toBe('revert: ERC20: insufficient balance');
  });
});

describe('FyndError basic properties', () => {
  it('is an instance of Error', () => {
    const err = new FyndError('msg', 'CONFIG');
    expect(err).toBeInstanceOf(Error);
    expect(err).toBeInstanceOf(FyndError);
  });

  it('has name FyndError', () => {
    const err = new FyndError('msg', 'HTTP');
    expect(err.name).toBe('FyndError');
  });

  it('has no details when not provided', () => {
    const err = new FyndError('msg', 'CONFIG');
    expect(err.details).toBeUndefined();
  });
});
