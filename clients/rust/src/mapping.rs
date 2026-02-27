//! Wire format mirror structs and conversions between client types and the Fynd RPC server's
//! JSON wire format.
//!
//! This module defines private structs that exactly mirror what the server sends/expects,
//! avoiding a dependency on `fynd-rpc` (which pulls in actix-web and server infrastructure).

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::error::{ErrorCode, FyndError};
use crate::types::{
    BackendKind, BlockInfo, HealthStatus, Order, OrderSide, OrderSolution, Quote, QuoteOptions,
    QuoteParams, Route, SolutionStatus, Swap,
};

// ============================================================================
// WIRE FORMAT — REQUEST SIDE
// ============================================================================

#[derive(Serialize)]
pub(crate) struct WireSolutionRequest {
    orders: Vec<WireOrder>,
    options: WireSolutionOptions,
}

#[serde_as]
#[derive(Serialize)]
struct WireOrder {
    token_in: String,
    token_out: String,
    #[serde_as(as = "DisplayFromStr")]
    amount: BigUint,
    side: WireOrderSide,
    sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireOrderSide {
    Sell,
}

#[serde_as]
#[derive(Serialize, Default)]
struct WireSolutionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_responses: Option<usize>,
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_gas: Option<BigUint>,
}

// ============================================================================
// WIRE FORMAT — RESPONSE SIDE
// ============================================================================

#[serde_as]
#[derive(Deserialize)]
pub(crate) struct WireSolution {
    orders: Vec<WireOrderSolution>,
    #[serde_as(as = "DisplayFromStr")]
    total_gas_estimate: BigUint,
    solve_time_ms: u64,
}

