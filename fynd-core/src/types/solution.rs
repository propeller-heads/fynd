//! Public interfaces for the solve endpoint.
//!
//! This module defines all solution-related request and response types exposed to clients:
//!
//! ## Request Types
//! - [`SolutionRequest`] - Top-level request containing orders to solve
//! - [`Order`] - A single swap order with token pair and amount
//! - [`SolutionOptions`] - Optional parameters for solving behavior
//!
//! ## Response Types
//! - [`Solution`] - Top-level response with solutions for all orders
//! - [`SingleOrderSolution`] - Solution for a single order with timing information
//! - [`OrderSolution`] - Solution for a single order including route
//! - [`Route`] - Sequence of swaps to execute
//! - [`Swap`] - A single swap on a specific protocol

use std::collections::HashMap;

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
pub use tycho_execution::encoding::models::UserTransferType;
use tycho_simulation::{
    tycho_common::models::Address,
    tycho_core::{
        models::{protocol::ProtocolComponent, token::Token},
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    },
};
use uuid::Uuid;

use super::primitives::ComponentId;
use crate::AlgorithmError;

// ============================================================================
// REQUEST TYPES
// ============================================================================

// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionRequest {
    /// Orders to solve.
    pub orders: Vec<Order>,
    /// Optional solving parameters that apply to all orders.
    #[serde(default)]
    pub options: SolutionOptions,
}

/// Options to customize the solving behavior.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SolutionOptions {
    /// Timeout in milliseconds. If `None`, uses server default.
    pub timeout_ms: Option<u64>,
    /// Minimum number of solver responses to wait for before returning.
    /// If `None` or `0`, waits for all solvers to respond (or timeout).
    ///
    /// Use the `/health` endpoint to check `num_solver_pools` before setting this value.
    /// Values exceeding the number of active solver pools are clamped internally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_responses: Option<usize>,
    /// Maximum gas cost allowed for a solution. Solutions exceeding this are filtered out.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_gas: Option<BigUint>,
    pub encoding_options: Option<EncodingOptions>,
}

/// Options to customize the encoding behavior.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodingOptions {
    pub slippage: f64,
    /// Token transfer method. Defaults to `TransferFrom`.
    #[serde(default = "default_transfer_type")]
    pub transfer_type: UserTransferType,
    /// Permit2 single-token authorization. Required when using `TransferFromPermit2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit: Option<PermitSingle>,
    /// Permit2 signature (65 bytes). Required when `permit` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit2_signature: Option<Bytes>,
}

/// A single permit for permit2 token transfer authorization.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermitSingle {
    /// The permit details (token, amount, expiration, nonce).
    pub details: PermitDetails,
    /// Address authorized to spend the tokens (typically the router).
    pub spender: Bytes,
    /// Deadline timestamp for the permit signature.
    #[serde_as(as = "DisplayFromStr")]
    pub sig_deadline: BigUint,
}

/// Details for a permit2 single-token permit.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermitDetails {
    /// Token address for which the permit is granted.
    pub token: Bytes,
    /// Amount of tokens approved.
    #[serde_as(as = "DisplayFromStr")]
    pub amount: BigUint,
    /// Expiration timestamp for the permit.
    #[serde_as(as = "DisplayFromStr")]
    pub expiration: BigUint,
    /// Nonce to prevent replay attacks.
    #[serde_as(as = "DisplayFromStr")]
    pub nonce: BigUint,
}

// ============================================================================
// RESPONSE TYPES
// ============================================================================

/// Complete solution for a [`SolutionRequest`].
///
/// Contains a solution for each order in the request, along with aggregate
/// gas estimates and timing information.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solution {
    /// Solutions for each order, in the same order as the request.
    pub orders: Vec<OrderSolution>,
    /// Total estimated gas for executing all swaps (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub total_gas_estimate: BigUint,
    /// Time taken to compute this solution, in milliseconds.
    pub solve_time_ms: u64,
}

