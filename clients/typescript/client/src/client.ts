import { FyndClientError } from "./error.js";
import { withRetry, type RetryOptions, DEFAULT_RETRY_OPTIONS } from "./retry.js";
import { TransactionHandle, type ExecutionReceipt } from "./execution.js";
import { assembleSignedOrder, type SignablePayload, type SignedOrder } from "./signing.js";
import type { Order, OrderSolution } from "./types.js";

export class FyndClient {
  private readonly baseUrl: string;
  private readonly retryOptions: RetryOptions;

  constructor(
    baseUrl: string,
    options: { retry?: Partial<RetryOptions> } = {},
  ) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.retryOptions = { ...DEFAULT_RETRY_OPTIONS, ...options.retry };
  }

  /**
   * Phase 1: Get a priced route from the solver.
   *
   * The returned quote is valid for the block it was computed in.
   * Re-quote if submission is delayed by more than one or two blocks.
   */
  async quote(order: Order): Promise<OrderSolution> {
    return withRetry(() => this.doQuote(order), this.retryOptions);
  }

  private async doQuote(order: Order): Promise<OrderSolution> {
    const response = await fetch(`${this.baseUrl}/v1/solve`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        orders: [
          {
            token_in: order.tokenIn,
            token_out: order.tokenOut,
            amount: order.amount.toString(),
            side: order.side,
            sender: order.sender,
            ...(order.receiver !== undefined && { receiver: order.receiver }),
          },
        ],
      }),
    });

    if (!response.ok) {
      const body = await response.text().catch(() => "");
      const retryable = response.status >= 500 || response.status === 429;
      throw new FyndClientError(
        `solver returned ${response.status}: ${body}`,
        retryable,
        response.status,
      );
    }

    const data = (await response.json()) as {
      orders: Array<{
        order_id: string;
        status: string;
        amount_in: string;
        amount_out: string;
        gas_estimate: string;
        price_impact_bps?: number;
        block: { number: number; hash: string; timestamp: number };
        route?: {
          swaps: Array<{
            component_id: string;
            protocol: string;
            token_in: string;
            token_out: string;
            amount_in: string;
            amount_out: string;
            gas_estimate: string;
          }>;
        };
      }>;
    };

    const orderData = data.orders[0];
    if (orderData === undefined) {
      throw new FyndClientError("empty orders in response", false);
    }

    switch (orderData.status) {
      case "success":
        break;
      case "no_route_found":
        throw new FyndClientError(
          `no route found for order ${orderData.order_id}`,
          false,
        );
      case "insufficient_liquidity":
        throw new FyndClientError(
          `insufficient liquidity for order ${orderData.order_id}`,
          false,
        );
      case "timeout":
        throw new FyndClientError(
          `solver timeout for order ${orderData.order_id}`,
          true,
        );
      case "not_ready":
        throw new FyndClientError("solver not ready", true);
      default:
        throw new FyndClientError(
          `unexpected status: ${orderData.status}`,
          false,
        );
    }

    return {
      orderId: orderData.order_id,
      amountIn: BigInt(orderData.amount_in),
      amountOut: BigInt(orderData.amount_out),
      gasEstimate: BigInt(orderData.gas_estimate),
      ...(orderData.price_impact_bps !== undefined && {
        priceImpactBps: orderData.price_impact_bps,
      }),
      block: orderData.block,
      ...(orderData.route !== undefined && {
        route: {
          swaps: orderData.route.swaps.map((s) => ({
            componentId: s.component_id,
            protocol: s.protocol,
            tokenIn: s.token_in,
            tokenOut: s.token_out,
            amountIn: BigInt(s.amount_in),
            amountOut: BigInt(s.amount_out),
            gasEstimate: BigInt(s.gas_estimate),
          })),
        },
      }),
    };
  }

  /**
   * Phase 2: Build the signable payload for a quote.
   *
   * Fetches current nonce and gas fees from the chain via `rpcUrl`.
   * Returns a `SignablePayload` — call `payload.signingHash` to get the hash
   * to sign externally, then pass the signature to `assembleSignedOrder()`.
   *
   * Note: Full EIP-1559 transaction construction requires RPC calls.
   * This implementation returns the parameters needed for the caller's signer.
   */
  async signablePayload(
    solution: OrderSolution,
    sender: string,
    rpcUrl: string,
  ): Promise<SignablePayload> {
    const [nonce, feeData] = await Promise.all([
      this.rpcCall<string>(rpcUrl, "eth_getTransactionCount", [sender, "latest"]),
      this.rpcCall<{ maxFeePerGas: string; maxPriorityFeePerGas: string }>(
        rpcUrl,
        "eth_feeHistory",
        [1, "latest", [50]],
      ).then(async () => {
        // Get base fee from latest block
        const block = await this.rpcCall<{ baseFeePerGas: string }>(
          rpcUrl,
          "eth_getBlockByNumber",
          ["latest", false],
        );
        const baseFee = BigInt(block.baseFeePerGas);
        const priorityFee = 1_500_000_000n; // 1.5 gwei tip
        return {
          maxFeePerGas: (baseFee * 2n + priorityFee).toString(16),
          maxPriorityFeePerGas: priorityFee.toString(16),
        };
      }),
    ]);

    const chainId = await this.rpcCall<string>(rpcUrl, "eth_chainId", []);

    // calldata is empty until the server populates it (future update)
    const rawTx = {
      to: "0x0000000000000000000000000000000000000000",
      data: "0x",
      value: "0x0",
      gasLimit: solution.gasEstimate + 50_000n,
      maxFeePerGas: BigInt("0x" + feeData.maxFeePerGas),
      maxPriorityFeePerGas: BigInt("0x" + feeData.maxPriorityFeePerGas),
      nonce: parseInt(nonce, 16),
      chainId: parseInt(chainId, 16),
    };

    // signingHash is a placeholder — full EIP-1559 hash requires RLP encoding
    // which is delegated to the caller's signing library (ethers.js, viem, etc.)
    return {
      backend: "fynd",
      signingHash:
        "0x0000000000000000000000000000000000000000000000000000000000000000",
      rawTx,
    };
  }

  /**
   * Phase 3: Broadcast a signed order to the blockchain.
   *
   * Sends the signed raw transaction via `eth_sendRawTransaction` and returns
   * a handle that resolves to a `SettledOrder` once the transaction is confirmed.
   */
  async execute(
    signedOrder: SignedOrder,
    tokenOut: string,
    receiver: string,
    rpcUrl: string,
  ): Promise<ExecutionReceipt> {
    if (signedOrder.backend === "turbine") {
      throw new FyndClientError("Turbine not yet implemented", false);
    }

    const txHash = await withRetry(
      () =>
        this.rpcCall<string>(rpcUrl, "eth_sendRawTransaction", [
          signedOrder.rawTx,
        ]),
      this.retryOptions,
    );

    return {
      type: "transaction",
      handle: new TransactionHandle(txHash, tokenOut, receiver),
    };
  }

  private async rpcCall<T>(
    rpcUrl: string,
    method: string,
    params: unknown[],
  ): Promise<T> {
    const response = await fetch(rpcUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
    });

    if (!response.ok) {
      throw new FyndClientError(
        `RPC error ${response.status}`,
        response.status >= 500,
        response.status,
      );
    }

    const json = (await response.json()) as {
      result: T;
      error?: { message: string };
    };
    if (json.error) {
      throw new FyndClientError(`RPC error: ${json.error.message}`, false);
    }
    return json.result;
  }
}

// Re-export assembleSignedOrder for convenience (used in the three-phase API)
export { assembleSignedOrder };
