import { decodeAbiParameters, encodeFunctionData, keccak256, serializeTransaction, toHex } from 'viem';
import { createFyndClient, type FyndClient as AutogenClient } from "@fynd/autogen";
import type { components } from "@fynd/autogen";
import { FyndError } from "./error.js";
import * as mapping from "./mapping.js";
import {
  DEFAULT_SETTLE_TIMEOUT_MS,
  type ApprovalPayload,
  type Eip1559Transaction,
  type ExecutionReceipt,
  type FyndPayload,
  type SettledOrder,
  type SettleOptions,
  type SignedApproval,
  type SignedSwap,
  type SwapPayload,
  type TxReceipt,
} from "./signing.js";
import type { Address, ApprovalParams, Hex, HealthStatus, InstanceInfo, Quote, QuoteParams } from "./types.js";

type WireErrorResponse = components["schemas"]["ErrorResponse"];

const ERC20_APPROVE_ABI = [{
  name: 'approve', type: 'function' as const,
  inputs: [{ name: 'spender', type: 'address' }, { name: 'amount', type: 'uint256' }],
  outputs: [{ type: 'bool' }],
  stateMutability: 'nonpayable' as const,
}] as const;

// ERC-20 Transfer(address,address,uint256)
const ERC20_TRANSFER_TOPIC = keccak256(toHex('Transfer(address,address,uint256)'));
// ERC-6909 Transfer(address,address,address,uint256,uint256)
const ERC6909_TRANSFER_TOPIC = keccak256(toHex('Transfer(address,address,address,uint256,uint256)'));

/** Minimal transaction receipt, compatible with viem and ethers receipts. */
export interface MinimalReceipt {
  transactionHash: Hex;
  /** 1 = success, 0 = reverted. */
  status: number;
  gasUsed: bigint;
  effectiveGasPrice: bigint;
  logs: Array<{ address: Address; topics: readonly Hex[]; data: Hex }>;
}

/**
 * Blockchain provider interface used by {@link FyndClient} for signing and execution.
 *
 * Use {@link viemProvider} to create one from a viem `PublicClient`.
 */
export interface EthProvider {
  getTransactionCount(args: { address: Address }): Promise<number>;
  estimateFeesPerGas(): Promise<{ maxFeePerGas: bigint; maxPriorityFeePerGas: bigint }>;
  call(tx: Eip1559Transaction): Promise<{ data?: Hex }>;
  estimateGas(tx: Eip1559Transaction): Promise<bigint>;
  sendRawTransaction(rawTx: Hex): Promise<Hex>;
  getTransactionReceipt(args: { hash: Hex }): Promise<MinimalReceipt | null>;
  readAllowance?(token: Address, owner: Address, spender: Address): Promise<bigint>;
  /** Replay a mined transaction in its original block context to extract the revert reason. */
  debugTraceTransaction?(hash: Hex): Promise<{ output: Hex }>;
}

/** Configuration for exponential backoff retry on transient quote errors. */
export interface RetryConfig {
  /** Maximum number of attempts (default: 3). */
  maxAttempts?: number;
  /** Initial backoff delay in milliseconds (default: 100). */
  initialBackoffMs?: number;
  /** Maximum backoff delay in milliseconds (default: 2000). */
  maxBackoffMs?: number;
}

/** Overrides for transaction parameters when building a swap or approval payload. */
export interface SigningHints {
  /** Override the sender address (defaults to {@link FyndClientOptions.sender}). */
  sender?: Address;
  /** Override the nonce (defaults to on-chain pending nonce). */
  nonce?: number;
  /** Override `maxFeePerGas` (defaults to provider estimate). */
  maxFeePerGas?: bigint;
  /** Override `maxPriorityFeePerGas` (defaults to provider estimate). */
  maxPriorityFeePerGas?: bigint;
  /** Override gas limit (defaults to `eth_estimateGas`). */
  gasLimit?: bigint;
  /** When `true`, simulate the transaction via `eth_call` before returning. */
  simulate?: boolean;
}

/** Options for {@link FyndClient.executeSwap}. */
export interface ExecutionOptions {
  /** When `true`, simulate execution without broadcasting a transaction. */
  dryRun?: boolean;
}

