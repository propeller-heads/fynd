import { describe, it, expect } from 'vitest';
import { toWireRequest, fromWireQuote, fromWireHealth } from './mapping.js';
import { FyndError } from './error.js';
import type { QuoteParams } from './types.js';
import type { components } from '@fynd/autogen';

type WireSolution = components["schemas"]["Solution"];

const SENDER = '0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045' as const;
const TOKEN_IN = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2' as const;
const TOKEN_OUT = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48' as const;
const RECEIVER = '0x1234512345123451234512345123451234512345' as const;

const baseParams: QuoteParams = {
  order: {
    tokenIn:  TOKEN_IN,
    tokenOut: TOKEN_OUT,
    amount:   1000n,
    side:     'sell',
    sender:   SENDER,
  },
};

const baseWireSolution: WireSolution = {
  orders: [
    {
      order_id:           'f47ac10b-58cc-4372-a567-0e02b2c3d479',
      status:             'success',
      amount_in:          '1000000000000000000',
      amount_out:         '3500000000',
      amount_out_net_gas: '3498000000',
      gas_estimate:       '150000',
      block: {
        hash:      '0xabcdef1234',
        number:    21000000,
        timestamp: 1730000000,
      },
    },
  ],
  total_gas_estimate: '150000',
  solve_time_ms:      12,
};

describe('toWireRequest', () => {
  it('converts a basic sell order', () => {
    const wire = toWireRequest(baseParams);
    expect(wire.orders).toHaveLength(1);
    const order = wire.orders[0];
    expect(order?.token_in).toBe(TOKEN_IN);
    expect(order?.token_out).toBe(TOKEN_OUT);
    expect(order?.amount).toBe('1000');
    expect(order?.side).toBe('sell');
    expect(order?.sender).toBe(SENDER);
  });

  it('omits receiver key when order.receiver is undefined', () => {
    const wire = toWireRequest(baseParams);
    expect(wire.orders[0]).not.toHaveProperty('receiver');
  });

  it('includes receiver key when order.receiver is set', () => {
    const params: QuoteParams = {
      order: { ...baseParams.order, receiver: RECEIVER },
    };
    const wire = toWireRequest(params);
    expect(wire.orders[0]?.receiver).toBe(RECEIVER);
  });

  it('omits options when not provided', () => {
    const wire = toWireRequest(baseParams);
    expect(wire).not.toHaveProperty('options');
  });

  it('converts maxGas bigint to string', () => {
    const params: QuoteParams = {
      ...baseParams,
      options: { maxGas: 500000n },
    };
    const wire = toWireRequest(params);
    expect(wire.options?.max_gas).toBe('500000');
  });

  it('converts all options when fully specified', () => {
    const params: QuoteParams = {
      ...baseParams,
      options: {
        timeoutMs:    2000,
        minResponses: 2,
        maxGas:       500000n,
      },
    };
    const wire = toWireRequest(params);
    expect(wire.options?.timeout_ms).toBe(2000);
    expect(wire.options?.min_responses).toBe(2);
    expect(wire.options?.max_gas).toBe('500000');
  });

  it('omits individual option fields when not provided', () => {
    const params: QuoteParams = {
      ...baseParams,
      options: { timeoutMs: 1000 },
    };
    const wire = toWireRequest(params);
    expect(wire.options).toBeDefined();
    expect(wire.options).not.toHaveProperty('min_responses');
    expect(wire.options).not.toHaveProperty('max_gas');
  });
});

