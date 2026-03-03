import { describe, it, expect, vi } from 'vitest';
import { FyndClient } from './client.js';
import { FyndError } from './error.js';
import type { EthProvider, MinimalReceipt, FyndClientOptions } from './client.js';
import type { Address, Hex } from './types.js';

const ROUTER  = '0x1111111111111111111111111111111111111111' as Address;
const SENDER  = '0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045' as Address;
const TOKEN_IN  = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2' as Address;
const TOKEN_OUT = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48' as Address;

// Build a mock provider with all methods returning sensible defaults
function makeMockProvider(): { [K in keyof EthProvider]: ReturnType<typeof vi.fn> } & EthProvider {
  return {
    getTransactionCount:    vi.fn().mockResolvedValue(5),
    estimateFeesPerGas:     vi.fn().mockResolvedValue({ maxFeePerGas: 20n, maxPriorityFeePerGas: 2n }),
    call:                   vi.fn().mockResolvedValue({ data: undefined }),
    estimateGas:            vi.fn().mockResolvedValue(150000n),
    sendRawTransaction:     vi.fn().mockResolvedValue('0xtxhash' as Hex),
    getTransactionReceipt:  vi.fn().mockResolvedValue(null),
  };
}

// Minimal mock that replaces the openapi-fetch HTTP layer
function makeClientWithHttpMock(
  solveResponse: { status: number; data?: unknown },
  healthResponse?: { status: number; data?: unknown },
  opts?: Partial<FyndClientOptions>,
): FyndClient {
  const client = new FyndClient({
    baseUrl:       'http://localhost:8080',
    chainId:       1,
    routerAddress: ROUTER,
    sender:        SENDER,
    ...opts,
  });

  // Override the private http client by accessing it via a cast
  const httpMock = {
    POST: vi.fn().mockImplementation(() => {
      if (solveResponse.status >= 200 && solveResponse.status < 300) {
        return Promise.resolve({ data: solveResponse.data, error: undefined, response: {} });
      }
      return Promise.resolve({ data: undefined, error: solveResponse.data, response: {} });
    }),
    GET: vi.fn().mockImplementation(() => {
      if (healthResponse) {
        if (healthResponse.status >= 200 && healthResponse.status < 300) {
          return Promise.resolve({ data: healthResponse.data, error: undefined, response: {} });
        }
        return Promise.resolve({ data: undefined, error: healthResponse.data, response: {} });
      }
      return Promise.resolve({ data: undefined, error: undefined, response: {} });
    }),
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  (client as any).http = httpMock;
  return client;
}

const wireSolution = {
  orders: [
    {
      order_id:           'order-1',
      status:             'success',
      amount_in:          '1000',
      amount_out:         '3500',
      amount_out_net_gas: '3498',
      gas_estimate:       '150000',
      route:              null,
      price_impact_bps:   null,
      block: { hash: '0xabc', number: 100, timestamp: 1000000 },
    },
  ],
  total_gas_estimate: '150000',
  solve_time_ms:      10,
};

describe('FyndClient.quote — happy path', () => {
  it('returns a Quote with correct fields', async () => {
    const client = makeClientWithHttpMock({ status: 200, data: wireSolution });
    const quote = await client.quote({
      order: { tokenIn: TOKEN_IN, tokenOut: TOKEN_OUT, amount: 1000n, side: 'sell', sender: SENDER },
    });
    expect(quote.orderId).toBe('order-1');
    expect(quote.status).toBe('success');
    expect(quote.backend).toBe('fynd');
    expect(quote.amountIn).toBe(1000n);
    expect(quote.amountOut).toBe(3500n);
    expect(quote.gasEstimate).toBe(150000n);
    expect(quote.tokenOut).toBe(TOKEN_OUT);
  });

  it('receiver defaults to sender when Order.receiver is absent', async () => {
    const client = makeClientWithHttpMock({ status: 200, data: wireSolution });
    const quote = await client.quote({
      order: { tokenIn: TOKEN_IN, tokenOut: TOKEN_OUT, amount: 1000n, side: 'sell', sender: SENDER },
    });
    expect(quote.receiver).toBe(SENDER);
  });

  it('receiver uses Order.receiver when present', async () => {
    const altReceiver = '0x2222222222222222222222222222222222222222' as Address;
    const client = makeClientWithHttpMock({ status: 200, data: wireSolution });
    const quote = await client.quote({
      order: {
        tokenIn:  TOKEN_IN,
        tokenOut: TOKEN_OUT,
        amount:   1000n,
        side:     'sell',
        sender:   SENDER,
        receiver: altReceiver,
      },
    });
    expect(quote.receiver).toBe(altReceiver);
  });
});

describe('FyndClient.quote — error path', () => {
  it('throws FyndError with NO_ROUTE_FOUND code', async () => {
    const client = makeClientWithHttpMock({
      status: 422,
      data:   { code: 'NO_ROUTE_FOUND', error: 'no route' },
    });
    await expect(
      client.quote({
        order: { tokenIn: TOKEN_IN, tokenOut: TOKEN_OUT, amount: 1000n, side: 'sell', sender: SENDER },
      }),
    ).rejects.toThrow(FyndError);

    try {
      await client.quote({
        order: { tokenIn: TOKEN_IN, tokenOut: TOKEN_OUT, amount: 1000n, side: 'sell', sender: SENDER },
      });
    } catch (e) {
      expect(e instanceof FyndError).toBe(true);
      if (e instanceof FyndError) {
        expect(e.code).toBe('NO_ROUTE_FOUND');
        expect(e.isRetryable()).toBe(false);
      }
    }
  });
});

describe('FyndClient.quote — retry path', () => {
  it('retries on QUEUE_FULL and succeeds on second attempt', async () => {
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      retry:         { maxAttempts: 3, initialBackoffMs: 1, maxBackoffMs: 10 },
    });

    let callCount = 0;
    const httpMock = {
      POST: vi.fn().mockImplementation(() => {
        callCount++;
        if (callCount === 1) {
          return Promise.resolve({
            data:     undefined,
            error:    { code: 'QUEUE_FULL', error: 'queue full' },
            response: {},
          });
        }
        return Promise.resolve({ data: wireSolution, error: undefined, response: {} });
      }),
      GET: vi.fn(),
    };
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (client as any).http = httpMock;

    const quote = await client.quote({
      order: { tokenIn: TOKEN_IN, tokenOut: TOKEN_OUT, amount: 1000n, side: 'sell', sender: SENDER },
    });
    expect(callCount).toBe(2);
    expect(quote.status).toBe('success');
  });

  it('does not retry on non-retryable errors', async () => {
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      retry:         { maxAttempts: 3, initialBackoffMs: 1 },
    });

    let callCount = 0;
    const httpMock = {
      POST: vi.fn().mockImplementation(() => {
        callCount++;
        return Promise.resolve({
          data:     undefined,
          error:    { code: 'BAD_REQUEST', error: 'bad request' },
          response: {},
        });
      }),
      GET: vi.fn(),
    };
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (client as any).http = httpMock;

    await expect(
      client.quote({
        order: { tokenIn: TOKEN_IN, tokenOut: TOKEN_OUT, amount: 1000n, side: 'sell', sender: SENDER },
      }),
    ).rejects.toThrow(FyndError);
    expect(callCount).toBe(1);
  });
});