/** Configuration for constructing a {@link FyndClient}. */
export interface FyndClientOptions {
  /** Base URL of the Fynd API (e.g. `"https://api.fynd.exchange"`). */
  baseUrl: string;
  /** EVM chain ID for transaction signing. */
  chainId: number;
  /** Default sender address, used when {@link SigningHints.sender} is not set. */
  sender?: Address;
  /** HTTP request timeout in milliseconds (default: 30000). */
  timeoutMs?: number;
  retry?: RetryConfig;
  /** Provider for reading chain state and simulating transactions. */
  provider?: EthProvider;
  /** Separate provider for broadcasting transactions; falls back to `provider`. */
  submitProvider?: EthProvider;
  /**
   * When `true` (default), fetch the revert reason when a transaction reverts.
   * Tries `debug_traceTransaction` first; falls back to `eth_call` with a warning.
   * Set to `false` to skip the extra RPC call and throw a generic revert error.
   */
  fetchRevertReason?: boolean;
}

/**
 * Client for the Fynd swap routing API.
 *
 * Provides methods to request quotes, build signable payloads, and execute
 * signed swap transactions on-chain.
 *
 * Requires `provider` for building signable payloads and executing transactions.
 * Optionally accepts a separate `submitProvider` for broadcasting (falls back to `provider`).
 */
export class FyndClient {
  private readonly http: AutogenClient;
  private readonly options: FyndClientOptions;
  private infoPromise: Promise<InstanceInfo> | undefined = undefined;

  constructor(options: FyndClientOptions) {
    this.http = createFyndClient(options.baseUrl);
    this.options = options;
  }

  /**
   * Requests a swap quote from the solver.
   *
   * Retries automatically on transient server errors (`TIMEOUT`, `QUEUE_FULL`,
   * `SERVICE_OVERLOADED`, `STALE_DATA`, `NOT_READY`) using exponential backoff
   * with jitter. Configure retry behavior via {@link FyndClientOptions.retry}
   * (defaults: 3 attempts, 100ms initial backoff, 2s max backoff).
   *
   * Each request is subject to an HTTP timeout controlled by
   * {@link FyndClientOptions.timeoutMs} (default: 30s).
   *
   * @throws {FyndError} With a server error code (`NO_ROUTE_FOUND`, `INSUFFICIENT_LIQUIDITY`, etc.) on non-retryable failures.
   * @throws {FyndError} With code `HTTP` on network-level failures.
   */
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

  /**
   * Returns the solver's health status.
   *
   * Unlike {@link quote}, this method does not retry on transient errors.
   *
   * @throws {FyndError} With a server error code if the solver reports unhealthy.
   * @throws {FyndError} With code `HTTP` on network-level failures.
   */
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

  /**
   * Fetches and caches static instance metadata from `GET /v1/info`.
   *
   * The result is cached for the lifetime of the client. On failure, the cache
   * is cleared so the next call retries.
   *
   * @throws {FyndError} With a server error code on non-OK responses.
   * @throws {FyndError} With code `HTTP` on network-level failures.
   */
  async info(): Promise<InstanceInfo> {
    this.infoPromise ??= this.fetchInfo().catch((err: unknown) => {
      this.infoPromise = undefined;
      throw err;
    });
    return this.infoPromise;
  }

