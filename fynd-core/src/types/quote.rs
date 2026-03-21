//! Public interfaces for the solve endpoint.
//!
//! This module defines all solution-related request and response types exposed to clients:
//!
//! ## Request Types
//! - [`QuoteRequest`] - Top-level request containing orders to solve
//! - [`Order`] - A single swap order with token pair and amount
//! - [`QuoteOptions`] - Optional parameters for solving behavior
//!
//! ## Response Types
//! - [`Quote`] - Top-level response with solutions for all orders
//! - [`SingleOrderQuote`] - Quote for a single order with timing information
//! - [`OrderQuote`] - Quote for a single order including route
//! - [`Route`] - Sequence of swaps to execute
//! - [`Swap`] - A single swap on a specific protocol

use std::collections::{HashMap, HashSet};

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
use crate::{price_guard::config::PriceGuardConfig, AlgorithmError};

// ============================================================================
// REQUEST TYPES
// ============================================================================

// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteRequest {
    /// Orders to solve.
    orders: Vec<Order>,
    /// Optional solving parameters that apply to all orders.
    #[serde(default)]
    options: QuoteOptions,
}

impl QuoteRequest {
    /// Creates a new solution request.
    pub fn new(orders: Vec<Order>, options: QuoteOptions) -> Self {
        Self { orders, options }
    }

    /// Returns the orders to solve.
    pub fn orders(&self) -> &[Order] {
        &self.orders
    }

    /// Returns the solving options.
    pub fn options(&self) -> &QuoteOptions {
        &self.options
    }
}

/// Options to customize the solving behavior.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuoteOptions {
    /// Timeout in milliseconds. If `None`, uses server default.
    timeout_ms: Option<u64>,
    /// Minimum number of solver responses to wait for before returning.
    /// If `None` or `0`, waits for all solvers to respond (or timeout).
    ///
    /// Use the `/health` endpoint to check `num_solver_pools` before setting this value.
    /// Values exceeding the number of active solver pools are clamped internally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_responses: Option<usize>,
    /// Maximum gas cost allowed for a solution. Quotes exceeding this are filtered out.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_gas: Option<BigUint>,
    /// Options for encoding the solution into an on-chain transaction.
    /// If `None`, the solution is returned without an encoded transaction.
    encoding_options: Option<EncodingOptions>,
    /// TODO: Should we move to `Order` for per-order price guard control if needed?
    /// One might want to turn off the price guard for a few orders of their list, but not all,
    /// depending on tokens. At the same time - I kept it in here to be coherent with where we
    /// are passing the encoding options. Please let me know if you disagree.
    /// Per-request price guard overrides. If `None`, uses server defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    price_guard: Option<PriceGuardConfig>,
}

impl QuoteOptions {
    /// Sets the timeout in milliseconds.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// Sets the minimum number of solver responses to wait for.
    pub fn with_min_responses(mut self, n: usize) -> Self {
        self.min_responses = Some(n);
        self
    }

    /// Sets the maximum gas cost allowed for a solution.
    pub fn with_max_gas(mut self, gas: BigUint) -> Self {
        self.max_gas = Some(gas);
        self
    }

    /// Sets the encoding options.
    pub fn with_encoding_options(mut self, opts: EncodingOptions) -> Self {
        self.encoding_options = Some(opts);
        self
    }

    /// Returns the timeout in milliseconds.
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    /// Returns the minimum number of solver responses.
    pub fn min_responses(&self) -> Option<usize> {
        self.min_responses
    }

    /// Returns the maximum gas cost constraint.
    pub fn max_gas(&self) -> Option<&BigUint> {
        self.max_gas.as_ref()
    }

    /// Returns the encoding options.
    pub fn encoding_options(&self) -> Option<&EncodingOptions> {
        self.encoding_options.as_ref()
    }

    /// Sets per-request price guard config.
    pub fn with_price_guard(mut self, config: PriceGuardConfig) -> Self {
        self.price_guard = Some(config);
        self
    }

    /// Returns the per-request price guard config.
    pub fn price_guard(&self) -> Option<&PriceGuardConfig> {
        self.price_guard.as_ref()
    }
}