describe('FyndClient.health', () => {
  it('returns HealthStatus on 200', async () => {
    const client = makeClientWithHttpMock(
      { status: 200, data: wireSolution },
      { status: 200, data: { healthy: true, last_update_ms: 500, num_solver_pools: 3 } },
    );
    const health = await client.health();
    expect(health.healthy).toBe(true);
    expect(health.lastUpdateMs).toBe(500);
    expect(health.numSolverPools).toBe(3);
  });

  it('throws FyndError on 503', async () => {
    const client = makeClientWithHttpMock(
      { status: 200, data: wireSolution },
      { status: 503, data: { code: 'STALE_DATA', error: 'data stale' } },
    );
    await expect(client.health()).rejects.toThrow(FyndError);
  });
});

describe('FyndClient.signablePayload', () => {
  it('throws CONFIG error when provider is not set', async () => {
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
    });
    const quote = { ...makeDummyQuote() };
    await expect(client.signablePayload(quote)).rejects.toThrow(FyndError);
    try {
      await client.signablePayload(quote);
    } catch (e) {
      expect(e instanceof FyndError && e.code).toBe('CONFIG');
    }
  });

  it('throws CONFIG error when no sender configured', async () => {
    const provider = makeMockProvider();
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      provider,
      // no sender
    });
    const quote = makeDummyQuote();
    await expect(client.signablePayload(quote)).rejects.toThrow(FyndError);
    try {
      await client.signablePayload(quote);
    } catch (e) {
      expect(e instanceof FyndError && e.code).toBe('CONFIG');
    }
  });

  it('builds transaction with correct to and value', async () => {
    const provider = makeMockProvider();
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote();
    const payload = await client.signablePayload(quote);
    expect(payload.kind).toBe('fynd');
    expect(payload.payload.tx.to).toBe(ROUTER);
    expect(payload.payload.tx.value).toBe(0n);
    expect(payload.payload.tx.data).toBe('0x');
  });

  it('uses hints.sender over options.sender', async () => {
    const provider = makeMockProvider();
    const altSender = '0x9999999999999999999999999999999999999999' as Address;
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote();
    await client.signablePayload(quote, { sender: altSender });
    expect(provider.getTransactionCount).toHaveBeenCalledWith({ address: altSender });
  });

  it('uses hints.nonce without calling getTransactionCount', async () => {
    const provider = makeMockProvider();
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote();
    const payload = await client.signablePayload(quote, { nonce: 99 });
    expect(payload.payload.tx.nonce).toBe(99);
    expect(provider.getTransactionCount).not.toHaveBeenCalled();
  });

  it('uses hints.maxFeePerGas without calling estimateFeesPerGas', async () => {
    const provider = makeMockProvider();
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote();
    const payload = await client.signablePayload(quote, {
      maxFeePerGas:         50n,
      maxPriorityFeePerGas: 5n,
    });
    expect(payload.payload.tx.maxFeePerGas).toBe(50n);
    expect(payload.payload.tx.maxPriorityFeePerGas).toBe(5n);
    expect(provider.estimateFeesPerGas).not.toHaveBeenCalled();
  });

  it('uses hints.gasLimit over quote.gasEstimate', async () => {
    const provider = makeMockProvider();
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote(); // gasEstimate = 150000n
    const payload = await client.signablePayload(quote, { gasLimit: 200000n });
    expect(payload.payload.tx.gas).toBe(200000n);
  });

  it('calls provider.call when hints.simulate is true', async () => {
    const provider = makeMockProvider();
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote();
    await client.signablePayload(quote, { simulate: true });
    expect(provider.call).toHaveBeenCalledOnce();
  });

  it('throws SIMULATE_FAILED when provider.call throws during simulation', async () => {
    const provider = makeMockProvider();
    provider.call.mockRejectedValueOnce(new Error('execution reverted'));
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });
    const quote = makeDummyQuote();
    await expect(client.signablePayload(quote, { simulate: true })).rejects.toThrow(FyndError);
    try {
      await client.signablePayload(quote, { simulate: true });
    } catch (e) {
      expect(e instanceof FyndError && e.code).toBe('SIMULATE_FAILED');
    }
  });
});

