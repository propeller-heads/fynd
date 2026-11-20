import { describe, expect, it } from 'vitest';
import type { StateOverride } from 'viem';
import { mergeSimulationOverrides } from './simulation-overrides.js';
import type { AccountOverride, Address, Hex, SimulationOverrides, Swap } from './types.js';

const word = (byte: number): Hex => `0x${byte.toString(16).padStart(64, '0')}`;
const address = (byte: number): Address => `0x${byte.toString(16).padStart(2, '0').repeat(20)}`;
/** The timestamp the tests give `block`: twelve seconds per block from a fixed origin. */
const timestampOf = (block: number): number => 1_500_000_000 + 12 * block;

/** An account at the address repeating `byte`, overriding one slot per `[slot, value]` pair. */
function storage(byte: number, slots: [number, number][]): AccountOverride {
  return { address: address(byte), stateDiff: slots.map(([slot, value]) => ({ slot: word(slot), value: word(value) })) };
}

/** Overrides for `block` (with its timestamp) over `accounts`. */
function overrides(block: number | undefined, accounts: AccountOverride[]): SimulationOverrides {
  return block === undefined ? { accounts } : { blockNumber: block, blockTimestamp: timestampOf(block), accounts };
}

function swap(simulationOverrides?: SimulationOverrides): Swap {
  return {
    componentId: 'pool',
    protocol: 'fallback:fermiswap',
    tokenIn: address(0x01),
    tokenOut: address(0x02),
    amountIn: 1000n,
    amountOut: 990n,
    gasEstimate: 50000n,
    ...(simulationOverrides !== undefined ? { simulationOverrides } : {}),
  };
}

function merged(...each: (SimulationOverrides | undefined)[]): SimulationOverrides {
  const result = mergeSimulationOverrides(each.map(swap));
  if (result === undefined) {
    throw new Error('expected merged overrides');
  }
  return result;
}

function slotOf(result: SimulationOverrides, byte: number, slot: number): Hex | undefined {
  return result.accounts
    .find((account) => account.address === address(byte))
    ?.stateDiff?.find((entry) => entry.slot === word(slot))?.value;
}

describe('mergeSimulationOverrides', () => {
  it('returns undefined when no swap carries overrides', () => {
    expect(mergeSimulationOverrides([])).toBeUndefined();
    expect(mergeSimulationOverrides([swap(), swap()])).toBeUndefined();
  });

  it('passes a single swap\'s overrides through unchanged', () => {
    const only = overrides(10, [storage(0xa1, [[0, 1]])]);

    expect(merged(undefined, only, undefined)).toEqual(only);
  });

  it('unions accounts and slots, listing each address once', () => {
    const result = merged(
      overrides(10, [storage(0xa1, [[0, 1]]), storage(0xb2, [[0, 2]])]),
      overrides(10, [storage(0xa1, [[1, 3]])]),
    );

    expect(result.accounts.map((account) => account.address)).toEqual([address(0xa1), address(0xb2)]);
    expect(slotOf(result, 0xa1, 0)).toBe(word(1));
    expect(slotOf(result, 0xa1, 1)).toBe(word(3));
    expect(slotOf(result, 0xb2, 0)).toBe(word(2));
    // Compiles only while the merged accounts stay assignable to viem's `stateOverride`.
    const stateOverride: StateOverride = result.accounts;
    expect(new Set(stateOverride.map((account) => account.address.toLowerCase())).size).toBe(stateOverride.length);
  });

  it('treats addresses that differ only in case as one account', () => {
    const upper = `0x${'A1'.repeat(20)}` as Address;
    const result = merged(
      overrides(10, [{ address: upper, nonce: 1 }]),
      overrides(10, [{ address: address(0xa1), nonce: 2 }]),
    );

    expect(result.accounts).toEqual([{ address: upper, nonce: 2 }]);
  });

  it('lets the later swap win a conflict within one block', () => {
    const result = merged(
      overrides(10, [{ ...storage(0xa1, [[0, 1], [1, 7]]), balance: 1n, nonce: 1 }]),
      overrides(10, [{ ...storage(0xa1, [[0, 2]]), balance: 2n }]),
    );
    const account = result.accounts[0];

    expect(account?.balance).toBe(2n);
    expect(account?.nonce).toBe(1);
    expect(slotOf(result, 0xa1, 0)).toBe(word(2));
    expect(slotOf(result, 0xa1, 1)).toBe(word(7));
  });

  it('lets the newest block win a conflict, whatever the route order', () => {
    const result = merged(
      overrides(11, [storage(0xa1, [[0, 11]])]),
      overrides(10, [storage(0xa1, [[0, 10], [1, 10]])]),
      overrides(undefined, [storage(0xa1, [[0, 1], [2, 1]])]),
    );

    expect(result.blockNumber).toBe(11);
    expect(slotOf(result, 0xa1, 0)).toBe(word(11));
    expect(slotOf(result, 0xa1, 1)).toBe(word(10));
    expect(slotOf(result, 0xa1, 2)).toBe(word(1));
  });

  it('takes the newest block with its own timestamp', () => {
    const untimed = (block: number): SimulationOverrides => ({ blockNumber: block, accounts: [] });

    const newerUntimed = merged(overrides(10, []), untimed(11));
    const filledIn = merged(untimed(11), overrides(11, []), overrides(undefined, []));
    const unnamed = merged(overrides(undefined, []));

    expect(newerUntimed).toEqual({ blockNumber: 11, accounts: [] });
    expect(filledIn).toEqual({ blockNumber: 11, blockTimestamp: timestampOf(11), accounts: [] });
    expect(unnamed).toEqual({ accounts: [] });
  });
});