/// Client fee configuration for the Tycho Router.
///
/// When present, the router charges a client fee in basis points on the swap output.
/// The `signature` must be an EIP-712 signature by the `receiver` over the
/// `ClientFee` typed data (see client libraries for helper methods).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFeeParams {
    /// Fee in basis points (0–10,000). 100 = 1%.
    bps: u16,
    /// Address that receives the fee (also the required EIP-712 signer).
    receiver: Bytes,
    /// Maximum subsidy from the client's vault balance (in token out denomination)
    #[serde_as(as = "DisplayFromStr")]
    max_contribution: BigUint,
    /// Unix timestamp after which the signature is invalid.
    deadline: u64,
    /// 65-byte EIP-712 ECDSA signature by `receiver`.
    signature: Bytes,
}

impl ClientFeeParams {
    /// Creates new client fee params.
    pub fn new(
        bps: u16,
        receiver: Bytes,
        max_contribution: BigUint,
        deadline: u64,
        signature: Bytes,
    ) -> Self {
        Self { bps, receiver, max_contribution, deadline, signature }
    }

    /// Fee in basis points.
    pub fn bps(&self) -> u16 {
        self.bps
    }

    /// Address that receives the fee.
    pub fn receiver(&self) -> &Bytes {
        &self.receiver
    }

    /// Maximum subsidy from client vault.
    pub fn max_contribution(&self) -> &BigUint {
        &self.max_contribution
    }

    /// Signature deadline timestamp.
    pub fn deadline(&self) -> u64 {
        self.deadline
    }

    /// EIP-712 signature by the receiver.
    pub fn signature(&self) -> &Bytes {
        &self.signature
    }
}

/// Breakdown of fees applied to the swap output by the on-chain FeeCalculator.
///
/// All amounts are absolute values in output token units.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeBreakdown {
    /// Router protocol fee (fee on output + router's share of client fee).
    #[serde_as(as = "DisplayFromStr")]
    router_fee: BigUint,
    /// Client's portion of the fee (after the router takes its share).
    #[serde_as(as = "DisplayFromStr")]
    client_fee: BigUint,
    /// Maximum slippage amount: (amount_out - router_fee - client_fee) * slippage.
    #[serde_as(as = "DisplayFromStr")]
    max_slippage: BigUint,
    /// Minimum amount the user receives on-chain.
    /// Equal to amount_out - router_fee - client_fee - max_slippage.
    /// This is the value encoded as min_amount_out in the transaction.
    #[serde_as(as = "DisplayFromStr")]
    min_amount_received: BigUint,
}

impl FeeBreakdown {
    /// Creates a new fee breakdown.
    pub fn new(
        router_fee: BigUint,
        client_fee: BigUint,
        max_slippage: BigUint,
        min_amount_received: BigUint,
    ) -> Self {
        Self { router_fee, client_fee, max_slippage, min_amount_received }
    }

    /// Router protocol fee amount.
    pub fn router_fee(&self) -> &BigUint {
        &self.router_fee
    }

    /// Client fee amount.
    pub fn client_fee(&self) -> &BigUint {
        &self.client_fee
    }

    /// Maximum slippage amount.
    pub fn max_slippage(&self) -> &BigUint {
        &self.max_slippage
    }

    /// Minimum amount the user receives on-chain.
    pub fn min_amount_received(&self) -> &BigUint {
        &self.min_amount_received
    }
}

/// Options to customize the encoding behavior.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodingOptions {
    slippage: f64,
    /// Token transfer method. Defaults to `TransferFrom`.
    #[serde(default = "default_transfer_type")]
    transfer_type: UserTransferType,
    /// Permit2 single-token authorization. Required when using `TransferFromPermit2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permit: Option<PermitSingle>,
    /// Permit2 signature (65 bytes). Required when `permit` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permit2_signature: Option<Bytes>,
    /// Client fee configuration. When absent, no client fee is charged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_fee_params: Option<ClientFeeParams>,
}