/// A single swap order to be solved.
///
/// An order specifies an intent to swap one token for another.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Unique identifier for this order.
    ///
    /// Auto-generated by the API.
    #[serde(default = "generate_order_id", skip_deserializing)]
    pub id: String,
    /// Input token address (the token being sold).
    pub token_in: Address,
    /// Output token address (the token being bought).
    pub token_out: Address,
    /// Amount to swap, interpreted according to `side` (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub amount: BigUint,
    /// Whether this is a sell (exact input) or buy (exact output) order.
    pub side: OrderSide,
    /// Address that will send the input tokens.
    pub sender: Address,
    /// Address that will receive the output tokens.
    ///
    /// Defaults to `sender` if not specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<Address>,
}

impl Order {
    /// Returns `true` if this is a sell order (exact input amount).
    pub fn is_sell(&self) -> bool {
        matches!(self.side, OrderSide::Sell)
    }

    /// Returns the address that will receive the output tokens.
    ///
    /// Returns `receiver` if set, otherwise returns `sender`.
    pub fn effective_receiver(&self) -> Address {
        self.receiver
            .clone()
            .unwrap_or_else(|| self.sender.clone())
    }

    /// Validates the order structure.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - `token_in` and `token_out` are the same address
    /// - `amount` is zero
    pub fn validate(&self) -> Result<(), OrderValidationError> {
        if self.token_in == self.token_out {
            return Err(OrderValidationError::SameTokens);
        }

        if self.amount.is_zero() {
            return Err(OrderValidationError::ZeroAmount);
        }

        Ok(())
    }
}

/// Specifies the side of an order: sell (exact input) or buy (exact output).
///
/// Currently only `Sell` is supported. `Buy` will be added in a future version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    /// Sell exactly the specified amount of the input token.
    Sell,
}

/// Errors that can occur when validating an [`Order`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum OrderValidationError {
    #[error("token_in and token_out must be different")]
    SameTokens,
    #[error("amount must be non-zero")]
    ZeroAmount,
}

/// Internal wrapper used by workers when returning a solution.
///
/// This wraps [`OrderSolution`] with per-worker timing information.
/// The `solve_time_ms` here is the time taken by an individual worker/algorithm,
/// not the total OrderManager orchestration time (which is in [`Solution`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleOrderSolution {
    /// The solution for the order.
    pub order: OrderSolution,
    /// Time taken by this specific worker to compute the solution, in milliseconds.
    pub solve_time_ms: u64,
}

/// Solution for a single [`Order`].
///
/// Contains the route to execute (if found), along with expected amounts,
/// gas estimates, and status information.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSolution {
    /// ID of the order this solution corresponds to.
    pub order_id: String,
    /// Status indicating whether a route was found.
    pub status: SolutionStatus,
    /// The route to execute, if a valid route was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out: BigUint,
    /// Estimated gas cost for executing this route (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub gas_estimate: BigUint,
    /// Price impact in basis points (1 bip = 0.01%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_impact_bps: Option<i32>,
    /// Amount out minus gas cost in output token terms.
    /// Used by OrderManager to compare solutions from different solvers.
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out_net_gas: BigUint,
    /// Block at which this quote was computed.
    pub block: BlockInfo,
    /// Algorithm that found this solution (internal use only).
    #[serde(skip)]
    pub algorithm: String,
    /// Effective gas price (in wei) at the time the route was computed.
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub gas_price: Option<BigUint>,
    /// An encoded EVM transaction ready to be submitted on-chain.
    pub transaction: Option<Transaction>,
}

/// Status of an order solution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SolutionStatus {
    /// A valid route was found.
    Success,
    /// No route exists between the specified tokens.
    NoRouteFound,
    /// A route exists but available liquidity is insufficient.
    InsufficientLiquidity,
    /// The solver timed out before finding a route.
    Timeout,
    /// No solver workers are ready (e.g., market data not yet initialized).
    NotReady,
}

