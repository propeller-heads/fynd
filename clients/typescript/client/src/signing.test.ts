import { describe, it, expect } from 'vitest';
import { approvalSigningHash, assembleSignedOrder, signingHash } from './signing.js';
import type { ApprovalPayload, SignablePayload, PrimitiveSignature } from './signing.js';
import type { Quote } from './types.js';

const TOKEN_OUT = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48' as const;
const SENDER    = '0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045' as const;
const ROUTER    = '0x1111111111111111111111111111111111111111' as const;

const baseQuote: Quote = {
  orderId:     'test-order-id',
  status:      'success',
  backend:     'fynd',
  amountIn:    1000000000000000000n,
  amountOut:   3500000000n,
  gasEstimate: 150000n,
  block: {
    hash:      '0xabcdef',
    number:    21000000,
    timestamp: 1730000000,
  },
  tokenOut: TOKEN_OUT,
  receiver: SENDER,
  transaction: {
    to: ROUTER,
    value: 0n,
    data: '0x',
  },
};

const basePayload: SignablePayload = {
  kind: 'fynd',
  payload: {
    quote: baseQuote,
    tx: {
      chainId:              1,
      nonce:                42,
      maxFeePerGas:         20000000000n,
      maxPriorityFeePerGas: 2000000000n,
      gas:                  150000n,
      to:                   ROUTER,
      value:                0n,
      data:                 '0x',
    },
  },
};

describe('assembleSignedOrder', () => {
  it('wraps payload and signature without modification', () => {
    const sig = ('0x' + 'a'.repeat(64) + 'b'.repeat(64) + '00') as PrimitiveSignature;
    const order = assembleSignedOrder(basePayload, sig);
    expect(order.payload).toBe(basePayload);
    expect(order.signature).toBe(sig);
  });

  it('is a pure function — no I/O', () => {
    const sig: PrimitiveSignature = `0x${'ff'.repeat(65)}`;
    const order = assembleSignedOrder(basePayload, sig);
    expect(order).toEqual({ payload: basePayload, signature: sig });
  });
});

describe('signingHash', () => {
  it('returns a 0x-prefixed hex string', () => {
    const hash = signingHash(basePayload);
    expect(hash).toMatch(/^0x[0-9a-f]{64}$/);
  });

  it('is deterministic for the same payload', () => {
    const hash1 = signingHash(basePayload);
    const hash2 = signingHash(basePayload);
    expect(hash1).toBe(hash2);
  });

  it('differs when nonce changes', () => {
    const payload1 = basePayload;
    const payload2: SignablePayload = {
      ...basePayload,
      payload: { ...basePayload.payload, tx: { ...basePayload.payload.tx, nonce: 43 } },
    };
    expect(signingHash(payload1)).not.toBe(signingHash(payload2));
  });

  it('differs when chainId changes', () => {
    const payload1 = basePayload;
    const payload2: SignablePayload = {
      ...basePayload,
      payload: { ...basePayload.payload, tx: { ...basePayload.payload.tx, chainId: 137 } },
    };
    expect(signingHash(payload1)).not.toBe(signingHash(payload2));
  });

  it('returns known hash for fixture transaction', async () => {
    // Cross-checked: keccak256(serializeTransaction({ type: 'eip1559', chainId: 1, nonce: 42,
    //   maxFeePerGas: 20000000000n, maxPriorityFeePerGas: 2000000000n, gas: 150000n,
    //   to: '0x1111...', value: 0n, data: '0x' }))
    const hash = signingHash(basePayload);
    // Verify format only — exact hash is viem-internal
    expect(hash.startsWith('0x')).toBe(true);
    expect(hash.length).toBe(66);

    // Verify a known hash by computing it independently
    const { keccak256, serializeTransaction } = await import('viem');
    const tx = basePayload.payload.tx;
    const expected = keccak256(
      serializeTransaction({
        type: 'eip1559',
        chainId: tx.chainId,
        nonce: tx.nonce,
        maxFeePerGas: tx.maxFeePerGas,
        maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
        gas: tx.gas,
        to: tx.to,
        value: tx.value,
        data: tx.data,
      }),
    );
    expect(hash).toBe(expected);
  });

  it('includes all transaction fields in hash', () => {
    // Changing value should change hash
    const withValue: SignablePayload = {
      ...basePayload,
      payload: {
        ...basePayload.payload,
        tx: { ...basePayload.payload.tx, value: 1n },
      },
    };
    expect(signingHash(basePayload)).not.toBe(signingHash(withValue));
  });
});

const TOKEN   = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2' as const;
const SPENDER = '0x1111111111111111111111111111111111111111' as const;

const baseApprovalPayload: ApprovalPayload = {
  tx: {
    chainId:              1,
    nonce:                5,
    maxFeePerGas:         20000000000n,
    maxPriorityFeePerGas: 2000000000n,
    gas:                  65_000n,
    to:                   TOKEN,
    value:                0n,
    data:                 '0x095ea7b3000000000000000000000000111111111111111111111111111111111111111100000000000000000000000000000000000000000000000000000000000003e8',
  },
  token:   TOKEN,
  spender: SPENDER,
  amount:  1000n,
};

describe('approvalSigningHash', () => {
  it('returns a 0x-prefixed 32-byte hex string', () => {
    const hash = approvalSigningHash(baseApprovalPayload);
    expect(hash).toMatch(/^0x[0-9a-f]{64}$/);
  });

  it('is deterministic for the same payload', () => {
    const hash1 = approvalSigningHash(baseApprovalPayload);
    const hash2 = approvalSigningHash(baseApprovalPayload);
    expect(hash1).toBe(hash2);
  });

  it('differs when nonce changes', () => {
    const other: ApprovalPayload = {
      ...baseApprovalPayload,
      tx: { ...baseApprovalPayload.tx, nonce: 99 },
    };
    expect(approvalSigningHash(baseApprovalPayload)).not.toBe(approvalSigningHash(other));
  });

  it('differs when amount changes', () => {
    const other: ApprovalPayload = {
      ...baseApprovalPayload,
      tx: { ...baseApprovalPayload.tx, data: '0xdeadbeef' as `0x${string}` },
      amount: 9999n,
    };
    expect(approvalSigningHash(baseApprovalPayload)).not.toBe(approvalSigningHash(other));
  });
});
