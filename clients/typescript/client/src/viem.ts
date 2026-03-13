import type { Address, Hex } from './types.js';
import type { EthProvider } from './client.js';

/**
 * Minimal subset of viem's PublicClient used by FyndClient.
 *
 * Declaring this interface avoids coupling to viem's full
 * type surface while remaining structurally compatible.
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
}

/**
 * Wraps a viem PublicClient into an EthProvider for FyndClient.
 *
 * @param client - A viem PublicClient (from createPublicClient)
 * @param sender - The sender address, used for estimateGas calls
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
      const receipt = await client.getTransactionReceipt(args);
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
  };
}