impl From<AlgorithmError> for SolutionStatus {
    fn from(err: crate::algorithm::AlgorithmError) -> Self {
        match err {
            AlgorithmError::NoPath { .. } => SolutionStatus::NoRouteFound,
            AlgorithmError::InsufficientLiquidity => SolutionStatus::InsufficientLiquidity,
            AlgorithmError::Timeout { .. } => SolutionStatus::Timeout,
            AlgorithmError::ExactOutNotSupported => SolutionStatus::NoRouteFound,
            AlgorithmError::Other(_) => SolutionStatus::NoRouteFound,
            AlgorithmError::InvalidConfiguration { .. } => SolutionStatus::NoRouteFound,
            AlgorithmError::SimulationFailed { .. } => SolutionStatus::NoRouteFound,
            AlgorithmError::DataNotFound { .. } => SolutionStatus::NoRouteFound,
        }
    }
}

/// Block information at which a quote was computed.
///
/// Quotes are only valid for the block at which they were computed. Market
/// conditions may change in subsequent blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockInfo {
    /// Block number.
    pub number: u64,
    /// Block hash as a hex string.
    pub hash: String,
    /// Block timestamp in Unix seconds.
    pub timestamp: u64,
}

// ============================================================================
// ROUTE & SWAP TYPES
// ============================================================================

/// A route consisting of one or more sequential swaps.
///
/// A route describes the path through liquidity pools to execute a swap.
/// For multi-hop swaps, the output of each swap becomes the input of the next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Ordered sequence of swaps to execute.
    pub swaps: Vec<Swap>,
}

impl Route {
    /// Creates a new route from an ordered sequence of swaps.
    pub fn new(swaps: Vec<Swap>) -> Self {
        Self { swaps }
    }
}

/// The result of a route-finding algorithm: a route plus its gas-adjusted net output.
///
/// `net_amount_out` is the output amount after subtracting gas costs converted to output token
/// units. It can be negative if gas exceeds output (e.g., tiny swaps or inaccurate gas
/// estimation). Used by the worker to populate `amount_out_net_gas` on `OrderSolution`.
#[derive(Debug, Clone)]
pub struct RouteResult {
    /// The route (sequence of swaps) to execute.
    pub route: Route,
    /// Net amount out after accounting for gas costs in output token terms.
    pub net_amount_out: BigInt,
    /// Effective gas price (in wei) at the time the route was computed.
    pub gas_price: BigUint,
}

impl Route {
    /// Returns the number of hops (swaps) in this route.
    pub fn hop_count(&self) -> usize {
        self.swaps.len()
    }

    /// Returns a human-readable path description (e.g., "WETH -> USDC -> DAI").
    ///
    /// Falls back to token address if token not found in the provided map.
    pub fn path_description(&self, tokens: &HashMap<Address, Token>) -> String {
        let mut symbols = Vec::with_capacity(self.swaps.len() + 1);

        for (i, swap) in self.swaps.iter().enumerate() {
            if i == 0 {
                let symbol = tokens
                    .get(&swap.token_in)
                    .map(|t| t.symbol.clone())
                    .unwrap_or_else(|| format!("{:?}", swap.token_in));
                symbols.push(symbol);
            }
            let symbol = tokens
                .get(&swap.token_out)
                .map(|t| t.symbol.clone())
                .unwrap_or_else(|| format!("{:?}", swap.token_out));
            symbols.push(symbol);
        }

        symbols.join(" -> ")
    }

    /// Returns the input token of the route (first swap's input).
    pub fn input_token(&self) -> Option<Address> {
        self.swaps
            .first()
            .map(|s| s.token_in.clone())
    }

    /// Returns the output token of the route (last swap's output).
    pub fn output_token(&self) -> Option<Address> {
        self.swaps
            .last()
            .map(|s| s.token_out.clone())
    }

    /// Returns intermediate tokens (excluding input and output).
    pub fn intermediate_tokens(&self) -> Vec<Address> {
        if self.swaps.len() <= 1 {
            return vec![];
        }

        self.swaps[..self.swaps.len() - 1]
            .iter()
            .map(|s| s.token_out.clone())
            .collect()
    }

