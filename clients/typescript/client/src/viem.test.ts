import { describe, it, expect, vi } from 'vitest';
import { viemProvider } from './viem.js';
import type { ViemPublicClient } from './viem.js';
import type { Address, Hex } from './types.js';
import type { Eip1559Transaction } from './signing.js';

const SENDER = '0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045' as Address;
const ROUTER = '0x1111111111111111111111111111111111111111' as Address;

const TX: Eip1559Transaction = {
  chainId: 1,
  nonce: 5,
  maxFeePerGas: 20n,
  maxPriorityFeePerGas: 2n,
  gas: 150000n,
  to: ROUTER,
  value: 0n,
  data: '0xdeadbeef' as Hex,
};

function makeMockViemClient(): ViemPublicClient & { [K in 'call' | 'estimateGas']: ReturnType<typeof vi.fn> } {
  return {
    getTransactionCount:   vi.fn().mockResolvedValue(5),
    estimateFeesPerGas:    vi.fn().mockResolvedValue({ maxFeePerGas: 20n, maxPriorityFeePerGas: 2n }),
    call:                  vi.fn().mockResolvedValue({ data: undefined }),
    estimateGas:           vi.fn().mockResolvedValue(150000n),
    sendRawTransaction:    vi.fn().mockResolvedValue('0xtxhash' as Hex),
    getTransactionReceipt: vi.fn().mockResolvedValue(null),
    readContract:          vi.fn().mockResolvedValue(0n),
  } as unknown as ViemPublicClient & { call: ReturnType<typeof vi.fn>; estimateGas: ReturnType<typeof vi.fn> };
}

describe('viemProvider', () => {
  // A call without `account` runs from the zero address, so routers that pull funds with
  // `transferFrom(msg.sender, ...)` revert during simulation.
  it('sends the sender as the caller of eth_call', async () => {
    const viem = makeMockViemClient();
    await viemProvider(viem, SENDER).call(TX);
    expect(viem.call).toHaveBeenCalledWith(expect.objectContaining({ account: SENDER }));
  });

  it('sends the sender as the caller of eth_estimateGas', async () => {
    const viem = makeMockViemClient();
    await viemProvider(viem, SENDER).estimateGas(TX);
    expect(viem.estimateGas).toHaveBeenCalledWith(expect.objectContaining({ account: SENDER }));
  });

  it('forwards the transaction fields to eth_call', async () => {
    const viem = makeMockViemClient();
    await viemProvider(viem, SENDER).call(TX);
    expect(viem.call).toHaveBeenCalledWith({
      account: SENDER,
      to: TX.to,
      data: TX.data,
      value: TX.value,
      gas: TX.gas,
      maxFeePerGas: TX.maxFeePerGas,
      maxPriorityFeePerGas: TX.maxPriorityFeePerGas,
    });
  });

  it('returns the call result data when present', async () => {
    const viem = makeMockViemClient();
    viem.call.mockResolvedValueOnce({ data: '0x01' as Hex });
    const result = await viemProvider(viem, SENDER).call(TX);
    expect(result).toEqual({ data: '0x01' });
  });

  it('omits data when the call returns none', async () => {
    const viem = makeMockViemClient();
    const result = await viemProvider(viem, SENDER).call(TX);
    expect(result).toEqual({});
  });
});