describe('fromWireQuote', () => {
  it('maps all fields correctly on happy path', () => {
    const quote = fromWireQuote(baseWireSolution, TOKEN_OUT, SENDER);
    expect(quote.orderId).toBe('f47ac10b-58cc-4372-a567-0e02b2c3d479');
    expect(quote.status).toBe('success');
    expect(quote.backend).toBe('fynd');
    expect(quote.amountIn).toBe(1000000000000000000n);
    expect(quote.amountOut).toBe(3500000000n);
    expect(quote.gasEstimate).toBe(150000n);
    expect(quote.tokenOut).toBe(TOKEN_OUT);
    expect(quote.receiver).toBe(SENDER);
  });

  it('converts amount strings to bigint', () => {
    const quote = fromWireQuote(baseWireSolution, TOKEN_OUT, SENDER);
    expect(typeof quote.amountIn).toBe('bigint');
    expect(typeof quote.amountOut).toBe('bigint');
    expect(typeof quote.gasEstimate).toBe('bigint');
    expect(quote.amountIn).toBe(1000000000000000000n);
  });

  it('maps block info correctly', () => {
    const quote = fromWireQuote(baseWireSolution, TOKEN_OUT, SENDER);
    expect(quote.block.hash).toBe('0xabcdef1234');
    expect(quote.block.number).toBe(21000000);
    expect(quote.block.timestamp).toBe(1730000000);
  });

  it('throws FyndError.CONFIG when orders array is empty', () => {
    const wire: WireSolution = { ...baseWireSolution, orders: [] };
    expect(() => fromWireQuote(wire, TOKEN_OUT, SENDER)).toThrow(FyndError);
    try {
      fromWireQuote(wire, TOKEN_OUT, SENDER);
    } catch (e) {
      expect(e instanceof FyndError && e.code).toBe('CONFIG');
    }
  });

  it('route is undefined when wire route is null', () => {
    const wire: WireSolution = {
      ...baseWireSolution,
      orders: [{ ...baseWireSolution.orders[0]!, route: null }],
    };
    const quote = fromWireQuote(wire, TOKEN_OUT, SENDER);
    expect(quote.route).toBeUndefined();
  });

  it('maps route swaps when route is present', () => {
    const wire: WireSolution = {
      ...baseWireSolution,
      orders: [
        {
          ...baseWireSolution.orders[0]!,
          route: {
            swaps: [
              {
                component_id: '0xpool',
                protocol:     'uniswap_v2',
                token_in:     TOKEN_IN,
                token_out:    TOKEN_OUT,
                amount_in:    '1000',
                amount_out:   '3500',
                gas_estimate: '80000',
              },
            ],
          },
        },
      ],
    };
    const quote = fromWireQuote(wire, TOKEN_OUT, SENDER);
    expect(quote.route).toBeDefined();
    expect(quote.route?.swaps).toHaveLength(1);
    const swap = quote.route?.swaps[0];
    expect(swap?.poolId).toBe('0xpool');
    expect(swap?.protocol).toBe('uniswap_v2');
    expect(swap?.amountIn).toBe(1000n);
    expect(swap?.amountOut).toBe(3500n);
    expect(swap?.gasEstimate).toBe(80000n);
  });

  it('maps component_id to poolId', () => {
    const wire: WireSolution = {
      ...baseWireSolution,
      orders: [
        {
          ...baseWireSolution.orders[0]!,
          route: {
            swaps: [
              {
                component_id: '0xpool123',
                protocol:     'uniswap_v3',
                token_in:     TOKEN_IN,
                token_out:    TOKEN_OUT,
                amount_in:    '100',
                amount_out:   '200',
                gas_estimate: '50000',
              },
            ],
          },
        },
      ],
    };
    const quote = fromWireQuote(wire, TOKEN_OUT, SENDER);
    expect(quote.route?.swaps[0]?.poolId).toBe('0xpool123');
  });

  it('priceImpactBps is undefined when wire value is null', () => {
    const wire: WireSolution = {
      ...baseWireSolution,
      orders: [{ ...baseWireSolution.orders[0]!, price_impact_bps: null }],
    };
    const quote = fromWireQuote(wire, TOKEN_OUT, SENDER);
    expect(quote.priceImpactBps).toBeUndefined();
  });

  it('priceImpactBps is set when wire value is present', () => {
    const wire: WireSolution = {
      ...baseWireSolution,
      orders: [{ ...baseWireSolution.orders[0]!, price_impact_bps: 15 }],
    };
    const quote = fromWireQuote(wire, TOKEN_OUT, SENDER);
    expect(quote.priceImpactBps).toBe(15);
  });

  it('propagates tokenOut and receiver from caller args', () => {
    const quote = fromWireQuote(baseWireSolution, TOKEN_OUT, RECEIVER);
    expect(quote.tokenOut).toBe(TOKEN_OUT);
    expect(quote.receiver).toBe(RECEIVER);
  });
});

describe('fromWireHealth', () => {
  it('maps all fields to camelCase', () => {
    const health = fromWireHealth({
      healthy:          true,
      last_update_ms:   1250,
      num_solver_pools: 2,
    });
    expect(health.healthy).toBe(true);
    expect(health.lastUpdateMs).toBe(1250);
    expect(health.numSolverPools).toBe(2);
  });

  it('maps healthy=false correctly', () => {
    const health = fromWireHealth({
      healthy:          false,
      last_update_ms:   5000,
      num_solver_pools: 0,
    });
    expect(health.healthy).toBe(false);
    expect(health.lastUpdateMs).toBe(5000);
    expect(health.numSolverPools).toBe(0);
  });
});
