import { isHex } from "viem";
import type { components } from "./autogen.js";
import { FyndError } from "./error.js";
import type {
    AccountOverride,
    Address,
    BlockInfo,
    ClientFeeParams,
    EncodingOptions,
    FeeBreakdown,
    HealthStatus,
    Hex,
    InstanceInfo,
    PermitDetails,
    PermitSingle,
    Quote,
    QuoteParams,
    Route,
    RouteFilter,
    SimulationOverrides,
    SimulationResult,
    Swap,
    Transaction,
} from "./types.js";

type WireOrder = components["schemas"]["Order"];
type WireSolution = components["schemas"]["Quote"];
type WireSolutionRequest = components["schemas"]["QuoteRequest"];
type WireSolutionOptions = components["schemas"]["QuoteOptions"];
type WireHealthStatus = components["schemas"]["HealthStatus"];
type WireInstanceInfo = components["schemas"]["InstanceInfo"];
type WireRoute = components["schemas"]["Route"];
type WireSwap = components["schemas"]["Swap"];
type WireBlockInfo = components["schemas"]["BlockInfo"];
type WireEncodingOptions = components["schemas"]["EncodingOptions"];
type WireTransaction = components["schemas"]["Transaction"];
type WirePermitSingle = components["schemas"]["PermitSingle"];
type WirePermitDetails = components["schemas"]["PermitDetails"];
type WireClientFeeParams = components["schemas"]["ClientFeeParams"];
type WireFeeBreakdown = components["schemas"]["FeeBreakdown"];
type WireSimulationResult = components["schemas"]["SimulationResult"];
type WireSimulationOverrides = components["schemas"]["SimulationOverrides"];
type WireRouteFilter = components["schemas"]["RouteFilter"];


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
            ...(params.options.routeFilter !== undefined
                ? {route_filter: toWireRouteFilter(params.options.routeFilter)}
                : {}),
        }
        : undefined;
    return {
        orders: [wireOrder],
        ...(wireOptions !== undefined ? {options: wireOptions} : {}),
    };
}

function toWireRouteFilter(filter: RouteFilter): WireRouteFilter {
    return {
        ...(filter.excludePools !== undefined ? {exclude_pools: filter.excludePools} : {}),
        ...(filter.excludeProtocols !== undefined
            ? {exclude_protocols: filter.excludeProtocols}
            : {}),
        ...(filter.excludeTokens !== undefined ? {exclude_tokens: filter.excludeTokens} : {}),
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
    const algorithm = orderSolution.algorithm ?? undefined;
    const feeBreakdown = orderSolution.fee_breakdown != null
        ? fromWireFeeBreakdown(orderSolution.fee_breakdown)
        : undefined;
    const simulationResult = orderSolution.simulation_result != null
        ? fromWireSimulationResult(orderSolution.simulation_result)
        : undefined;
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
        ...(algorithm !== undefined ? {algorithm} : {}),
        ...(feeBreakdown !== undefined ? {feeBreakdown} : {}),
        ...(simulationResult !== undefined ? {simulationResult} : {}),
    };
}

function fromWireRoute(wire: WireRoute): Route {
    return {swaps: wire.swaps.map(fromWireSwap)};
}

function fromWireSwap(wire: WireSwap): Swap {
    const simulationOverrides = wire.simulation_overrides != null
        ? fromWireSimulationOverrides(wire.simulation_overrides, wire.component_id)
        : undefined;
    return {
        componentId: wire.component_id,
        protocol: wire.protocol,
        tokenIn: wire.token_in as Address,
        tokenOut: wire.token_out as Address,
        amountIn: BigInt(wire.amount_in),
        amountOut: BigInt(wire.amount_out),
        gasEstimate: BigInt(wire.gas_estimate),
        ...(simulationOverrides !== undefined ? {simulationOverrides} : {}),
    };
}

/**
 * Converts a swap's simulation overrides into viem's state override shape. The overrides only serve
 * a simulation, so a malformed set is dropped with a warning and never fails the quote.
 */