impl EncodingOptions {
    /// Creates encoding options with the given slippage and default transfer type.
    pub fn new(slippage: f64) -> Self {
        Self {
            slippage,
            transfer_type: default_transfer_type(),
            permit: None,
            permit2_signature: None,
            client_fee_params: None,
        }
    }

    /// Sets the token transfer type.
    pub fn with_transfer_type(mut self, transfer_type: UserTransferType) -> Self {
        self.transfer_type = transfer_type;
        self
    }

    /// Sets the permit2 single-token authorization.
    pub fn with_permit(mut self, permit: PermitSingle) -> Self {
        self.permit = Some(permit);
        self
    }

    /// Sets the permit2 signature.
    pub fn with_signature(mut self, sig: Bytes) -> Self {
        self.permit2_signature = Some(sig);
        self
    }

    /// Returns the slippage tolerance.
    pub fn slippage(&self) -> f64 {
        self.slippage
    }

    /// Returns the token transfer type.
    pub fn transfer_type(&self) -> &UserTransferType {
        &self.transfer_type
    }

    /// Returns the permit2 authorization, if set.
    pub fn permit(&self) -> Option<&PermitSingle> {
        self.permit.as_ref()
    }

    /// Returns the permit2 signature, if set.
    pub fn permit2_signature(&self) -> Option<&Bytes> {
        self.permit2_signature.as_ref()
    }

    /// Sets the client fee params.
    pub fn with_client_fee_params(mut self, params: ClientFeeParams) -> Self {
        self.client_fee_params = Some(params);
        self
    }

    /// Returns the client fee params, if set.
    pub fn client_fee_params(&self) -> Option<&ClientFeeParams> {
        self.client_fee_params.as_ref()
    }
}

/// A single permit for permit2 token transfer authorization.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermitSingle {
    /// The permit details (token, amount, expiration, nonce).
    details: PermitDetails,
    /// Address authorized to spend the tokens (typically the router).
    spender: Bytes,
    /// Deadline timestamp for the permit signature.
    #[serde_as(as = "DisplayFromStr")]
    sig_deadline: BigUint,
}

impl PermitSingle {
    /// Creates a new permit.
    pub fn new(details: PermitDetails, spender: Bytes, sig_deadline: BigUint) -> Self {
        Self { details, spender, sig_deadline }
    }

    /// Returns the permit details.
    pub fn details(&self) -> &PermitDetails {
        &self.details
    }

    /// Returns the spender address.
    pub fn spender(&self) -> &Bytes {
        &self.spender
    }

    /// Returns the signature deadline.
    pub fn sig_deadline(&self) -> &BigUint {
        &self.sig_deadline
    }
}

/// Details for a permit2 single-token permit.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermitDetails {
    /// Token address for which the permit is granted.
    token: Bytes,
    /// Amount of tokens approved.
    #[serde_as(as = "DisplayFromStr")]
    amount: BigUint,
    /// Expiration timestamp for the permit.
    #[serde_as(as = "DisplayFromStr")]
    expiration: BigUint,
    /// Nonce to prevent replay attacks.
    #[serde_as(as = "DisplayFromStr")]
    nonce: BigUint,
}

impl PermitDetails {
    /// Creates permit details.
    pub fn new(token: Bytes, amount: BigUint, expiration: BigUint, nonce: BigUint) -> Self {
        Self { token, amount, expiration, nonce }
    }

    /// Returns the token address.
    pub fn token(&self) -> &Bytes {
        &self.token
    }

    /// Returns the approved amount.
    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    /// Returns the expiration timestamp.
    pub fn expiration(&self) -> &BigUint {
        &self.expiration
    }

    /// Returns the nonce.
    pub fn nonce(&self) -> &BigUint {
        &self.nonce
    }
}

// ============================================================================
// RESPONSE TYPES
// ============================================================================

/// Complete solution for a [`QuoteRequest`].
///
/// Contains a solution for each order in the request, along with aggregate
/// gas estimates and timing information.
#[must_use]
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    /// Quotes for each order, in the same order as the request.
    orders: Vec<OrderQuote>,
    /// Total estimated gas for executing all swaps (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    total_gas_estimate: BigUint,
    /// Time taken to compute this solution, in milliseconds.
    solve_time_ms: u64,
}