    /// Returns the total gas estimate for all swaps in this route.
    pub fn total_gas(&self) -> BigUint {
        self.swaps
            .iter()
            .map(|s| &s.gas_estimate)
            .fold(BigUint::ZERO, |acc, g| acc + g)
    }

    /// Validates that the route is well-formed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The route has no swaps
    /// - Consecutive swaps are not connected (output token != next input token)
    pub fn validate(&self) -> Result<(), RouteValidationError> {
        if self.swaps.is_empty() {
            return Err(RouteValidationError::EmptyRoute);
        }

        for window in self.swaps.windows(2) {
            if window[0].token_out != window[1].token_in {
                return Err(RouteValidationError::DisconnectedSwaps {
                    first_out: window[0].token_out.clone(),
                    second_in: window[1].token_in.clone(),
                });
            }
        }

        Ok(())
    }
}

/// Errors that can occur when validating a [`Route`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum RouteValidationError {
    #[error("route must contain at least one swap")]
    EmptyRoute,
    #[error("swaps are not connected: {first_out} != {second_in}")]
    DisconnectedSwaps { first_out: Address, second_in: Address },
}

/// A single swap within a route.
///
/// Represents an atomic swap on a specific liquidity pool (component).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swap {
    /// Identifier of the liquidity pool component.
    pub component_id: ComponentId,
    /// Protocol system identifier (e.g., "uniswap_v2", "uniswap_v3", "vm:balancer").
    pub protocol: String,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out: BigUint,
    /// Estimated gas cost for this swap (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    pub gas_estimate: BigUint,
    /// Protocol component description.
    pub protocol_component: ProtocolComponent,
    /// Protocol state used to perform the swap.
    pub protocol_state: Box<dyn ProtocolSim>,
}

impl Swap {
    /// Creates a new swap with an auto-calculated gas estimate.
    // All arguments correspond directly to struct fields; no grouping makes sense here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        component_id: ComponentId,
        protocol: String,
        token_in: Address,
        token_out: Address,
        amount_in: BigUint,
        amount_out: BigUint,
        gas_estimate: BigUint,
        protocol_component: ProtocolComponent,
        protocol_state: Box<dyn ProtocolSim>,
    ) -> Self {
        Self {
            component_id,
            protocol,
            token_in,
            token_out,
            amount_in,
            amount_out,
            gas_estimate,
            protocol_component,
            protocol_state,
        }
    }
}

/// An encoded EVM transaction ready to be submitted on-chain.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// Contract address to call.
    pub to: Bytes,
    /// Native token value to send with the transaction.
    #[serde_as(as = "DisplayFromStr")]
    pub value: num_bigint::BigUint,
    /// ABI-encoded calldata.
    pub data: Vec<u8>,
}

// ============================================================================
// PRIVATE HELPERS
// ============================================================================