describe('FyndClient.execute — standard path', () => {
  it('calls sendRawTransaction and returns receipt with settle()', async () => {
    const provider = makeMockProvider();
    provider.sendRawTransaction.mockResolvedValueOnce('0xhash123' as Hex);
    const receipt: MinimalReceipt = {
      transactionHash: '0xhash123' as Hex,
      gasUsed:         150000n,
      effectiveGasPrice: 20n,
      logs:            [],
    };
    provider.getTransactionReceipt.mockResolvedValueOnce(receipt);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(makeDummyQuote());
    const executionReceipt = await client.execute(signedOrder);
    expect(provider.sendRawTransaction).toHaveBeenCalledOnce();

    const settled = await executionReceipt.settle();
    expect(settled.txHash).toBe('0xhash123');
    expect(settled.gasCost).toBe(3000000n); // 150000 * 20
  });

  it('throws CONFIG when provider not set', async () => {
    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
    });
    const signedOrder = makeSignedOrder(makeDummyQuote());
    await expect(client.execute(signedOrder)).rejects.toThrow(FyndError);
  });
});

describe('FyndClient.execute — dry-run path', () => {
  it('calls provider.call and estimateGas, not sendRawTransaction', async () => {
    const provider = makeMockProvider();
    // Return 32 bytes of return data (1000 in big-endian uint256)
    // 64 hex chars total: 61 zeros + '3e8' (0x3e8 = 1000)
    const returnHex = ('0x' + '0'.repeat(61) + '3e8') as Hex;  // 0x3e8 = 1000
    provider.call.mockResolvedValueOnce({ data: returnHex as Hex });
    provider.estimateGas.mockResolvedValueOnce(100000n);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(makeDummyQuote());
    const executionReceipt = await client.execute(signedOrder, { dryRun: true });
    expect(provider.sendRawTransaction).not.toHaveBeenCalled();
    expect(provider.call).toHaveBeenCalledOnce();
    expect(provider.estimateGas).toHaveBeenCalledOnce();

    const settled = await executionReceipt.settle();
    expect(settled.txHash).toBeUndefined();
    expect(settled.gasCost).toBe(100000n * 20000000000n); // gasUsed * maxFeePerGas
  });

  it('settle() resolves immediately for dry-run', async () => {
    const provider = makeMockProvider();
    provider.call.mockResolvedValueOnce({ data: undefined });
    provider.estimateGas.mockResolvedValueOnce(50000n);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(makeDummyQuote());
    const executionReceipt = await client.execute(signedOrder, { dryRun: true });
    // Should resolve without any polling
    const settled = await executionReceipt.settle();
    expect(provider.getTransactionReceipt).not.toHaveBeenCalled();
    expect(settled.settledAmount).toBeUndefined();
  });

  it('settledAmount decoded from 32-byte return data', async () => {
    const provider = makeMockProvider();
    // 32 bytes = 64 hex chars: represent value 0x123 = 291
    // Pad to exactly 64 hex chars: 61 zeros + '123'
    const returnHex = ('0x' + '0'.repeat(61) + '123') as Hex;
    provider.call.mockResolvedValueOnce({ data: returnHex });
    provider.estimateGas.mockResolvedValueOnce(50000n);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(makeDummyQuote());
    const receipt = await client.execute(signedOrder, { dryRun: true });
    const settled = await receipt.settle();
    expect(settled.settledAmount).toBe(0x123n);
  });

  it('settledAmount is undefined when return data is absent', async () => {
    const provider = makeMockProvider();
    provider.call.mockResolvedValueOnce({ data: undefined });
    provider.estimateGas.mockResolvedValueOnce(50000n);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(makeDummyQuote());
    const receipt = await client.execute(signedOrder, { dryRun: true });
    const settled = await receipt.settle();
    expect(settled.settledAmount).toBeUndefined();
  });

  it('throws SIMULATE_FAILED when provider.call throws during dry-run', async () => {
    const provider = makeMockProvider();
    provider.call.mockRejectedValueOnce(new Error('execution reverted'));

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(makeDummyQuote());
    await expect(client.execute(signedOrder, { dryRun: true })).rejects.toThrow(FyndError);
    try {
      await client.execute(signedOrder, { dryRun: true });
    } catch (e) {
      expect(e instanceof FyndError && e.code).toBe('SIMULATE_FAILED');
    }
  });

  it('uses maxFeePerGas for dry-run gasCost calculation', async () => {
    const provider = makeMockProvider();
    provider.call.mockResolvedValueOnce({ data: undefined });
    provider.estimateGas.mockResolvedValueOnce(200000n);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const quote = makeDummyQuote();
    // maxFeePerGas is set in signablePayload; here we create a signed order directly
    const signedOrder = makeSignedOrder(quote, { maxFeePerGas: 30n });
    const receipt = await client.execute(signedOrder, { dryRun: true });
    const settled = await receipt.settle();
    expect(settled.gasCost).toBe(200000n * 30n); // gasUsed * maxFeePerGas
  });
});