impl Quote {
    pub(crate) fn new(
        orders: Vec<OrderQuote>,
        total_gas_estimate: BigUint,
        solve_time_ms: u64,
    ) -> Self {
        Self { orders, total_gas_estimate, solve_time_ms }
    }

    /// Returns the solutions for each order.
    pub fn orders(&self) -> &[OrderQuote] {
        &self.orders
    }

    /// Consumes this solution and returns the order solutions.
    pub fn into_orders(self) -> Vec<OrderQuote> {
        self.orders
    }

    /// Returns the total estimated gas for all swaps.
    pub fn total_gas_estimate(&self) -> &BigUint {
        &self.total_gas_estimate
    }

    /// Returns the time taken to compute this solution, in milliseconds.
    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }
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
    id: String,
    /// Input token address (the token being sold).
    token_in: Address,
    /// Output token address (the token being bought).
    token_out: Address,
    /// Amount to swap, interpreted according to `side` (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    amount: BigUint,
    /// Whether this is a sell (exact input) or buy (exact output) order.
    side: OrderSide,
    /// Address that will send the input tokens.
    sender: Address,
    /// Address that will receive the output tokens.
    ///
    /// Defaults to `sender` if not specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receiver: Option<Address>,
}

impl Order {
    /// Creates a new order with an auto-generated ID.
    pub fn new(
        token_in: Address,
        token_out: Address,
        amount: BigUint,
        side: OrderSide,
        sender: Address,
    ) -> Self {
        Self { id: generate_order_id(), token_in, token_out, amount, side, sender, receiver: None }
    }

    /// Overrides the auto-generated order ID.
    pub fn with_id(mut self, id: String) -> Self {
        self.id = id;
        self
    }

    /// Sets the receiver address.
    pub fn with_receiver(mut self, receiver: Address) -> Self {
        self.receiver = Some(receiver);
        self
    }

    /// Returns the order ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the input token address.
    pub fn token_in(&self) -> &Address {
        &self.token_in
    }

    /// Returns the output token address.
    pub fn token_out(&self) -> &Address {
        &self.token_out
    }

    /// Returns the swap amount.
    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    /// Returns the order side.
    pub fn side(&self) -> OrderSide {
        self.side
    }

    /// Returns the sender address.
    pub fn sender(&self) -> &Address {
        &self.sender
    }

    /// Returns the receiver address, if set.
    pub fn receiver(&self) -> Option<&Address> {
        self.receiver.as_ref()
    }

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
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum OrderValidationError {
    #[error("token_in and token_out must be different")]
    SameTokens,
    #[error("amount must be non-zero")]
    ZeroAmount,
}

/// Internal wrapper used by workers when returning a solution.
///
/// This wraps [`OrderQuote`] with per-worker timing information.
/// The `solve_time_ms` here is the time taken by an individual worker/algorithm,
/// not the total WorkerPoolRouter orchestration time (which is in [`Quote`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleOrderQuote {
    /// The solution for the order.
    order: OrderQuote,
    /// Time taken by this specific worker to compute the solution, in milliseconds.
    solve_time_ms: u64,
}

impl SingleOrderQuote {
    pub(crate) fn new(order: OrderQuote, solve_time_ms: u64) -> Self {
        Self { order, solve_time_ms }
    }

    /// Returns the order solution.
    pub fn order(&self) -> &OrderQuote {
        &self.order
    }

    /// Returns the time taken by this worker to compute the solution, in milliseconds.
    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }
}

