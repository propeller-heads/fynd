import type { AccountOverride, Address, Hex, SimulationOverrides, Swap } from "./types.js";

interface MergedAccount {
    address: Address;
    balance: bigint | undefined;
    nonce: number | undefined;
    stateDiff: Map<string, { slot: Hex; value: Hex }>;
}

/**
 * Merges the simulation overrides of a route's pAMM swaps into one set to simulate the route with.
 * Returns `undefined` when no swap carries overrides.
 *
 * These are the rules Fynd's own simulation merges by. The accounts are a union, per account and
 * per storage slot, and list each address once, as viem's `stateOverride` requires. Where two swaps
 * set one balance, nonce or slot, the swap with the newer `blockNumber` wins: a swap naming no block
 * loses to any that names one, and between two swaps naming the same block the later one in the
 * route wins. The block is the newest one any swap names, with its timestamp.
 *
 * The result holds the venues' state only. Add the overrides that fund your sender on top, keeping
 * your funding wherever both set a value, as Fynd does.
 *
 * @param swaps - The swaps of one route, in route order.
 */
export function mergeSimulationOverrides(
    swaps: readonly Swap[],
): SimulationOverrides | undefined {
    const contributing = swaps.flatMap((swap) =>
        swap.simulationOverrides !== undefined ? [swap.simulationOverrides] : [],
    );
    if (contributing.length === 0) {
        return undefined;
    }
    // Merged oldest block first, so a newer block's value overwrites an older one's. The sort is
    // stable, so route order stands within one block, and a missing block sorts before every block.
    contributing.sort((a, b) => (a.blockNumber ?? -1) - (b.blockNumber ?? -1));

    let blockNumber: number | undefined;
    let blockTimestamp: number | undefined;
    const accounts = new Map<string, MergedAccount>();
    for (const overrides of contributing) {
        if (overrides.blockNumber !== undefined) {
            if (overrides.blockNumber === blockNumber) {
                blockTimestamp ??= overrides.blockTimestamp;
            } else {
                blockNumber = overrides.blockNumber;
                blockTimestamp = overrides.blockTimestamp;
            }
        }
        for (const account of overrides.accounts) {
            const key = account.address.toLowerCase();
            let merged = accounts.get(key);
            if (merged === undefined) {
                merged = {
                    address: account.address,
                    balance: undefined,
                    nonce: undefined,
                    stateDiff: new Map(),
                };
                accounts.set(key, merged);
            }
            if (account.balance !== undefined) {
                merged.balance = account.balance;
            }
            if (account.nonce !== undefined) {
                merged.nonce = account.nonce;
            }
            for (const entry of account.stateDiff ?? []) {
                merged.stateDiff.set(entry.slot.toLowerCase(), entry);
            }
        }
    }

    return {
        ...(blockNumber !== undefined ? { blockNumber } : {}),
        ...(blockTimestamp !== undefined ? { blockTimestamp } : {}),
        accounts: [...accounts.values()].map(toAccountOverride),
    };
}

function toAccountOverride({ address, balance, nonce, stateDiff }: MergedAccount): AccountOverride {
    return {
        address,
        ...(balance !== undefined ? { balance } : {}),
        ...(nonce !== undefined ? { nonce } : {}),
        ...(stateDiff.size > 0 ? { stateDiff: [...stateDiff.values()] } : {}),
    };
}
