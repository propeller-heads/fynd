import type {components} from "@fynd/autogen";
import {FyndError} from "./error.js";
import type {
    Address,
    BlockInfo,
    ClientFeeParams,
    EncodingOptions,
    HealthStatus,
    Hex,
    PermitDetails,
    PermitSingle,
    Quote,
    QuoteParams,
    Route,
    Swap,
    Transaction,
} from "./types.js";

type WireOrder = components["schemas"]["Order"];
type WireSolution = components["schemas"]["Quote"];
type WireSolutionRequest = components["schemas"]["QuoteRequest"];
type WireSolutionOptions = components["schemas"]["QuoteOptions"];
type WireHealthStatus = components["schemas"]["HealthStatus"];
type WireRoute = components["schemas"]["Route"];
type WireSwap = components["schemas"]["Swap"];
type WireBlockInfo = components["schemas"]["BlockInfo"];
type WireEncodingOptions = components["schemas"]["EncodingOptions"];
type WireTransaction = components["schemas"]["Transaction"];
type WirePermitSingle = components["schemas"]["PermitSingle"];
type WirePermitDetails = components["schemas"]["PermitDetails"];
type WireClientFeeParams = components["schemas"]["ClientFeeParams"];


export function toWireRequest(params: QuoteParams): WireSolutionRequest {
    const wireOrder: WireOrder = {
        token_in: params.order.tokenIn,
        token_out: params.order.tokenOut,
        amount: params.order.amount.toString(),
        side: 'sell',
        sender: params.order.sender,
        // exactOptionalPropertyTypes: must omit undefined keys entirely
        ...(params.order.receiver !== undefined ? {receiver: params.order.receiver} : {}),
    };
    const wireOptions: WireSolutionOptions | undefined = params.options !== undefined
        ? {
            ...(params.options.timeoutMs !== undefined
                ? {timeout_ms: params.options.timeoutMs}
                : {}),
            ...(params.options.minResponses !== undefined
                ? {min_responses: params.options.minResponses}
                : {}),
            ...(params.options.maxGas !== undefined
                ? {max_gas: params.options.maxGas.toString()}
                : {}),
            ...(params.options.encodingOptions !== undefined
                ? {encoding_options: toWireEncodingOptions(params.options.encodingOptions)}
                : {}),
        }
        : undefined;
    return {
        orders: [wireOrder],
        ...(wireOptions !== undefined ? {options: wireOptions} : {}),
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
    const transaction = orderSolution.transaction !== null
    && orderSolution.transaction !== undefined
        ? fromWireTransaction(orderSolution.transaction)
        : undefined;
    const priceImpactBps = orderSolution.price_impact_bps ?? undefined;
    return {
        orderId: orderSolution.order_id,
        status: orderSolution.status,
        backend: 'fynd',  // WireOrderQuote has no backend field; hardcoded per design
        amountIn: BigInt(orderSolution.amount_in),
        amountOut: BigInt(orderSolution.amount_out),
        gasEstimate: BigInt(orderSolution.gas_estimate),
        block: fromWireBlockInfo(orderSolution.block),
        tokenOut,
        receiver,
        // exactOptionalPropertyTypes: spread optional fields only when defined
        ...(route !== undefined ? {route} : {}),
        ...(transaction !== undefined ? {transaction} : {}),
        ...(priceImpactBps !== undefined ? {priceImpactBps} : {}),
    };
}

function fromWireRoute(wire: WireRoute): Route {
    return {swaps: wire.swaps.map(fromWireSwap)};
}

function fromWireSwap(wire: WireSwap): Swap {
    return {
        poolId: wire.component_id,
        protocol: wire.protocol,
        tokenIn: wire.token_in as Address,
        tokenOut: wire.token_out as Address,
        amountIn: BigInt(wire.amount_in),
        amountOut: BigInt(wire.amount_out),
        gasEstimate: BigInt(wire.gas_estimate),
    };
}

function fromWireBlockInfo(wire: WireBlockInfo): BlockInfo {
    return {
        number: wire.number,
        hash: wire.hash,
        timestamp: wire.timestamp,
    };
}

function toWireEncodingOptions(opts: EncodingOptions): WireEncodingOptions {
    return {
        // Server deserializes slippage as a string despite OpenAPI declaring number.
        slippage: opts.slippage.toString() as unknown as number,
        ...(opts.transferType !== undefined ? {transfer_type: opts.transferType} : {}),
        ...(opts.permit !== undefined ? {permit: toWirePermitSingle(opts.permit)} : {}),
        ...(opts.permit2Signature !== undefined
            ? {permit2_signature: opts.permit2Signature}
            : {}),
        ...(opts.clientFeeParams !== undefined
            ? {client_fee_params: toWireClientFeeParams(opts.clientFeeParams)}
            : {}),
    };
}

function toWireClientFeeParams(p: ClientFeeParams): WireClientFeeParams {
    return {
        bps: p.bps,
        receiver: p.receiver,
        max_contribution: p.maxContribution.toString(),
        deadline: p.deadline.toString(),
        signature: p.signature,
    };
}

function toWirePermitSingle(permit: PermitSingle): WirePermitSingle {
    return {
        details: toWirePermitDetails(permit.details),
        spender: permit.spender,
        sig_deadline: permit.sigDeadline.toString(),
    };
}

function toWirePermitDetails(d: PermitDetails): WirePermitDetails {
    return {
        token: d.token,
        amount: d.amount.toString(),
        expiration: d.expiration.toString(),
        nonce: d.nonce.toString(),
    };
}

function fromWireTransaction(wire: WireTransaction): Transaction {
    return {
        to: wire.to as Address,
        value: BigInt(wire.value),
        data: wire.data as Hex,
    };
}

export function fromWireHealth(wire: WireHealthStatus): HealthStatus {
    return {
        healthy: wire.healthy,
        lastUpdateMs: wire.last_update_ms,
        numSolverPools: wire.num_solver_pools,
        gasPriceAgeMs: wire.gas_price_age_ms ?? undefined,
    };
}