/// Generates a default UserTransferType value.
fn default_transfer_type() -> UserTransferType {
    UserTransferType::TransferFrom
}
/// Generates a unique order ID using UUID v4.
fn generate_order_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::algorithm::test_utils::{component, token, MockProtocolSim};

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order(token_in_byte: u8, token_out_byte: u8, amount: u64) -> Order {
        Order {
            id: generate_order_id(),
            token_in: make_address(token_in_byte),
            token_out: make_address(token_out_byte),
            amount: BigUint::from(amount),
            side: OrderSide::Sell,
            sender: make_address(0xAA),
            receiver: None,
        }
    }

    fn make_swap(token_in_byte: u8, token_out_byte: u8, amount_in: u64, amount_out: u64) -> Swap {
        let token_in = token(token_in_byte, "TIN");
        let token_out = token(token_out_byte, "TOUT");
        Swap {
            component_id: "pool-1".to_string(),
            protocol: "uniswap_v2".to_string(),
            token_in: make_address(token_in_byte),
            token_out: make_address(token_out_byte),
            amount_in: BigUint::from(amount_in),
            amount_out: BigUint::from(amount_out),
            gas_estimate: BigUint::from(100_000u64),
            protocol_component: component("test-pool", &[token_in, token_out]),
            protocol_state: Box::new(MockProtocolSim::default()),
        }
    }

    // -------------------------------------------------------------------------
    // Order Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_order_is_sell() {
        let order = make_order(0x01, 0x02, 1000);
        assert!(order.is_sell());
    }

    #[rstest]
    #[case::with_receiver(Some(0xBB), 0xBB)]
    #[case::without_receiver(None, 0xAA)]
    fn test_order_effective_receiver(#[case] receiver: Option<u8>, #[case] expected: u8) {
        let mut order = make_order(0x01, 0x02, 1000);
        order.receiver = receiver.map(make_address);
        assert_eq!(order.effective_receiver(), make_address(expected));
    }

    #[rstest]
    #[case::valid(0x01, 0x02, 1000, true, None)]
    #[case::same_tokens(0x01, 0x01, 1000, false, Some("SameTokens"))]
    #[case::zero_amount(0x01, 0x02, 0, false, Some("ZeroAmount"))]
    fn test_order_validation(
        #[case] token_in: u8,
        #[case] token_out: u8,
        #[case] amount: u64,
        #[case] should_pass: bool,
        #[case] error_type: Option<&str>,
    ) {
        let order = make_order(token_in, token_out, amount);
        let result = order.validate();

        assert_eq!(result.is_ok(), should_pass);
        if let Some(err_name) = error_type {
            let err = result.unwrap_err();
            match err_name {
                "SameTokens" => assert!(matches!(err, OrderValidationError::SameTokens)),
                "ZeroAmount" => assert!(matches!(err, OrderValidationError::ZeroAmount)),
                _ => panic!("Unknown error type"),
            }
        }
    }

    #[test]
    fn test_order_id_auto_generated() {
        let id = generate_order_id();
        assert!(!id.is_empty());
        assert!(id.contains('-')); // UUIDs contain dashes
    }

    // -------------------------------------------------------------------------
    // Route Tests
    // -------------------------------------------------------------------------

    fn make_route(swaps: Vec<(u8, u8)>) -> Route {
        let swaps: Vec<Swap> = swaps
            .into_iter()
            .map(|(a, b)| make_swap(a, b, 1000, 990))
            .collect();
        Route::new(swaps)
    }

    #[rstest]
    #[case::empty(vec![], 0)]
    #[case::single(vec![(0x01, 0x02)], 1)]
    #[case::two_hops(vec![(0x01, 0x02), (0x02, 0x03)], 2)]
    #[case::three_hops(vec![(0x01, 0x02), (0x02, 0x03), (0x03, 0x04)], 3)]
    fn test_route_hop_count(#[case] swaps: Vec<(u8, u8)>, #[case] expected: usize) {
        let route = make_route(swaps);
        assert_eq!(route.hop_count(), expected);
    }

    #[rstest]
    #[case::empty(vec![], None)]
    #[case::single(vec![(0x01, 0x02)], Some(0x01))]
    #[case::multi(vec![(0x01, 0x02), (0x02, 0x03)], Some(0x01))]
    fn test_route_input_token(#[case] swaps: Vec<(u8, u8)>, #[case] expected: Option<u8>) {
        let route = make_route(swaps);
        assert_eq!(route.input_token(), expected.map(make_address));
    }

    #[rstest]
    #[case::empty(vec![], None)]
    #[case::single(vec![(0x01, 0x02)], Some(0x02))]
    #[case::multi(vec![(0x01, 0x02), (0x02, 0x03)], Some(0x03))]
    fn test_route_output_token(#[case] swaps: Vec<(u8, u8)>, #[case] expected: Option<u8>) {
        let route = make_route(swaps);
        assert_eq!(route.output_token(), expected.map(make_address));
    }

    #[rstest]
    #[case::empty(vec![], vec![])]
    #[case::single(vec![(0x01, 0x02)], vec![])]
    #[case::two_hops(vec![(0x01, 0x02), (0x02, 0x03)], vec![0x02])]
    #[case::three_hops(vec![(0x01, 0x02), (0x02, 0x03), (0x03, 0x04)], vec![0x02, 0x03])]
    fn test_route_intermediate_tokens(#[case] swaps: Vec<(u8, u8)>, #[case] expected: Vec<u8>) {
        let route = make_route(swaps);
        let intermediates = route.intermediate_tokens();
        assert_eq!(
            intermediates,
            expected
                .into_iter()
                .map(make_address)
                .collect::<Vec<_>>()
        );
    }

    #[rstest]
    #[case::empty(0, 0u64)]
    #[case::single(1, 100_000u64)]
    #[case::two_swaps(2, 200_000u64)]
    #[case::three_swaps(3, 300_000u64)]
    fn test_route_total_gas(#[case] num_swaps: usize, #[case] expected_gas: u64) {
        let swaps: Vec<Swap> = (0..num_swaps)
            .map(|i| make_swap(i as u8, (i + 1) as u8, 1000, 990))
            .collect();
        let route = Route::new(swaps);
        assert_eq!(route.total_gas(), BigUint::from(expected_gas));
    }

    #[rstest]
    #[case::valid_single(vec![(0x01, 0x02)], true, None)]
    #[case::valid_connected(vec![(0x01, 0x02), (0x02, 0x03)], true, None)]
    #[case::empty(vec![], false, Some("EmptyRoute"))]
    #[case::disconnected(vec![(0x01, 0x02), (0x03, 0x04)], false, Some("DisconnectedSwaps"))]
    fn test_route_validation(
        #[case] swaps: Vec<(u8, u8)>,
        #[case] should_pass: bool,
        #[case] error_type: Option<&str>,
    ) {
        let route = make_route(swaps);
        let result = route.validate();

        assert_eq!(result.is_ok(), should_pass);
        if let Some(err_name) = error_type {
            let err = result.unwrap_err();
            match err_name {
                "EmptyRoute" => assert!(matches!(err, RouteValidationError::EmptyRoute)),
                "DisconnectedSwaps" => {
                    assert!(matches!(err, RouteValidationError::DisconnectedSwaps { .. }))
                }
                _ => panic!("Unknown error type"),
            }
        }
    }

    // -------------------------------------------------------------------------
    // Serialization Tests - BigUint as String
    // -------------------------------------------------------------------------

    #[test]
    fn test_order_serializes_amount_as_string() {
        let order = make_order(0x01, 0x02, 1_000_000_000_000_000_000);
        let json = serde_json::to_string(&order).unwrap();

        assert!(json.contains(r#""amount":"1000000000000000000""#));
        assert!(!json.contains(r#""amount":1000000000000000000"#));
    }

    #[test]
    fn test_order_deserializes_amount_from_string() {
        let json = r#"{
            "token_in": "0x0101010101010101010101010101010101010101",
            "token_out": "0x0202020202020202020202020202020202020202",
            "amount": "1000000000000000000",
            "side": "sell",
            "sender": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }"#;

        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.amount, BigUint::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn test_swap_serializes_amounts_as_strings() {
        let swap = make_swap(0x01, 0x02, 1_000_000_000, 999_000_000);
        let json = serde_json::to_string(&swap).unwrap();

        assert!(json.contains(r#""amount_in":"1000000000""#));
        assert!(json.contains(r#""amount_out":"999000000""#));
        assert!(json.contains(r#""gas_estimate":"100000""#));
    }

    #[test]
    fn test_swap_deserializes_amounts_from_strings() {
        let json = r#"{
            "component_id": "pool-1",
            "protocol": "uniswap_v2",
            "token_in": "0x0101010101010101010101010101010101010101",
            "token_out": "0x0202020202020202020202020202020202020202",
            "amount_in": "1000000000000000000",
            "amount_out": "999000000000000000",
            "gas_estimate": "150000",
            "protocol_component": {
                "id": "test-pool",
                "protocol_system": "uniswap_v2",
                "protocol_type_name": "swap",
                "chain": "ethereum",
                "tokens": [
                    "0x0101010101010101010101010101010101010101",
                    "0x0202020202020202020202020202020202020202"
                ],
                "contract_addresses": [],
                "static_attributes": {},
                "change": "Update",
                "creation_tx": "0x",
                "created_at": "1970-01-01T00:00:00"
            },
            "protocol_state": {
                "protocol": "MockProtocolSim",
                "state": {
                    "spot_price": 2,
                    "gas": 50000,
                    "liquidity": 340282366920938463463374607431768211455,
                    "fee": 0.0
                }
            }
        }"#;

        let swap: Swap = serde_json::from_str(json).unwrap();
        assert_eq!(swap.amount_in, BigUint::from(1_000_000_000_000_000_000u64));
        assert_eq!(swap.amount_out, BigUint::from(999_000_000_000_000_000u64));
        assert_eq!(swap.gas_estimate, BigUint::from(150_000u64));
    }

    #[test]
    fn test_large_amounts_serialize_correctly() {
        // 2^256 - 1, larger than JS safe integer (2^53 - 1)
        let large_amount = BigUint::parse_bytes(
            b"115792089237316195423570985008687907853269984665640564039457584007913129639935",
            10,
        )
        .unwrap();

        let order = Order {
            id: "test".to_string(),
            token_in: make_address(0x01),
            token_out: make_address(0x02),
            amount: large_amount.clone(),
            side: OrderSide::Sell,
            sender: make_address(0xAA),
            receiver: None,
        };

        let json = serde_json::to_string(&order).unwrap();
        assert!(json.contains(
            r#""amount":"115792089237316195423570985008687907853269984665640564039457584007913129639935""#
        ));

        // Round-trip preserves value
        let deserialized: Order = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.amount, large_amount);
    }

    #[test]
    fn test_solution_serializes_amounts_as_strings() {
        let solution = Solution {
            orders: vec![],
            total_gas_estimate: BigUint::from(500_000u64),
            solve_time_ms: 10,
        };

        let json = serde_json::to_string(&solution).unwrap();
        assert!(json.contains(r#""total_gas_estimate":"500000""#));
    }

    // -------------------------------------------------------------------------
    // path_description Tests
    // -------------------------------------------------------------------------

    fn make_token(byte: u8, symbol: &str) -> Token {
        use tycho_simulation::evm::tycho_models::Chain;
        Token {
            address: make_address(byte),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    #[rstest]
    #[case::empty(vec![], vec![], "")]
    #[case::single_hop(vec![(0x01, 0x02)], vec![(0x01, "WETH"), (0x02, "USDC")], "WETH -> USDC")]
    #[case::multi_hop(
        vec![(0x01, 0x02), (0x02, 0x03)],
        vec![(0x01, "WETH"), (0x02, "USDC"), (0x03, "DAI")],
        "WETH -> USDC -> DAI"
    )]
    #[case::cyclic(
        vec![(0x01, 0x02), (0x02, 0x01)],
        vec![(0x01, "WETH"), (0x02, "USDC")],
        "WETH -> USDC -> WETH"
    )]
    #[case::unknown_tokens(
        vec![(0x01, 0x02)],
        vec![],
        "Bytes(0x0101010101010101010101010101010101010101) -> Bytes(0x0202020202020202020202020202020202020202)"
    )]
    fn test_path_description(
        #[case] swaps: Vec<(u8, u8)>,
        #[case] token_data: Vec<(u8, &str)>,
        #[case] expected: &str,
    ) {
        let route = make_route(swaps);
        let tokens: HashMap<Address, Token> = token_data
            .into_iter()
            .map(|(byte, symbol)| {
                let t = make_token(byte, symbol);
                (t.address.clone(), t)
            })
            .collect();

        assert_eq!(route.path_description(&tokens), expected);
    }
}