  private async fetchInfo(): Promise<InstanceInfo> {
    const timeoutMs = this.options.timeoutMs ?? 30_000;
    const { data, error } = await this.http.GET("/v1/info", {
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (error !== undefined) {
      throw FyndError.fromWireError(error as WireErrorResponse);
    }
    if (data === undefined) {
      throw FyndError.config("server returned no data for /v1/info");
    }
    return mapping.fromWireInstanceInfo(data);
  }

  /**
   * Builds an unsigned EIP-1559 transaction payload from a quote, ready for wallet signing.
   *
   * Fetches the sender's nonce, current gas fees, and gas estimate from the provider
   * unless overridden via {@link SigningHints}.
   *
   * The quote must include a `transaction` field (returned when `encodingOptions`
   * is set in the quote request). The `to`, `value`, and `data` fields are read
   * from `quote.transaction`.
   *
   * When `hints.simulate` is `true`, the transaction is executed via `eth_call`
   * before returning. This catches reverts early but adds one RPC round-trip.
   *
   * @param quote - A quote obtained from {@link quote}. Must have `transaction` populated.
   * @param hints - Optional overrides for nonce, gas fees, gas limit (defaults to `eth_estimateGas`), sender, and simulation.
   * @throws {FyndError} With code `CONFIG` if `provider` or `sender` is not configured.
   * @throws {FyndError} With code `CONFIG` if `quote.transaction` is absent (forgot `encodingOptions`).
   * @throws {FyndError} With code `SIMULATE_FAILED` if `hints.simulate` is `true` and the `eth_call` reverts.
   */
  async swapPayload(quote: Quote, hints?: SigningHints): Promise<SwapPayload> {
    if (quote.backend !== 'fynd') {
      throw new Error('not implemented: Turbine backend signing');
    }
    return this.fyndSwapPayload(quote, hints ?? {});
  }

  private async fyndSwapPayload(quote: Quote, hints: SigningHints): Promise<SwapPayload> {
    const senderOpt = hints.sender ?? this.options.sender;
    if (senderOpt === undefined) {
      throw FyndError.config(
        "sender is required: set FyndClientOptions.sender or SigningHints.sender"
      );
    }
    const sender: Address = senderOpt;

    const provider = this.options.provider;
    if (provider === undefined) {
      throw FyndError.config("provider is required for swapPayload");
    }

    const nonce = hints.nonce !== undefined
      ? hints.nonce
      : await provider.getTransactionCount({ address: sender });

    const { maxFeePerGas, maxPriorityFeePerGas } =
      hints.maxFeePerGas !== undefined && hints.maxPriorityFeePerGas !== undefined
        ? { maxFeePerGas: hints.maxFeePerGas, maxPriorityFeePerGas: hints.maxPriorityFeePerGas }
        : await provider.estimateFeesPerGas();

    const txData = quote.transaction;
    if (txData === undefined) {
      throw FyndError.config(
        "quote has no calldata; set encodingOptions in QuoteOptions"
      );
    }

    const txBase = {
      chainId:              this.options.chainId,
      nonce,
      maxFeePerGas,
      maxPriorityFeePerGas,
      to:    txData.to,
      value: txData.value,
      data:  txData.data,
    };

    const gas = hints.gasLimit !== undefined
      ? hints.gasLimit
      : await provider.estimateGas({ ...txBase, gas: 0n });

    const tx: Eip1559Transaction = { ...txBase, gas };

    if (hints.simulate === true) {
      await provider.call(tx).catch((err: unknown) => {
        throw FyndError.simulateFailed(`transaction simulation failed: ${String(err)}`);
      });
    }

    const payload: FyndPayload = { quote, tx };
    return { kind: 'fynd', payload };
  }

  /**
   * Broadcasts a signed order on-chain and returns a handle to await settlement.
   *
   * Uses `submitProvider` if configured, otherwise falls back to `provider`.
   * The signed transaction is serialized as an EIP-1559 envelope and sent via
   * `eth_sendRawTransaction`.
   *
   * Call {@link ExecutionReceipt.settle} on the returned handle to poll for
   * the transaction receipt. Settlement polling has a default timeout of
   * {@link DEFAULT_SETTLE_TIMEOUT_MS} (120s), configurable via {@link SettleOptions.timeoutMs}.
   * The settled result includes `gasCost` (gasUsed * effectiveGasPrice) and
   * `settledAmount` (parsed from ERC-20/ERC-6909 Transfer logs to the receiver).
   *
   * When `dryRun` is `true`, the transaction is simulated via `eth_call` and
   * `eth_estimateGas` without broadcasting. The returned `settle()` resolves
   * immediately with estimated gas cost and decoded return data.
   *
   * @param order - A signed swap from {@link assembleSignedSwap}.
   * @param options - Set `dryRun: true` to simulate without broadcasting.
   * @throws {FyndError} With code `CONFIG` if no provider is configured.
   * @throws {FyndError} With code `CONFIG` if the signature has an invalid v byte.
   * @throws {FyndError} With code `SIMULATE_FAILED` when `dryRun` is `true` and the simulation reverts.
   * @throws {FyndError} With code `EXECUTION_REVERTED` when the mined transaction reverts.
   */
  async executeSwap(order: SignedSwap, options?: ExecutionOptions): Promise<ExecutionReceipt> {
    const { payload, signature } = order;
    const tx = payload.payload.tx;
    const quote = payload.payload.quote;

    if (options?.dryRun === true) {
      return this.dryRunExecute(tx, quote);
    }

    const provider = this.options.submitProvider ?? this.options.provider;
    if (provider === undefined) {
      throw FyndError.config("provider is required for executeSwap");
    }

    const txHash = await this.serializeAndBroadcast(tx, signature);
    const tokenOut = quote.tokenOut;
    const receiver = quote.receiver;

    return {
      settle: async (options?: SettleOptions): Promise<SettledOrder> => {
        const timeoutMs = options?.timeoutMs ?? DEFAULT_SETTLE_TIMEOUT_MS;
        const receipt = await this.pollForReceipt(provider, txHash, timeoutMs);
        if (receipt.status === 0) {
          const reason = this.options.fetchRevertReason !== false
            ? await this.getRevertReason(provider, tx, txHash)
            : 'revert reason fetching disabled';
          throw FyndError.executionReverted(`swap reverted: ${reason}`);
        }
        const settledAmount = computeSettledAmount(receipt, tokenOut, receiver);
        const gasCost = receipt.gasUsed * receipt.effectiveGasPrice;
        // exactOptionalPropertyTypes: spread optional fields only when defined
        return {
          txHash,
          gasCost,
          ...(settledAmount !== undefined ? { settledAmount } : {}),
        };
      },
    };
  }

  private async serializeAndBroadcast(tx: Eip1559Transaction, signature: Hex): Promise<Hex> {
    const provider = this.options.submitProvider ?? this.options.provider;
    if (provider === undefined) {
      throw FyndError.config("provider is required for broadcast");
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

    return provider.sendRawTransaction(rawTx);
  }

  private async getRevertReason(
    provider: EthProvider,
    tx: Eip1559Transaction,
    txHash: Hex,
  ): Promise<string> {
    if (provider.debugTraceTransaction !== undefined) {
      try {
        const trace = await provider.debugTraceTransaction(txHash);
        return decodeRevertData(trace.output);
      } catch { /* fall through to eth_call */ }
    }
    console.warn(
      '[fynd] debug_traceTransaction unavailable; replaying via eth_call — ' +
      'revert reason may differ if block state has changed since execution',
    );
    return provider.call(tx).then(
      () => 'transaction reverted with no revert data',
      (err: unknown) => String(err),
    );
  }

  private async pollForReceipt(
    provider: EthProvider,
    txHash: Hex,
    timeoutMs: number,
  ): Promise<MinimalReceipt> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const receipt = await provider.getTransactionReceipt({ hash: txHash });
      if (receipt !== null) {
        return receipt;
      }
      if (Date.now() >= deadline) {
        throw FyndError.timeout(
          `transaction ${txHash} did not settle within ${timeoutMs}ms`,
        );
      }
      await sleep(2_000);
    }
  }

  /**
   * Builds an unsigned EIP-1559 `approve(spender, amount)` transaction for the given token,
   * or `null` if the approval is not needed.
   *
   * Returns `null` immediately when `params.transferType` is `'none'`.
   * When `params.checkAllowance` is `true`, reads the on-chain allowance first and returns
   * `null` if it is already sufficient (skipping nonce and fee resolution).
   *
   * Fetches the spender address from `GET /v1/info` (cached after first call):
   * `'transfer_from'` → router, `'transfer_from_permit2'` → Permit2.
   * Reads nonce and gas fees from `provider` unless overridden via `hints`.
   * Gas defaults to `hints.gasLimit ?? 65_000n`.
   *
   * @param params - Token, amount, transfer type, and optional allowance-check flag.
   * @param hints - Optional overrides for sender, nonce, gas fees, and gas limit.
   * @throws {FyndError} With code `CONFIG` if `provider` or `sender` is not configured.
   * @throws {FyndError} With code `CONFIG` if `params.checkAllowance` is `true` and `provider.readAllowance` is absent.
   */
  async approval(params: ApprovalParams, hints?: SigningHints): Promise<ApprovalPayload | null> {
    if (params.transferType === 'none') return null;

    const info = await this.info();
    const spender = params.transferType === 'transfer_from_permit2'
      ? info.permit2Address
      : info.routerAddress;

    const provider = this.options.provider;
    if (provider === undefined) throw FyndError.config("provider is required for approval");

    const senderOpt = hints?.sender ?? this.options.sender;
    if (senderOpt === undefined) throw FyndError.config("sender is required for approval");
    const sender: Address = senderOpt;

    const { token, amount } = params;

    if (params.checkAllowance === true) {
      if (provider.readAllowance === undefined) {
        throw FyndError.config("provider.readAllowance is required when checkAllowance is true");
      }
      const current = await provider.readAllowance(token, sender, spender);
      if (current >= amount) return null;
    }

    const nonce = hints?.nonce !== undefined
      ? hints.nonce
      : await provider.getTransactionCount({ address: sender });

    const { maxFeePerGas, maxPriorityFeePerGas } =
      hints?.maxFeePerGas !== undefined && hints?.maxPriorityFeePerGas !== undefined
        ? { maxFeePerGas: hints.maxFeePerGas, maxPriorityFeePerGas: hints.maxPriorityFeePerGas }
        : await provider.estimateFeesPerGas();

    const gas = hints?.gasLimit ?? 65_000n;

    const data = encodeFunctionData({
      abi: ERC20_APPROVE_ABI,
      functionName: 'approve',
      args: [spender, amount],
    }) as Hex;

    const tx: Eip1559Transaction = {
      chainId: this.options.chainId,
      nonce,
      maxFeePerGas,
      maxPriorityFeePerGas,
      gas,
      to: token,
      value: 0n,
      data,
    };

    return { tx, token, spender, amount };
  }

  /**
   * Broadcasts a signed ERC-20 approval and polls for inclusion.
   *
   * @param signedApproval - Signed approval from {@link approval} + wallet signature.
   * @param options - Optional poll timeout (defaults to {@link DEFAULT_SETTLE_TIMEOUT_MS}).
   * @throws {FyndError} With code `CONFIG` if no provider is configured.
   * @throws {FyndError} With code `SETTLE_TIMEOUT` if the transaction does not confirm in time.
   * @throws {FyndError} With code `EXECUTION_REVERTED` when the mined transaction reverts.
   */
  async executeApproval(signedApproval: SignedApproval, options?: SettleOptions): Promise<TxReceipt> {
    const provider = this.options.submitProvider ?? this.options.provider;
    if (provider === undefined) throw FyndError.config("provider is required for executeApproval");
    const txHash = await this.serializeAndBroadcast(signedApproval.tx, signedApproval.signature);
    const receipt = await this.pollForReceipt(
      provider, txHash, options?.timeoutMs ?? DEFAULT_SETTLE_TIMEOUT_MS,
    );
    if (receipt.status === 0) {
      const reason = this.options.fetchRevertReason !== false
        ? await this.getRevertReason(provider, signedApproval.tx, txHash)
        : 'revert reason fetching disabled';
      throw FyndError.executionReverted(`approval reverted: ${reason}`);
    }
    return { txHash, gasCost: receipt.gasUsed * receipt.effectiveGasPrice };
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

const ERROR_SELECTOR = '0x08c379a0';
const PANIC_SELECTOR  = '0x4e487b71';

/** Decode a Solidity revert payload into a human-readable string. */
function decodeRevertData(output: Hex): string {
  if (output === '0x') return 'empty revert data';
  if (output.startsWith(ERROR_SELECTOR) && output.length > 10) {
    try {
      const [msg] = decodeAbiParameters([{ type: 'string' }], `0x${output.slice(10)}` as Hex);
      return msg;
    } catch { /* fall through */ }
  }
  if (output.startsWith(PANIC_SELECTOR) && output.length > 10) {
    try {
      const [code] = decodeAbiParameters([{ type: 'uint256' }], `0x${output.slice(10)}` as Hex);
      return `Panic(${code})`;
    } catch { /* fall through */ }
  }
  return `revert data: ${output}`;
}
