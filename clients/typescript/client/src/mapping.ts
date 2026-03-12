import type { components } from "@fynd/autogen";
import { FyndError } from "./error.js";
import type {
  Address,
  BlockInfo,
  EncodingOptions,
  HealthStatus,
  Quote,
  QuoteParams,
  Route,
  Swap,
  Transaction,
} from "./types.js";

type WireOrder           = components["schemas"]["Order"];
type WireSolution        = components["schemas"]["Quote"];
type WireSolutionRequest = components["schemas"]["QuoteRequest"];
type WireSolutionOptions = components["schemas"]["QuoteOptions"];
type WireHealthStatus    = components["schemas"]["HealthStatus"];
type WireRoute           = components["schemas"]["Route"];
type WireSwap            = components["schemas"]["Swap"];
type WireBlockInfo       = components["schemas"]["BlockInfo"];
type WireTransaction     = components["schemas"]["Transaction"];
type WireEncodingOptions = components["schemas"]["EncodingOptions"];
type WirePermitSingle    = components["schemas"]["PermitSingle"];
type WirePermitDetails   = components["schemas"]["PermitDetails"];


export function toWireRequest(params: QuoteParams): WireSolutionRequest {
  const wireOrder: WireOrder = {
    token_in:  params.order.tokenIn,
    token_out: params.order.tokenOut,
    amount:    params.order.amount.toString(),
    side:      'sell',
    sender:    params.order.sender,
    // exactOptionalPropertyTypes: must omit undefined keys entirely
    ...(params.order.receiver !== undefined ? { receiver: params.order.receiver } : {}),
  };
  const wireOptions: WireSolutionOptions | undefined = params.options !== undefined
    ? {
        ...(params.options.timeoutMs !== undefined
          ? { timeout_ms: params.options.timeoutMs }
          : {}),
        ...(params.options.minResponses !== undefined
          ? { min_responses: params.options.minResponses }
          : {}),
        ...(params.options.maxGas !== undefined
          ? { max_gas: params.options.maxGas.toString() }
          : {}),
        ...(params.options.encodingOptions !== undefined
          ? { encoding_options: toWireEncodingOptions(params.options.encodingOptions) }
          : {}),
      }
    : undefined;
  return {
    orders: [wireOrder],
    ...(wireOptions !== undefined ? { options: wireOptions } : {}),
  };
}

export function fromWireQuote(
  wire: WireSolution,
  tokenOut: Address,
  receiver: Address,
): Quote {
  const orderSolution = wire.orders[0];
  if (orderSolution === undefined) {
    throw FyndError.config("server returned empty orders array");
  }
  const route = orderSolution.route !== null && orderSolution.route !== undefined
    ? fromWireRoute(orderSolution.route)
    : undefined;
  const priceImpactBps = orderSolution.price_impact_bps ?? undefined;
  const transaction = orderSolution.transaction != null
    ? fromWireTransaction(orderSolution.transaction)
    : undefined;
  return {
    orderId:         orderSolution.order_id,
    status:          orderSolution.status,
    backend:         'fynd',  // WireOrderQuote has no backend field; hardcoded per design
    amountIn:        BigInt(orderSolution.amount_in),
    amountOut:       BigInt(orderSolution.amount_out),
    gasEstimate:     BigInt(orderSolution.gas_estimate),
    block:           fromWireBlockInfo(orderSolution.block),
    tokenOut,
    receiver,
    // exactOptionalPropertyTypes: spread optional fields only when defined
    ...(route !== undefined ? { route } : {}),
    ...(priceImpactBps !== undefined ? { priceImpactBps } : {}),
    ...(transaction !== undefined ? { transaction } : {}),
  };
}

function fromWireRoute(wire: WireRoute): Route {
  return { swaps: wire.swaps.map(fromWireSwap) };
}

function fromWireSwap(wire: WireSwap): Swap {
  return {
    poolId:      wire.component_id,
    protocol:    wire.protocol,
    tokenIn:     wire.token_in as Address,
    tokenOut:    wire.token_out as Address,
    amountIn:    BigInt(wire.amount_in),
    amountOut:   BigInt(wire.amount_out),
    gasEstimate: BigInt(wire.gas_estimate),
  };
}

function fromWireBlockInfo(wire: WireBlockInfo): BlockInfo {
  return {
    number:    wire.number,
    hash:      wire.hash,
    timestamp: wire.timestamp,
  };
}

function fromWireTransaction(wire: WireTransaction): Transaction {
  return {
    to:    wire.to as Address,
    value: BigInt(wire.value),
    data:  wire.data as `0x${string}`,
  };
}

function toWireEncodingOptions(opts: EncodingOptions): WireEncodingOptions {
  const permit: WirePermitSingle | undefined = opts.permit !== undefined
    ? toWirePermitSingle(opts.permit)
    : undefined;
  return {
    slippage: opts.slippage,
    ...(opts.transferType !== undefined ? { transfer_type: opts.transferType } : {}),
    ...(permit !== undefined ? { permit } : {}),
    ...(opts.permit2Signature !== undefined ? { permit2_signature: opts.permit2Signature } : {}),
  };
}

function toWirePermitSingle(permit: import("./types.js").PermitSingle): WirePermitSingle {
  const details: WirePermitDetails = {
    token:      permit.details.token,
    amount:     permit.details.amount.toString(),
    expiration: permit.details.expiration.toString(),
    nonce:      permit.details.nonce.toString(),
  };
  return {
    details,
    spender:      permit.spender,
    sig_deadline: permit.sigDeadline.toString(),
  };
}

export function fromWireHealth(wire: WireHealthStatus): HealthStatus {
  return {
    healthy:        wire.healthy,
    lastUpdateMs:   wire.last_update_ms,
    numSolverPools: wire.num_solver_pools,
  };
}