/// Quote for a single [`Order`].
///
/// Contains the route to execute (if found), along with expected amounts,
/// gas estimates, and status information.
#[must_use]
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderQuote {
    /// ID of the order this solution corresponds to.
    order_id: String,
    /// Status indicating whether a route was found.
    status: QuoteStatus,
    /// The route to execute, if a valid route was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<Route>,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    amount_out: BigUint,
    /// Estimated gas cost for executing this route (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    gas_estimate: BigUint,
    /// Price impact in basis points (1 bip = 0.01%).
    #[serde(skip_serializing_if = "Option::is_none")]
    price_impact_bps: Option<i32>,
    /// Amount out minus gas cost in output token terms.
    /// Used by WorkerPoolRouter to compare solutions from different solvers.
    #[serde_as(as = "DisplayFromStr")]
    amount_out_net_gas: BigUint,
    /// Block at which this quote was computed.
    block: BlockInfo,
    /// Algorithm that found this solution (internal use only).
    #[serde(skip)]
    algorithm: String,
    /// Effective gas price (in wei) at the time the route was computed.
    #[serde_as(as = "Option<DisplayFromStr>")]
    gas_price: Option<BigUint>,
    /// An encoded EVM transaction ready to be submitted on-chain.
    transaction: Option<Transaction>,
    /// Fee breakdown (populated when encoding options are provided).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_breakdown: Option<FeeBreakdown>,
    /// Address of the sender.
    sender: Bytes,
    /// Address of the receiver.
    receiver: Bytes,
}

impl OrderQuote {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        order_id: String,
        status: QuoteStatus,
        amount_in: BigUint,
        amount_out: BigUint,
        gas_estimate: BigUint,
        amount_out_net_gas: BigUint,
        block: BlockInfo,
        algorithm: String,
        sender: Bytes,
        receiver: Bytes,
    ) -> Self {
        Self {
            order_id,
            status,
            route: None,
            amount_in,
            amount_out,
            gas_estimate,
            price_impact_bps: None,
            amount_out_net_gas,
            block,
            algorithm,
            gas_price: None,
            transaction: None,
            fee_breakdown: None,
            sender,
            receiver,
        }
    }

    /// Sets the status of this quote.
    pub(crate) fn set_status(&mut self, status: QuoteStatus) {
        self.status = status;
    }

    /// Sets the route for this solution.
    pub(crate) fn with_route(mut self, route: Route) -> Self {
        self.route = Some(route);
        self
    }

    /// Sets the effective gas price.
    pub(crate) fn with_gas_price(mut self, gas_price: BigUint) -> Self {
        self.gas_price = Some(gas_price);
        self
    }

    /// Sets the price impact in basis points.
    #[allow(dead_code)]
    pub(crate) fn with_price_impact_bps(mut self, bps: i32) -> Self {
        self.price_impact_bps = Some(bps);
        self
    }

    /// Sets the encoded EVM transaction in place.
    pub fn set_transaction(&mut self, transaction: Transaction) {
        self.transaction = Some(transaction);
    }

    /// Returns the order ID.
    pub fn order_id(&self) -> &str {
        &self.order_id
    }

    /// Returns the solution status.
    pub fn status(&self) -> QuoteStatus {
        self.status
    }

    /// Returns the route, if a valid route was found.
    pub fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// Consumes this solution and returns the route.
    pub fn into_route(self) -> Option<Route> {
        self.route
    }

    /// Returns the input amount.
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// Returns the output amount.
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Returns the estimated gas cost.
    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    /// Returns the price impact in basis points, if available.
    pub fn price_impact_bps(&self) -> Option<i32> {
        self.price_impact_bps
    }

    /// Returns the output amount minus gas cost in output token terms.
    pub fn amount_out_net_gas(&self) -> &BigUint {
        &self.amount_out_net_gas
    }

    /// Returns the block at which this solution was computed.
    pub fn block(&self) -> &BlockInfo {
        &self.block
    }

    /// Returns the algorithm name that found this solution.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the effective gas price at the time the route was computed.
    pub fn gas_price(&self) -> Option<&BigUint> {
        self.gas_price.as_ref()
    }

    /// Returns the encoded EVM transaction, if available.
    pub fn transaction(&self) -> Option<&Transaction> {
        self.transaction.as_ref()
    }

    /// Returns the fee breakdown, if encoding was requested.
    pub fn fee_breakdown(&self) -> Option<&FeeBreakdown> {
        self.fee_breakdown.as_ref()
    }

    /// Sets the fee breakdown in place.
    pub fn set_fee_breakdown(&mut self, fb: FeeBreakdown) {
        self.fee_breakdown = Some(fb);
    }

    /// Returns the sender address.
    pub fn sender(&self) -> &Bytes {
        &self.sender
    }

    /// Returns the receiver address.
    pub fn receiver(&self) -> &Bytes {
        &self.receiver
    }
}

