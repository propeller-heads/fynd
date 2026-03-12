import { keccak256, serializeTransaction, toHex } from 'viem';
import { createFyndClient, type FyndClient as AutogenClient } from "@fynd/autogen";
import type { components } from "@fynd/autogen";
import { FyndError } from "./error.js";
import * as mapping from "./mapping.js";
import type {
  Eip1559Transaction,
  ExecutionReceipt,
  FyndPayload,
  SettledOrder,
  SignablePayload,
  SignedOrder,
} from "./signing.js";
import type { Address, Hex, HealthStatus, Quote, QuoteParams } from "./types.js";

type WireErrorResponse = components["schemas"]["ErrorResponse"];

// ERC-20 Transfer(address,address,uint256)
const ERC20_TRANSFER_TOPIC = keccak256(toHex('Transfer(address,address,uint256)'));
// ERC-6909 Transfer(address,address,address,uint256,uint256)
const ERC6909_TRANSFER_TOPIC = keccak256(toHex('Transfer(address,address,address,uint256,uint256)'));

export interface MinimalReceipt {
  transactionHash: Hex;
  gasUsed: bigint;
  effectiveGasPrice: bigint;
  logs: Array<{ address: Address; topics: readonly Hex[]; data: Hex }>;
}

export interface EthProvider {
  getTransactionCount(args: { address: Address }): Promise<number>;
  estimateFeesPerGas(): Promise<{ maxFeePerGas: bigint; maxPriorityFeePerGas: bigint }>;
  call(tx: Eip1559Transaction): Promise<{ data?: Hex }>;
  estimateGas(tx: Eip1559Transaction): Promise<bigint>;
  sendRawTransaction(rawTx: Hex): Promise<Hex>;
  getTransactionReceipt(args: { hash: Hex }): Promise<MinimalReceipt | null>;
}

export interface RetryConfig {
  maxAttempts?: number;      // default: 3
  initialBackoffMs?: number; // default: 100
  maxBackoffMs?: number;     // default: 2_000
}

export interface SigningHints {
  sender?: Address;
  nonce?: number;
  maxFeePerGas?: bigint;
  maxPriorityFeePerGas?: bigint;
  gasLimit?: bigint;
  simulate?: boolean;
}

export interface ExecutionOptions {
  dryRun?: boolean;
}

export interface FyndClientOptions {
  baseUrl: string;
  chainId: number;
  sender?: Address;
  timeoutMs?: number;    // default: 30_000
  retry?: RetryConfig;
  provider?: EthProvider;
  submitProvider?: EthProvider;
}

export class FyndClient {
  private readonly http: AutogenClient;
  private readonly options: FyndClientOptions;

  constructor(options: FyndClientOptions) {
    this.http = createFyndClient(options.baseUrl);
    this.options = options;
  }