// Transfer log decoding — tested indirectly via execute settle()
describe('Transfer log decoding via settle()', () => {
  const ERC20_TOPIC = '0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef' as Hex;
  const ERC6909_TOPIC = '0x1b3d7edb2e9c0b0e7c525b20aaaef0f5940d2ed71663c7d39266ecafac728859' as Hex;

  function padAddress(addr: string): Hex {
    return `0x${'0'.repeat(24)}${addr.slice(2)}` as Hex;
  }

  function makeReceiptWithLogs(logs: MinimalReceipt['logs']): MinimalReceipt {
    return {
      transactionHash: '0xtxhash' as Hex,
      gasUsed:         100000n,
      effectiveGasPrice: 10n,
      logs,
    };
  }

  async function executeAndSettle(
    quote: ReturnType<typeof makeDummyQuote>,
    receipt: MinimalReceipt,
  ) {
    const provider = makeMockProvider();
    provider.sendRawTransaction.mockResolvedValueOnce('0xtxhash' as Hex);
    provider.getTransactionReceipt.mockResolvedValueOnce(receipt);

    const client = new FyndClient({
      baseUrl:       'http://localhost:8080',
      chainId:       1,
      routerAddress: ROUTER,
      sender:        SENDER,
      provider,
    });

    const signedOrder = makeSignedOrder(quote);
    const executionReceipt = await client.execute(signedOrder);
    return executionReceipt.settle();
  }

  it('ERC-20: matching log returns correct amount', async () => {
    const amount = 3500000000n;
    const amountHex = amount.toString(16).padStart(64, '0');
    const log = {
      address: TOKEN_OUT,
      topics: [ERC20_TOPIC, padAddress(SENDER), padAddress(SENDER)] as Hex[],
      data:   `0x${amountHex}` as Hex,
    };
    const settled = await executeAndSettle(makeDummyQuote(), makeReceiptWithLogs([log]));
    expect(settled.settledAmount).toBe(amount);
  });

  it('ERC-20: wrong token address returns undefined', async () => {
    const wrongToken = '0x0000000000000000000000000000000000000001' as Address;
    const log = {
      address: wrongToken,
      topics: [ERC20_TOPIC, padAddress(SENDER), padAddress(SENDER)] as Hex[],
      data:   `0x${'0'.repeat(63)}1` as Hex,
    };
    const settled = await executeAndSettle(makeDummyQuote(), makeReceiptWithLogs([log]));
    expect(settled.settledAmount).toBeUndefined();
  });

  it('ERC-20: wrong receiver returns undefined', async () => {
    const wrongReceiver = '0x9999999999999999999999999999999999999999' as Address;
    const log = {
      address: TOKEN_OUT,
      topics: [ERC20_TOPIC, padAddress(SENDER), padAddress(wrongReceiver)] as Hex[],
      data:   `0x${'0'.repeat(63)}1` as Hex,
    };
    const settled = await executeAndSettle(makeDummyQuote(), makeReceiptWithLogs([log]));
    expect(settled.settledAmount).toBeUndefined();
  });

  it('ERC-6909: matching log returns correct amount at bytes 32..64', async () => {
    const amount = 12345n;
    const amountHex = amount.toString(16).padStart(64, '0');
    // data: caller[32 bytes] + amount[32 bytes]
    const callerHex = '0'.repeat(64);
    const log = {
      address: TOKEN_OUT,
      topics: [ERC6909_TOPIC, padAddress(SENDER), padAddress(SENDER)] as Hex[],
      data:   `0x${callerHex}${amountHex}` as Hex,
    };
    const settled = await executeAndSettle(makeDummyQuote(), makeReceiptWithLogs([log]));
    expect(settled.settledAmount).toBe(amount);
  });

  it('multiple matching logs: amounts are summed', async () => {
    const amountHex1 = (1000n).toString(16).padStart(64, '0');
    const amountHex2 = (2000n).toString(16).padStart(64, '0');
    const logs = [
      {
        address: TOKEN_OUT,
        topics: [ERC20_TOPIC, padAddress(SENDER), padAddress(SENDER)] as Hex[],
        data:   `0x${amountHex1}` as Hex,
      },
      {
        address: TOKEN_OUT,
        topics: [ERC20_TOPIC, padAddress(SENDER), padAddress(SENDER)] as Hex[],
        data:   `0x${amountHex2}` as Hex,
      },
    ];
    const settled = await executeAndSettle(makeDummyQuote(), makeReceiptWithLogs(logs));
    expect(settled.settledAmount).toBe(3000n);
  });

  it('empty logs returns undefined', async () => {
    const settled = await executeAndSettle(makeDummyQuote(), makeReceiptWithLogs([]));
    expect(settled.settledAmount).toBeUndefined();
  });
});