/// Status of an order solution.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteStatus {
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
    /// The solution failed external price validation.
    PriceCheckFailed,
}

impl From<AlgorithmError> for QuoteStatus {
    fn from(err: crate::algorithm::AlgorithmError) -> Self {
        match err {
            AlgorithmError::NoPath { .. } => QuoteStatus::NoRouteFound,
            AlgorithmError::InsufficientLiquidity => QuoteStatus::InsufficientLiquidity,
            AlgorithmError::Timeout { .. } => QuoteStatus::Timeout,
            AlgorithmError::ExactOutNotSupported => QuoteStatus::NoRouteFound,
            AlgorithmError::Other(_) => QuoteStatus::NoRouteFound,
            AlgorithmError::InvalidConfiguration { .. } => QuoteStatus::NoRouteFound,
            AlgorithmError::SimulationFailed { .. } => QuoteStatus::NoRouteFound,
            AlgorithmError::DataNotFound { .. } => QuoteStatus::NoRouteFound,
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
    number: u64,
    /// Block hash as a hex string.
    hash: String,
    /// Block timestamp in Unix seconds.
    timestamp: u64,
}

impl BlockInfo {
    /// Creates new block info.
    pub fn new(number: u64, hash: String, timestamp: u64) -> Self {
        Self { number, hash, timestamp }
    }

    /// Returns the block number.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// Returns the block hash.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Returns the block timestamp in Unix seconds.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }
}

// ============================================================================
// ROUTE & SWAP TYPES
// ============================================================================

/// A route consisting of one or more sequential swaps.
///
/// A route describes the path through liquidity pools to execute a swap.
/// For multi-hop swaps, the output of each swap becomes the input of the next.
#[must_use]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Ordered sequence of swaps to execute.
    swaps: Vec<Swap>,
}

impl Route {
    /// Creates a new route from an ordered sequence of swaps.
    pub fn new(swaps: Vec<Swap>) -> Self {
        Self { swaps }
    }

    /// Returns the swaps in this route.
    pub fn swaps(&self) -> &[Swap] {
        &self.swaps
    }

    /// Consumes the route and returns its swaps.
    pub fn into_swaps(self) -> Vec<Swap> {
        self.swaps
    }
}

/// The result of a route-finding algorithm: a route plus its gas-adjusted net output.
///
/// `net_amount_out` is the output amount after subtracting gas costs converted to output token
/// units. It can be negative if gas exceeds output (e.g., tiny swaps or inaccurate gas
/// estimation). Used by the worker to populate `amount_out_net_gas` on `OrderQuote`.
#[derive(Debug, Clone)]
pub struct RouteResult {
    /// The route (sequence of swaps) to execute.
    route: Route,
    /// Net amount out after accounting for gas costs in output token terms.
    net_amount_out: BigInt,
    /// Effective gas price (in wei) at the time the route was computed.
    gas_price: BigUint,
}

impl RouteResult {
    /// Creates a new route result.
    pub fn new(route: Route, net_amount_out: BigInt, gas_price: BigUint) -> Self {
        Self { route, net_amount_out, gas_price }
    }

    pub(crate) fn route(&self) -> &Route {
        &self.route
    }

    pub(crate) fn into_route(self) -> Route {
        self.route
    }

    pub(crate) fn net_amount_out(&self) -> &BigInt {
        &self.net_amount_out
    }

    pub(crate) fn gas_price(&self) -> &BigUint {
        &self.gas_price
    }
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
    /// - A token appears more than once in the path unless it is both the first and last token
    ///   (Tycho execution only supports this type of cycle)
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