  async quote(params: QuoteParams): Promise<Quote> {
    const tokenOut = params.order.tokenOut;
    const receiver = params.order.receiver ?? params.order.sender;

    const retry = this.options.retry ?? {};
    const maxAttempts = retry.maxAttempts ?? 3;
    const initialBackoffMs = retry.initialBackoffMs ?? 100;
    const maxBackoffMs = retry.maxBackoffMs ?? 2_000;

    const body = mapping.toWireRequest({ ...params, order: { ...params.order, receiver } });

    let delay = initialBackoffMs;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      try {
        return await this.doQuote(body, tokenOut, receiver);
      } catch (e) {
        if (e instanceof FyndError && e.isRetryable() && attempt < maxAttempts) {
          const jittered = delay * (0.75 + Math.random() * 0.5);
          await sleep(jittered);
          delay = Math.min(delay * 2, maxBackoffMs);
        } else {
          throw e;
        }
      }
    }
    // Unreachable but satisfies TypeScript exhaustiveness
    throw FyndError.config("retry loop exhausted without result");
  }

  private async doQuote(
    body: components["schemas"]["QuoteRequest"],
    tokenOut: Address,
    receiver: Address,
  ): Promise<Quote> {
    const timeoutMs = this.options.timeoutMs ?? 30_000;
    const { data, error } = await this.http.POST("/v1/quote", {
      body,
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (error !== undefined) {
      // openapi-fetch does not yet union error shapes per status code; cast is required here
      throw FyndError.fromWireError(error as WireErrorResponse);
    }
    if (data === undefined) {
      throw FyndError.config("server returned no data for successful response");
    }
    return mapping.fromWireQuote(data, tokenOut, receiver);
  }

  async health(): Promise<HealthStatus> {
    const { data, error } = await this.http.GET("/v1/health");
    if (error !== undefined) {
      throw FyndError.fromWireError(error as WireErrorResponse);
    }
    if (data === undefined) {
      throw FyndError.config("server returned no data for health response");
    }
    return mapping.fromWireHealth(data);
  }

  async signablePayload(quote: Quote, hints?: SigningHints): Promise<SignablePayload> {
    if (quote.backend !== 'fynd') {
      throw new Error('not implemented: Turbine backend signing');
    }
    return this.fyndSignablePayload(quote, hints ?? {});
  }

  private async fyndSignablePayload(quote: Quote, hints: SigningHints): Promise<SignablePayload> {
    const senderOpt = hints.sender ?? this.options.sender;
    if (senderOpt === undefined) {
      throw FyndError.config(
        "sender is required: set FyndClientOptions.sender or SigningHints.sender"
      );
    }
    const sender: Address = senderOpt;

    const provider = this.options.provider;
    if (provider === undefined) {
      throw FyndError.config("provider is required for signablePayload");
    }

    const nonce = hints.nonce !== undefined
      ? hints.nonce
      : await provider.getTransactionCount({ address: sender });

    const { maxFeePerGas, maxPriorityFeePerGas } =
      hints.maxFeePerGas !== undefined && hints.maxPriorityFeePerGas !== undefined
        ? { maxFeePerGas: hints.maxFeePerGas, maxPriorityFeePerGas: hints.maxPriorityFeePerGas }
        : await provider.estimateFeesPerGas();

    const gas = hints.gasLimit ?? quote.gasEstimate;

    const txData = quote.transaction;
    if (txData === undefined) {
      throw FyndError.config(
        "quote has no calldata; set encodingOptions in QuoteOptions"
      );
    }

    const tx: Eip1559Transaction = {
      chainId:              this.options.chainId,
      nonce,
      maxFeePerGas,
      maxPriorityFeePerGas,
      gas,
      to:    txData.to,
      value: txData.value,
      data:  txData.data,
    };

    if (hints.simulate === true) {
      await provider.call(tx).catch((err: unknown) => {
        throw FyndError.simulateFailed(`transaction simulation failed: ${String(err)}`);
      });
    }

    const payload: FyndPayload = { quote, tx };
    return { kind: 'fynd', payload };
  }

  async execute(order: SignedOrder, options?: ExecutionOptions): Promise<ExecutionReceipt> {
    const { payload, signature } = order;
    const tx = payload.payload.tx;
    const quote = payload.payload.quote;

    if (options?.dryRun === true) {
      return this.dryRunExecute(tx, quote);
    }

    const provider = this.options.submitProvider ?? this.options.provider;
    if (provider === undefined) {
      throw FyndError.config("provider is required for execute");
    }

    // Parse r, s, yParity from the 65-byte hex signature: r[32] + s[32] + v[1]
    // signature is '0x' + 130 hex chars (65 bytes)
    const r = `0x${signature.slice(2, 66)}` as Hex;
    const s = `0x${signature.slice(66, 130)}` as Hex;
    const vByte = parseInt(signature.slice(130, 132), 16);
    // Normalize: legacy v=27/28 → yParity 0/1; EIP-1559 v=0/1 pass through
    const vNormalized = vByte === 27 ? 0 : vByte === 28 ? 1 : vByte;
    if (vNormalized !== 0 && vNormalized !== 1) {
      throw FyndError.config(`invalid signature v byte: ${vByte}`);
    }
    const yParity: 0 | 1 = vNormalized;

    const rawTx = serializeTransaction(
      {
        type:                 'eip1559',
        chainId:              tx.chainId,
        nonce:                tx.nonce,
        maxFeePerGas:         tx.maxFeePerGas,
        maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
        gas:                  tx.gas,
        to:                   tx.to,
        value:                tx.value,
        data:                 tx.data,
      },
      { r, s, yParity },
    ) as Hex;

    const txHash = await provider.sendRawTransaction(rawTx);
    const tokenOut = quote.tokenOut;
    const receiver = quote.receiver;

    return {
      settle: async (): Promise<SettledOrder> => {
        for (;;) {
          const receipt = await provider.getTransactionReceipt({ hash: txHash });
          if (receipt !== null) {
            const settledAmount = computeSettledAmount(receipt, tokenOut, receiver);
            const gasCost = receipt.gasUsed * receipt.effectiveGasPrice;
            // exactOptionalPropertyTypes: spread optional fields only when defined
            return {
              txHash,
              gasCost,
              ...(settledAmount !== undefined ? { settledAmount } : {}),
            };
          }
          await sleep(2_000);
        }
      },
    };
  }

  private async dryRunExecute(tx: Eip1559Transaction, quote: Quote): Promise<ExecutionReceipt> {
    const provider = this.options.provider;
    if (provider === undefined) {
      throw FyndError.config("provider is required for dry-run execute");
    }

    const callResult = await provider.call(tx).catch((err: unknown) => {
      throw FyndError.simulateFailed(`dry run simulation failed: ${String(err)}`);
    });
    const gasUsed = await provider.estimateGas(tx).catch((err: unknown) => {
      throw FyndError.simulateFailed(`dry run gas estimation failed: ${String(err)}`);
    });

    // Parse first 32 bytes of return data as uint256 settled amount.
    // Hex string: '0x' + 64 hex chars = 66 chars total for 32 bytes.
    const returnData = callResult.data;
    const settledAmount =
      returnData !== undefined && returnData.length >= 66
        ? BigInt(`0x${returnData.slice(2, 66)}`)
        : undefined;

    const gasCost = gasUsed * tx.maxFeePerGas;

    // No txHash for dry-run
    void quote;

    return {
      settle: async (): Promise<SettledOrder> => ({
        gasCost,
        // exactOptionalPropertyTypes: spread optional fields only when defined
        ...(settledAmount !== undefined ? { settledAmount } : {}),
      }),
    };
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function computeSettledAmount(
  receipt: MinimalReceipt,
  tokenOut: Address,
  receiver: Address,
): bigint | undefined {
  let total = 0n;
  let found = false;

  for (const log of receipt.logs) {
    if (log.address.toLowerCase() !== tokenOut.toLowerCase()) continue;
    if (log.topics.length === 0) continue;

    const topic0 = log.topics[0];
    if (topic0 === undefined) continue;

    if (topic0 === ERC20_TRANSFER_TOPIC && log.topics.length >= 3) {
      // topics[2] = to address (padded to 32 bytes, address in last 20 bytes)
      const toTopic = log.topics[2];
      if (toTopic === undefined) continue;
      const toAddr = `0x${toTopic.slice(-40)}` as Address;
      if (toAddr.toLowerCase() === receiver.toLowerCase()) {
        // data = uint256 amount (32 bytes = 64 hex chars after '0x')
        const amount = BigInt(`0x${log.data.slice(2, 66)}`);
        total += amount;
        found = true;
      }
    } else if (topic0 === ERC6909_TRANSFER_TOPIC && log.topics.length >= 3) {
      // topics[2] = to address
      const toTopic = log.topics[2];
      if (toTopic === undefined) continue;
      const toAddr = `0x${toTopic.slice(-40)}` as Address;
      if (toAddr.toLowerCase() === receiver.toLowerCase()) {
        // data layout: caller[32 bytes] + amount[32 bytes]
        // amount starts at byte offset 32 → hex positions 2+64=66 to 66+64=130
        const amount = BigInt(`0x${log.data.slice(66, 130)}`);
        total += amount;
        found = true;
      }
    }
  }

  return found ? total : undefined;
}