#[serde_as]
#[derive(Deserialize)]
struct WireOrderSolution {
    order_id: String,
    status: WireSolutionStatus,
    #[serde(default)]
    route: Option<WireRoute>,
    #[serde_as(as = "DisplayFromStr")]
    amount_in: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    amount_out: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    gas_estimate: BigUint,
    #[serde(default)]
    price_impact_bps: Option<i32>,
    // Intentionally deserialized but dropped — internal server ranking field, not exposed to clients.
    #[serde_as(as = "DisplayFromStr")]
    _amount_out_net_gas: BigUint,
    block: WireBlockInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireSolutionStatus {
    Success,
    NoRouteFound,
    InsufficientLiquidity,
    Timeout,
    NotReady,
}

#[derive(Deserialize)]
struct WireRoute {
    swaps: Vec<WireSwap>,
}

#[serde_as]
#[derive(Deserialize)]
struct WireSwap {
    component_id: String,
    protocol: String,
    token_in: String,
    token_out: String,
    #[serde_as(as = "DisplayFromStr")]
    amount_in: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    amount_out: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    gas_estimate: BigUint,
}

#[derive(Deserialize)]
struct WireBlockInfo {
    number: u64,
    hash: String,
    timestamp: u64,
}

#[derive(Deserialize)]
pub(crate) struct WireHealthStatus {
    pub(crate) healthy: bool,
    pub(crate) last_update_ms: u64,
    pub(crate) num_solver_pools: usize,
}

#[derive(Deserialize)]
pub(crate) struct WireErrorResponse {
    pub(crate) error: String,
    pub(crate) code: String,
    #[serde(default)]
    pub(crate) _details: Option<serde_json::Value>,
}

// ============================================================================
// ADDRESS CONVERSION HELPERS
// ============================================================================

fn wire_addr_to_bytes(addr: &str) -> Result<bytes::Bytes, FyndError> {
    let hex_str = addr.strip_prefix("0x").unwrap_or(addr);
    let decoded = alloy::hex::decode(hex_str)
        .map_err(|e| FyndError::Protocol(format!("invalid address hex: {e}")))?;
    if decoded.len() != 20 {
        return Err(FyndError::Protocol(format!(
            "expected 20-byte address, got {} bytes",
            decoded.len()
        )));
    }
    Ok(bytes::Bytes::copy_from_slice(&decoded))
}

fn bytes_to_wire_addr(b: &bytes::Bytes) -> Result<String, FyndError> {
    if b.len() != 20 {
        return Err(FyndError::Protocol(format!(
            "expected 20-byte address, got {} bytes",
            b.len()
        )));
    }
    Ok(format!("0x{}", alloy::hex::encode(b.as_ref())))
}

pub(crate) fn bytes_to_alloy_address(
    b: &bytes::Bytes,
) -> Result<alloy::primitives::Address, FyndError> {
    if b.len() != 20 {
        return Err(FyndError::Protocol(format!(
            "expected 20-byte address, got {} bytes",
            b.len()
        )));
    }
    let arr: [u8; 20] = b
        .as_ref()
        .try_into()
        .expect("length checked above");
    Ok(alloy::primitives::Address::from(arr))
}

// ============================================================================
// CLIENT TYPES → WIRE FORMAT
// ============================================================================

pub(crate) fn quote_params_to_wire(params: QuoteParams) -> Result<WireSolutionRequest, FyndError> {
    let mut wire_orders = Vec::with_capacity(params.orders.len());
    for order in params.orders {
        wire_orders.push(order_to_wire(order)?);
    }
    Ok(WireSolutionRequest { orders: wire_orders, options: options_to_wire(params.options) })
}

fn order_to_wire(order: Order) -> Result<WireOrder, FyndError> {
    let token_in = bytes_to_wire_addr(order.token_in())?;
    let token_out = bytes_to_wire_addr(order.token_out())?;
    let sender = bytes_to_wire_addr(order.sender())?;
    let receiver = order
        .receiver()
        .map(bytes_to_wire_addr)
        .transpose()?;
    let side = match order.side() {
        OrderSide::Sell => WireOrderSide::Sell,
    };
    Ok(WireOrder { token_in, token_out, amount: order.amount().clone(), side, sender, receiver })
}

fn options_to_wire(opts: QuoteOptions) -> WireSolutionOptions {
    WireSolutionOptions {
        timeout_ms: opts.timeout_ms,
        min_responses: opts.min_responses,
        max_gas: opts.max_gas,
    }
}

// ============================================================================
// WIRE FORMAT → CLIENT TYPES
// ============================================================================

pub(crate) fn wire_to_quote(wire: WireSolution) -> Result<Quote, FyndError> {
    let mut orders = Vec::with_capacity(wire.orders.len());
    for ws in wire.orders {
        orders.push(wire_to_order_solution(ws)?);
    }
    Ok(Quote::new(orders, wire.total_gas_estimate, wire.solve_time_ms))
}

fn wire_to_order_solution(ws: WireOrderSolution) -> Result<OrderSolution, FyndError> {
    let status = wire_to_status(ws.status);
    let route = ws
        .route
        .map(wire_to_route)
        .transpose()?;
    let block = BlockInfo::new(ws.block.number, ws.block.hash, ws.block.timestamp);
    Ok(OrderSolution {
        order_id: ws.order_id,
        status,
        backend: BackendKind::Fynd,
        route,
        amount_in: ws.amount_in,
        amount_out: ws.amount_out,
        gas_estimate: ws.gas_estimate,
        price_impact_bps: ws.price_impact_bps,
        block,
    })
}

fn wire_to_status(ws: WireSolutionStatus) -> SolutionStatus {
    match ws {
        WireSolutionStatus::Success => SolutionStatus::Success,
        WireSolutionStatus::NoRouteFound => SolutionStatus::NoRouteFound,
        WireSolutionStatus::InsufficientLiquidity => SolutionStatus::InsufficientLiquidity,
        WireSolutionStatus::Timeout => SolutionStatus::Timeout,
        WireSolutionStatus::NotReady => SolutionStatus::NotReady,
    }
}

fn wire_to_route(wr: WireRoute) -> Result<Route, FyndError> {
    let mut swaps = Vec::with_capacity(wr.swaps.len());
    for ws in wr.swaps {
        swaps.push(wire_to_swap(ws)?);
    }
    Ok(Route::new(swaps))
}

fn wire_to_swap(ws: WireSwap) -> Result<Swap, FyndError> {
    let token_in = wire_addr_to_bytes(&ws.token_in)?;
    let token_out = wire_addr_to_bytes(&ws.token_out)?;
    Ok(Swap::new(
        ws.component_id,
        ws.protocol,
        token_in,
        token_out,
        ws.amount_in,
        ws.amount_out,
        ws.gas_estimate,
    ))
}

pub(crate) fn wire_to_health(wh: WireHealthStatus) -> HealthStatus {
    HealthStatus::new(wh.healthy, wh.last_update_ms, wh.num_solver_pools)
}

pub(crate) fn wire_error_to_fynd(we: WireErrorResponse) -> FyndError {
    let code = ErrorCode::from_server_code(&we.code);
    FyndError::Api { code, message: we.error }
}
