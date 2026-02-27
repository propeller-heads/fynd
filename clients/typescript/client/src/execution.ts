import { FyndClientError } from "./error.js";

export interface SettledOrder {
  txHash: string;
  /** Amount of output token actually received, from on-chain Transfer logs. */
  amountReceived: bigint;
  blockNumber: number;
}

/** ERC-20 Transfer event topic: keccak256("Transfer(address,address,uint256)") */
const TRANSFER_TOPIC =
  "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

export interface EthLog {
  address: string;
  topics: string[];
  data: string;
  blockNumber: string | number;
}

export interface TxReceipt {
  status: string; // "0x1" = success
  blockNumber: string | number;
  logs: EthLog[];
}

/**
 * Derives the amount received from on-chain Transfer event logs.
 *
 * Looks for ERC-20 Transfer events from `tokenOut` to `receiver`
 * and sums their values from log data.
 */
export function extractAmountReceived(
  receipt: TxReceipt,
  tokenOut: string,
  receiver: string,
): bigint {
  const tokenOutLower = tokenOut.toLowerCase();
  const receiverLower = receiver.toLowerCase().replace(/^0x/, "").padStart(40, "0");

  return receipt.logs
    .filter((log) => {
      if (log.address.toLowerCase() !== tokenOutLower) return false;
      if (log.topics[0]?.toLowerCase() !== TRANSFER_TOPIC) return false;
      // topics[2] is the indexed `to` address (padded to 32 bytes)
      const toTopic = log.topics[2]?.toLowerCase().slice(-40);
      return toTopic === receiverLower;
    })
    .reduce((acc, log) => {
      // Transfer value is the non-indexed data field (first 32 bytes = 256 bits)
      const value = BigInt("0x" + log.data.replace(/^0x/, "").slice(0, 64));
      return acc + value;
    }, 0n);
}

/**
 * A handle to a submitted transaction.
 * Call `settle(rpcUrl)` to wait for confirmation and parse logs.
 */
export class TransactionHandle {
  constructor(
    public readonly txHash: string,
    private readonly tokenOut: string,
    private readonly receiver: string,
  ) {}

  /**
   * Polls for the transaction receipt and returns a settled order.
   *
   * Uses eth_getTransactionReceipt via the provided RPC URL.
   * Derives amountReceived from ERC-20 Transfer logs — no debug RPC required.
   */
  async settle(rpcUrl: string, timeoutMs = 120_000): Promise<SettledOrder> {
    const deadline = Date.now() + timeoutMs;

    while (Date.now() < deadline) {
      const receipt = await this.getReceipt(rpcUrl);

      if (receipt !== null) {
        if (receipt.status !== "0x1") {
          throw new FyndClientError("transaction reverted", false);
        }

        const blockNumber =
          typeof receipt.blockNumber === "string"
            ? parseInt(receipt.blockNumber, 16)
            : receipt.blockNumber;

        const amountReceived = extractAmountReceived(
          receipt,
          this.tokenOut,
          this.receiver,
        );

        return { txHash: this.txHash, amountReceived, blockNumber };
      }

      await new Promise((r) => setTimeout(r, 2000));
    }

    throw new FyndClientError(
      `transaction ${this.txHash} not confirmed within ${timeoutMs}ms`,
      true,
    );
  }

  private async getReceipt(rpcUrl: string): Promise<TxReceipt | null> {
    const response = await fetch(rpcUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "eth_getTransactionReceipt",
        params: [this.txHash],
      }),
    });

    if (!response.ok) {
      throw new FyndClientError(
        `RPC request failed: ${response.status}`,
        response.status >= 500,
      );
    }

    const json = (await response.json()) as {
      result: TxReceipt | null;
      error?: { message: string };
    };
    if (json.error) {
      throw new FyndClientError(`RPC error: ${json.error.message}`, false);
    }
    return json.result;
  }
}

export type ExecutionReceipt =
  | { type: "transaction"; handle: TransactionHandle }
  | { type: "intent" }; // Turbine placeholder