        // Collect every token in the path:
        // A token may only repeat if it is both the first and last in the path.
        let first_token = &self.swaps[0].token_in;
        let last_token = &self.swaps[self.swaps.len() - 1].token_out;

        let mut seen = HashSet::new();
        seen.insert(first_token.clone());
        let last_idx = self.swaps.len() - 1;
        for (i, swap) in self.swaps.iter().enumerate() {
            if !seen.insert(swap.token_out.clone()) {
                // Duplicate token — only allowed if it's the last token
                // matching the first (simple round-trip cycle).
                if !(i == last_idx && swap.token_out == *first_token) {
                    return Err(RouteValidationError::UnsupportedCycle {
                        token: swap.token_out.clone(),
                        first: first_token.clone(),
                        last: last_token.clone(),
                    });
                }
            }
        }

        Ok(())
    }
}

/// Errors that can occur when validating a [`Route`].
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum RouteValidationError {
    #[error("route must contain at least one swap")]
    EmptyRoute,
    #[non_exhaustive]
    #[error("swaps are not connected: {first_out} != {second_in}")]
    DisconnectedSwaps { first_out: Address, second_in: Address },
    #[non_exhaustive]
    #[error(
        "unsupported cycle: token {token} appears more than once in route \
         {first} -> ... -> {last} (only first == last cycles are supported)"
    )]
    UnsupportedCycle { token: Address, first: Address, last: Address },
}

/// A single swap within a route.
///
/// Represents an atomic swap on a specific liquidity pool (component).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swap {
    /// Identifier of the liquidity pool component.
    component_id: ComponentId,
    /// Protocol system identifier (e.g., "uniswap_v2", "uniswap_v3", "vm:balancer").
    protocol: String,
    /// Input token address.
    token_in: Address,
    /// Output token address.
    token_out: Address,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    amount_out: BigUint,
    /// Estimated gas cost for this swap (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    gas_estimate: BigUint,
    /// Protocol component to perform the swap.
    protocol_component: ProtocolComponent,
    /// Protocol state used to perform the swap.
    protocol_state: Box<dyn ProtocolSim>,
    /// Decimal of the amount to be swapped in this operation (for example, 0.5 means 50%)
    #[serde_as(as = "DisplayFromStr")]
    split: f64,
}

impl Swap {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
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
            split: 0.0,
        }
    }
    /// Sets the split of this Swap
    #[allow(dead_code)]
    pub(crate) fn with_split(mut self, split: f64) -> Self {
        self.split = split;
        self
    }

    /// Returns the component ID of the liquidity pool.
    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// Returns the protocol identifier.
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// Returns the input token address.
    pub fn token_in(&self) -> &Address {
        &self.token_in
    }

    /// Returns the output token address.
    pub fn token_out(&self) -> &Address {
        &self.token_out
    }

    /// Returns the input amount.
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// Returns the output amount.
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Returns the estimated gas cost for this swap.
    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    /// Returns the protocol component.
    pub fn protocol_component(&self) -> &ProtocolComponent {
        &self.protocol_component
    }

    /// Returns the protocol state.
    pub fn protocol_state(&self) -> &dyn ProtocolSim {
        self.protocol_state.as_ref()
    }

    /// Returns the split of this swap.
    pub fn split(&self) -> &f64 {
        &self.split
    }
}

/// An encoded EVM transaction ready to be submitted on-chain.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// Contract address to call.
    to: Bytes,
    /// Native token value to send with the transaction.
    #[serde_as(as = "DisplayFromStr")]
    value: num_bigint::BigUint,
    /// ABI-encoded calldata.
    data: Vec<u8>,
}

impl Transaction {
    /// Creates a new transaction.
    pub fn new(to: Bytes, value: BigUint, data: Vec<u8>) -> Self {
        Self { to, value, data }
    }

    /// Returns the contract address to call.
    pub fn to(&self) -> &Bytes {
        &self.to
    }

    /// Returns the native token value to send.
    pub fn value(&self) -> &num_bigint::BigUint {
        &self.value
    }