function fromWireSimulationOverrides(
    wire: WireSimulationOverrides,
    componentId: string,
): SimulationOverrides | undefined {
    try {
        const accounts: AccountOverride[] = Object.entries(wire.accounts).map(
            ([address, account]) => {
                const stateDiff = Object.entries(account.stateDiff ?? {}).map(([slot, value]) => ({
                    slot: hexOfBytes(slot, 32, `storage slot of ${address}`),
                    value: hexOfBytes(value, 32, `storage value of ${address}`),
                }));
                return {
                    address: hexOfBytes(address, 20, 'account address') as Address,
                    ...(account.balance != null
                        ? {balance: hexQuantity(account.balance, 256, `balance of ${address}`)}
                        : {}),
                    ...(account.nonce != null
                        ? {nonce: Number(hexQuantity(account.nonce, 64, `nonce of ${address}`))}
                        : {}),
                    ...(stateDiff.length > 0 ? {stateDiff} : {}),
                };
            },
        );
        return {
            ...(wire.block_number != null ? {blockNumber: wire.block_number} : {}),
            ...(wire.block_timestamp != null ? {blockTimestamp: wire.block_timestamp} : {}),
            accounts,
        };
    } catch (error) {
        console.warn(
            `[fynd] dropping malformed simulation overrides of swap ${componentId}: ${String(error)}`,
        );
        return undefined;
    }
}

/** Returns `value` when it is exactly `bytes` bytes of hex, and throws naming `what` otherwise. */
function hexOfBytes(value: string, bytes: number, what: string): Hex {
    if (!isHex(value, {strict: true}) || value.length !== 2 + 2 * bytes) {
        throw new Error(`${what} ${value} is not ${bytes} bytes of hex`);
    }
    return value;
}

/** Parses a JSON-RPC hex quantity of at most `bits` bits, and throws naming `what` otherwise. */
function hexQuantity(value: string, bits: number, what: string): bigint {
    if (!isHex(value, {strict: true}) || value === '0x') {
        throw new Error(`${what} ${value} is not a hex quantity`);
    }
    const quantity = BigInt(value);
    if (quantity >= 1n << BigInt(bits)) {
        throw new Error(`${what} ${value} exceeds ${bits} bits`);
    }
    return quantity;
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
        ...(opts.transferType !== undefined && opts.transferType !== 'none' ? {transfer_type: opts.transferType} : {}),
        ...(opts.permit !== undefined ? {permit: toWirePermitSingle(opts.permit)} : {}),
        ...(opts.permit2Signature !== undefined
            ? {permit2_signature: opts.permit2Signature}
            : {}),
        ...(opts.clientFeeParams !== undefined
            ? {client_fee_params: toWireClientFeeParams(opts.clientFeeParams)}
            : {}),
        ...(opts.simulate === true ? {simulate: true} : {}),
    };
}

function toWireClientFeeParams(p: ClientFeeParams): WireClientFeeParams {
    return {
        bps: p.bps,
        receiver: p.receiver,
        max_contribution: p.maxContribution.toString(),
        deadline: p.deadline,
        // The wire field is required. Empty bytes tell the server to encode a placeholder that
        // the client patches after signing.
        signature: '0x',
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

function fromWireFeeBreakdown(wire: WireFeeBreakdown): FeeBreakdown {
    return {
        routerFee: BigInt(wire.router_fee),
        clientFee: BigInt(wire.client_fee),
        maxSlippage: BigInt(wire.max_slippage),
        minAmountReceived: BigInt(wire.min_amount_received),
        ...(wire.swaps_hash != null ? {swapsHash: wire.swaps_hash as Hex} : {}),
    };
}

function fromWireSimulationResult(wire: WireSimulationResult): SimulationResult {
    if (wire.status === "success") {
        return {status: "success", amountOut: BigInt(wire.amount_out), gasUsed: wire.gas_used};
    }
    return {status: "failure", reason: wire.reason};
}

function fromWireTransaction(wire: WireTransaction): Transaction {
    return {
        to: wire.to as Address,
        value: BigInt(wire.value),
        data: wire.data as Hex,
        ...(wire.client_fee_signature_offset != null
            ? {clientFeeSignatureOffset: wire.client_fee_signature_offset}
            : {}),
    };
}

export function fromWireHealth(wire: WireHealthStatus): HealthStatus {
    return {
        healthy: wire.healthy,
        lastUpdateMs: wire.last_update_ms,
        numSolverPools: wire.num_solver_pools,
        ...(wire.gas_price_age_ms != null ? { gasPriceAgeMs: wire.gas_price_age_ms } : {}),
    };
}

export function fromWireInstanceInfo(wire: WireInstanceInfo): InstanceInfo {
  return {
    routerAddress:  (wire.router_address ?? null) as Address | null,
    jitRouterAddress: (wire.jit_router_address ?? null) as Address | null,
    toAddress:        (wire.to_address ?? wire.router_address ?? null) as Address | null,
    permit2Address: wire.permit2_address as Address,
    chainId:        wire.chain_id,
  };
}
