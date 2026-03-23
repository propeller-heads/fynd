import type { Address, Hex } from './types.js';
import type { EthProvider } from './client.js';

const ERC20_ALLOWANCE_ABI = [{
  name: 'allowance', type: 'function' as const,
  inputs: [{ name: 'owner', type: 'address' }, { name: 'spender', type: 'address' }],
  outputs: [{ type: 'uint256' }],
  stateMutability: 'view' as const,
}] as const;

/**
 * Minimal subset of viem's `PublicClient` used by {@link viemProvider}.
 *
 * Any viem `PublicClient` created via `createPublicClient` satisfies this interface.
 */
export interface ViemPublicClient {
  getTransactionCount(
    args: { address: Address },
  ): Promise<number>;
  estimateFeesPerGas(): Promise<{
    maxFeePerGas: bigint | null;
    maxPriorityFeePerGas: bigint | null;
  }>;
  call(
    args: {
      to: Address;
      data: Hex;
      value: bigint;
      gas: bigint;
      maxFeePerGas: bigint;
      maxPriorityFeePerGas: bigint;
    },
  ): Promise<{ data?: Hex | undefined }>;
  estimateGas(
    args: {
      account: Address;
      to: Address;
      data: Hex;
      value: bigint;
      maxFeePerGas: bigint;
      maxPriorityFeePerGas: bigint;
    },
  ): Promise<bigint>;
  sendRawTransaction(
    args: { serializedTransaction: Hex },
  ): Promise<Hex>;
  getTransactionReceipt(
    args: { hash: Hex },
  ): Promise<{
    transactionHash: Hex;
    gasUsed: bigint;
    effectiveGasPrice: bigint;
    logs: ReadonlyArray<{
      address: string;
      topics: readonly string[];
      data: string;
    }>;
  }>;
  readContract(args: {
    address: Address;
    abi: readonly unknown[];
    functionName: string;
    args?: readonly unknown[];
  }): Promise<unknown>;
}

/**
 * Adapts a viem `PublicClient` into an {@link EthProvider} for use with {@link FyndClient}.
 *
 * @example
 * ```ts
 * const provider = viemProvider(publicClient, senderAddress);
 * const client = new FyndClient({ baseUrl, chainId, provider });
 * ```
 */
export function viemProvider(
  client: ViemPublicClient,
  sender: Address,
): EthProvider {
  return {
    async getTransactionCount(args) {
      return client.getTransactionCount(args);
    },
    async estimateFeesPerGas() {
      const fees = await client.estimateFeesPerGas();
      return {
        maxFeePerGas: fees.maxFeePerGas ?? 0n,
        maxPriorityFeePerGas: fees.maxPriorityFeePerGas ?? 0n,
      };
    },
    async call(tx) {
      const result = await client.call({
        to: tx.to,
        data: tx.data,
        value: tx.value,
        gas: tx.gas,
        maxFeePerGas: tx.maxFeePerGas,
        maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
      });
      return result.data !== undefined ? { data: result.data } : {};
    },
    async estimateGas(tx) {
      return client.estimateGas({
        account: sender,
        to: tx.to,
        data: tx.data,
        value: tx.value,
        maxFeePerGas: tx.maxFeePerGas,
        maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
      });
    },
    async sendRawTransaction(rawTx) {
      return client.sendRawTransaction({
        serializedTransaction: rawTx,
      });
    },
    async getTransactionReceipt(args) {
      let receipt;
      try {
        receipt = await client.getTransactionReceipt(args);
      } catch {
        // viem throws when the receipt is not yet available; settle() expects null.
        return null;
      }
      return {
        transactionHash: receipt.transactionHash,
        gasUsed: receipt.gasUsed,
        effectiveGasPrice: receipt.effectiveGasPrice,
        logs: receipt.logs.map((log) => ({
          address: log.address as Address,
          topics: log.topics as readonly Hex[],
          data: log.data as Hex,
        })),
      };
    },
    async readAllowance(token, owner, spender) {
      const result = await client.readContract({
        address: token, abi: ERC20_ALLOWANCE_ABI,
        functionName: 'allowance', args: [owner, spender],
      });
      return result as bigint;
    },
  };
}