    /// Returns the ABI-encoded calldata.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
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
        Order::new(
            make_address(token_in_byte),
            make_address(token_out_byte),
            BigUint::from(amount),
            OrderSide::Sell,
            make_address(0xAA),
        )
    }

    fn make_swap(token_in_byte: u8, token_out_byte: u8, amount_in: u64, amount_out: u64) -> Swap {
        let token_in = token(token_in_byte, "TIN");
        let token_out = token(token_out_byte, "TOUT");
        Swap::new(
            "pool-1".to_string(),
            "uniswap_v2".to_string(),
            make_address(token_in_byte),
            make_address(token_out_byte),
            BigUint::from(amount_in),
            BigUint::from(amount_out),
            BigUint::from(100_000u64),
            component("test-pool", &[token_in, token_out]),
            Box::new(MockProtocolSim::default()),
        )
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
        let order = make_order(0x01, 0x02, 1000);
        let order = match receiver {
            Some(b) => order.with_receiver(make_address(b)),
            None => order,
        };
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
    #[case::valid_first_last_cycle(vec![(0x01, 0x02), (0x02, 0x01)], true, None)]
    #[case::empty(vec![], false, Some("EmptyRoute"))]
    #[case::disconnected(vec![(0x01, 0x02), (0x03, 0x04)], false, Some("DisconnectedSwaps"))]
    #[case::unsupported_intermediate_cycle(
        vec![(0x01, 0x02), (0x02, 0x03), (0x03, 0x02)],
        false,
        Some("UnsupportedCycle")
    )]
    #[case::unsupported_mid_path_cycle(
        vec![(0x01, 0x02), (0x02, 0x01), (0x01, 0x03)],
        false,
        Some("UnsupportedCycle")
    )]
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
                "UnsupportedCycle" => {
                    assert!(matches!(err, RouteValidationError::UnsupportedCycle { .. }))
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
            "split": "0",
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

        let order = Order::new(
            make_address(0x01),
            make_address(0x02),
            large_amount.clone(),
            OrderSide::Sell,
            make_address(0xAA),
        );

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
        let solution = Quote::new(vec![], BigUint::from(500_000u64), 10);

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

    // -------------------------------------------------------------------------
    // ClientFeeParams & EncodingOptions Tests
    // -------------------------------------------------------------------------

    fn make_client_fee_params() -> ClientFeeParams {
        ClientFeeParams::new(
            100,
            Bytes::from(make_address(0xBB).as_ref()),
            BigUint::from(500_000u64),
            1_893_456_000u64,
            Bytes::from(vec![0xAB; 65]),
        )
    }

    #[test]
    fn test_client_fee_params_serde_roundtrip() {
        let fee = make_client_fee_params();
        let json = serde_json::to_string(&fee).unwrap();

        assert!(json.contains(r#""max_contribution":"500000""#));
        assert!(json.contains(r#""deadline":1893456000"#));

        let deserialized: ClientFeeParams = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.bps(), fee.bps());
        assert_eq!(*deserialized.max_contribution(), *fee.max_contribution());
        assert_eq!(deserialized.deadline(), fee.deadline());
    }

    #[test]
    fn test_encoding_options_serde_with_client_fee() {
        let fee = make_client_fee_params();
        let opts = EncodingOptions::new(0.005).with_client_fee_params(fee);
        let json = serde_json::to_string(&opts).unwrap();

        let deserialized: EncodingOptions = serde_json::from_str(&json).unwrap();
        assert!(deserialized
            .client_fee_params()
            .is_some());
        assert_eq!(
            deserialized
                .client_fee_params()
                .unwrap()
                .bps(),
            100
        );
        assert!((deserialized.slippage() - 0.005).abs() < f64::EPSILON);
    }

    #[test]
    fn test_encoding_options_serde_without_client_fee() {
        let opts = EncodingOptions::new(0.01);
        let json = serde_json::to_string(&opts).unwrap();

        assert!(!json.contains("client_fee_params"));

        let deserialized: EncodingOptions = serde_json::from_str(&json).unwrap();
        assert!(deserialized
            .client_fee_params()
            .is_none());
    }
}