// ---- helpers ----

function makeDummyQuote(overrides?: Partial<{ gasEstimate: bigint }>) {
  return {
    orderId:     'test-order',
    status:      'success' as const,
    backend:     'fynd' as const,
    amountIn:    1000n,
    amountOut:   3500n,
    gasEstimate: overrides?.gasEstimate ?? 150000n,
    block: { hash: '0xabc', number: 100, timestamp: 1000 },
    tokenOut: TOKEN_OUT,
    receiver: SENDER,
  };
}

function makeSignedOrder(
  quote: ReturnType<typeof makeDummyQuote>,
  txOverrides?: { maxFeePerGas?: bigint },
) {
  const tx = {
    chainId:              1,
    nonce:                0,
    maxFeePerGas:         txOverrides?.maxFeePerGas ?? 20000000000n,
    maxPriorityFeePerGas: 2000000000n,
    gas:                  quote.gasEstimate,
    to:                   ROUTER,
    value:                0n,
    data:                 '0x' as Hex,
  };
  const payload = { kind: 'fynd' as const, payload: { quote, tx } };
  // A valid 65-byte hex signature (r[32]+s[32]+v[1])
  const sig = `0x${'ab'.repeat(32)}${'cd'.repeat(32)}00` as `0x${string}`;
  return { payload, signature: sig };
}
