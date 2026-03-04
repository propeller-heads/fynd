//! Data Transfer Objects (DTOs) for the HTTP API.
//!
//! These types mirror the core domain types but include OpenAPI annotations
//! for API documentation. They are converted to/from core types at the API boundary.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tycho_simulation::{
    tycho_common::models::Address,
    tycho_core::{dto::ProtocolComponent, simulation::protocol_sim::ProtocolSim, Bytes},
};
use utoipa::ToSchema;
use uuid::Uuid;

// ============================================================================
// REQUEST TYPES
// ============================================================================

/// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SolutionRequest {
    /// Orders to solve.
    pub orders: Vec<Order>,
    /// Optional solving parameters that apply to all orders.
    #[serde(default)]
    pub options: SolutionOptions,
}

/// Options to customize the solving behavior.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct SolutionOptions {
    /// Timeout in milliseconds. If `None`, uses server default.
    #[schema(example = 2000)]
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
    #[schema(value_type = Option<String>, example = "500000")]
    pub max_gas: Option<BigUint>,
    pub encoding_options: Option<EncodingOptions>,
}

/// Token transfer method for moving funds into Tycho execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserTransferType {
    /// Use Permit2 for token transfer. Requires `permit` and `signature`.
    TransferFromPermit2,
    /// Use standard ERC-20 approval and `transferFrom`. Default.
    #[default]
    TransferFrom,
    /// Use funds already present in the Tycho Router (no transfer performed).
    None,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EncodingOptions {
    #[serde_as(as = "DisplayFromStr")]
    #[schema(example = "0.001")]
    pub slippage: f64,
    /// Token transfer method. Defaults to `transfer_from`.
    #[serde(default)]
    pub transfer_type: UserTransferType,
    /// Permit2 single-token authorization. Required when using `transfer_from_permit2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit: Option<PermitSingle>,
    /// Permit2 signature (65 bytes, hex-encoded). Required when `permit` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "0xabcd...")]
    pub signature: Option<Bytes>,
}

/// A single permit for permit2 token transfer authorization.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PermitSingle {
    /// The permit details (token, amount, expiration, nonce).
    pub details: PermitDetails,
    /// Address authorized to spend the tokens (typically the router).
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub spender: Bytes,
    /// Deadline timestamp for the permit signature.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1893456000")]
    pub sig_deadline: BigUint,
}

/// Details for a permit2 single-token permit.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PermitDetails {
    /// Token address for which the permit is granted.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub token: Bytes,
    /// Amount of tokens approved.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1000000000000000000")]
    pub amount: BigUint,
    /// Expiration timestamp for the permit.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1893456000")]
    pub expiration: BigUint,
    /// Nonce to prevent replay attacks.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "0")]
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Solution {
    /// Solutions for each order, in the same order as the request.
    pub orders: Vec<OrderSolution>,
    /// Total estimated gas for executing all swaps (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "150000")]
    pub total_gas_estimate: BigUint,
    /// Time taken to compute this solution, in milliseconds.
    #[schema(example = 12)]
    pub solve_time_ms: u64,
}

/// A single swap order to be solved.
///
/// An order specifies an intent to swap one token for another.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Order {
    /// Unique identifier for this order.
    ///
    /// Auto-generated by the API.
    #[serde(default = "generate_order_id", skip_deserializing)]
    pub id: String,
    /// Input token address (the token being sold).
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub token_in: Address,
    /// Output token address (the token being bought).
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token_out: Address,
    /// Amount to swap, interpreted according to `side` (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1000000000000000000")]
    pub amount: BigUint,
    /// Whether this is a sell (exact input) or buy (exact output) order.
    pub side: OrderSide,
    /// Address that will send the input tokens.
    #[schema(value_type = String, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")]
    pub sender: Address,
    /// Address that will receive the output tokens.
    ///
    /// Defaults to `sender` if not specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")]
    pub receiver: Option<Address>,
}

/// Specifies the side of an order: sell (exact input) or buy (exact output).
///
/// Currently only `Sell` is supported. `Buy` will be added in a future version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    /// Sell exactly the specified amount of the input token.
    Sell,
}

/// Internal wrapper used by workers when returning a solution.
///
/// This wraps [`OrderSolution`] with per-worker timing information.
/// The `solve_time_ms` here is the time taken by an individual worker/algorithm,
/// not the total OrderManager orchestration time (which is in [`Solution`]).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrderSolution {
    /// ID of the order this solution corresponds to.
    #[schema(example = "f47ac10b-58cc-4372-a567-0e02b2c3d479")]
    pub order_id: String,
    /// Status indicating whether a route was found.
    pub status: SolutionStatus,
    /// The route to execute, if a valid route was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1000000000000000000")]
    pub amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "3500000000")]
    pub amount_out: BigUint,
    /// Estimated gas cost for executing this route (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "150000")]
    pub gas_estimate: BigUint,
    /// Price impact in basis points (1 bip = 0.01%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_impact_bps: Option<i32>,
    /// Amount out minus gas cost in output token terms.
    /// Used by OrderManager to compare solutions from different solvers.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "3498000000")]
    pub amount_out_net_gas: BigUint,
    /// Block at which this quote was computed.
    pub block: BlockInfo,
    /// Effective gas price (in wei) at the time the route was computed.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "20000000000")]
    pub gas_price: Option<BigUint>,
    pub transaction: Option<Transaction>,
}

/// Status of an order solution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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

/// Block information at which a quote was computed.
///
/// Quotes are only valid for the block at which they were computed. Market
/// conditions may change in subsequent blocks.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BlockInfo {
    /// Block number.
    #[schema(example = 21000000)]
    pub number: u64,
    /// Block hash as a hex string.
    #[schema(example = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd")]
    pub hash: String,
    /// Block timestamp in Unix seconds.
    #[schema(example = 1730000000)]
    pub timestamp: u64,
}

// ============================================================================
// ROUTE & SWAP TYPES
// ============================================================================

/// A route consisting of one or more sequential swaps.
///
/// A route describes the path through liquidity pools to execute a swap.
/// For multi-hop swaps, the output of each swap becomes the input of the next.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Route {
    /// Ordered sequence of swaps to execute.
    pub swaps: Vec<Swap>,
}

/// A single swap within a route.
///
/// Represents an atomic swap on a specific liquidity pool (component).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Swap {
    /// Identifier of the liquidity pool component.
    #[schema(example = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")]
    pub component_id: String,
    /// Protocol system identifier (e.g., "uniswap_v2", "uniswap_v3", "vm:balancer").
    #[schema(example = "uniswap_v2")]
    pub protocol: String,
    /// Input token address.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub token_in: Address,
    /// Output token address.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token_out: Address,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1000000000000000000")]
    pub amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "3500000000")]
    pub amount_out: BigUint,
    /// Estimated gas cost for this swap (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "150000")]
    pub gas_estimate: BigUint,
    #[schema(value_type = Object)]
    pub protocol_component: ProtocolComponent,
    #[schema(value_type = Object)]
    pub protocol_state: Box<dyn ProtocolSim>,
}

// ============================================================================
// ENCODING TYPES
// ============================================================================

/// An encoded EVM transaction ready to be submitted on-chain.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Transaction {
    /// Contract address to call.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub to: Bytes,

    /// Native token value to send with the transaction (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "0")]
    pub value: BigUint,

    /// ABI-encoded calldata as hex string.
    #[schema(value_type = String, example = "0x1234567890abcdef")]
    #[serde(serialize_with = "serialize_bytes_hex", deserialize_with = "deserialize_bytes_hex")]
    pub data: Vec<u8>,
}

// ============================================================================
// CUSTOM SERIALIZATION
// ============================================================================

/// Serializes Vec<u8> to hex string with 0x prefix.
fn serialize_bytes_hex<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&format!("0x{}", hex::encode(bytes)))
}

/// Deserializes hex string (with or without 0x prefix) to Vec<u8>.
fn deserialize_bytes_hex<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let s = s.strip_prefix("0x").unwrap_or(&s);
    hex::decode(s).map_err(serde::de::Error::custom)
}

// ============================================================================
// CONVERSIONS: Transaction DTO <-> Core
// ============================================================================

impl From<fynd_core::types::Transaction> for Transaction {
    fn from(core: fynd_core::Transaction) -> Self {
        Self { to: core.to, value: core.value, data: core.data }
    }
}

// ============================================================================
// HEALTH CHECK TYPES
// ============================================================================

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthStatus {
    /// Whether the service is healthy.
    #[schema(example = true)]
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    #[schema(example = 1250)]
    pub last_update_ms: u64,
    /// Number of active solver pools.
    #[schema(example = 2)]
    pub num_solver_pools: usize,
}

// ============================================================================
// CONVERSIONS: DTO -> Core
// ============================================================================

impl From<SolutionRequest> for fynd_core::SolutionRequest {
    fn from(dto: SolutionRequest) -> Self {
        Self {
            orders: dto
                .orders
                .into_iter()
                .map(Into::into)
                .collect(),
            options: dto.options.into(),
        }
    }
}

impl From<SolutionOptions> for fynd_core::SolutionOptions {
    fn from(dto: SolutionOptions) -> Self {
        Self {
            timeout_ms: dto.timeout_ms,
            min_responses: dto.min_responses,
            max_gas: dto.max_gas,
            encoding_options: dto.encoding_options.map(Into::into),
        }
    }
}

impl From<UserTransferType> for fynd_core::UserTransferType {
    fn from(dto: UserTransferType) -> Self {
        match dto {
            UserTransferType::TransferFromPermit2 => Self::TransferFromPermit2,
            UserTransferType::TransferFrom => Self::TransferFrom,
            UserTransferType::None => Self::None,
        }
    }
}

impl From<EncodingOptions> for fynd_core::EncodingOptions {
    fn from(dto: EncodingOptions) -> Self {
        Self {
            slippage: dto.slippage,
            transfer_type: dto.transfer_type.into(),
            permit: dto.permit.map(Into::into),
            signature: dto.signature,
        }
    }
}

impl From<PermitSingle> for fynd_core::PermitSingle {
    fn from(dto: PermitSingle) -> Self {
        Self { details: dto.details.into(), spender: dto.spender, sig_deadline: dto.sig_deadline }
    }
}

impl From<PermitDetails> for fynd_core::PermitDetails {
    fn from(dto: PermitDetails) -> Self {
        Self { token: dto.token, amount: dto.amount, expiration: dto.expiration, nonce: dto.nonce }
    }
}

impl From<Order> for fynd_core::Order {
    fn from(dto: Order) -> Self {
        Self {
            id: dto.id,
            token_in: dto.token_in,
            token_out: dto.token_out,
            amount: dto.amount,
            side: dto.side.into(),
            sender: dto.sender,
            receiver: dto.receiver,
        }
    }
}

impl From<OrderSide> for fynd_core::OrderSide {
    fn from(dto: OrderSide) -> Self {
        match dto {
            OrderSide::Sell => Self::Sell,
        }
    }
}

// ============================================================================
// CONVERSIONS: Core -> DTO
// ============================================================================

impl From<fynd_core::Solution> for Solution {
    fn from(core: fynd_core::Solution) -> Self {
        Self {
            orders: core
                .orders
                .into_iter()
                .map(Into::into)
                .collect(),
            total_gas_estimate: core.total_gas_estimate,
            solve_time_ms: core.solve_time_ms,
        }
    }
}

impl From<fynd_core::OrderSolution> for OrderSolution {
    fn from(core: fynd_core::OrderSolution) -> Self {
        Self {
            order_id: core.order_id,
            status: core.status.into(),
            route: core.route.map(Into::into),
            amount_in: core.amount_in,
            amount_out: core.amount_out,
            gas_estimate: core.gas_estimate,
            price_impact_bps: core.price_impact_bps,
            amount_out_net_gas: core.amount_out_net_gas,
            block: core.block.into(),
            gas_price: core.gas_price,
            transaction: core.transaction.map(Into::into),
        }
    }
}

impl From<fynd_core::SolutionStatus> for SolutionStatus {
    fn from(core: fynd_core::SolutionStatus) -> Self {
        match core {
            fynd_core::SolutionStatus::Success => Self::Success,
            fynd_core::SolutionStatus::NoRouteFound => Self::NoRouteFound,
            fynd_core::SolutionStatus::InsufficientLiquidity => Self::InsufficientLiquidity,
            fynd_core::SolutionStatus::Timeout => Self::Timeout,
            fynd_core::SolutionStatus::NotReady => Self::NotReady,
        }
    }
}

impl From<fynd_core::BlockInfo> for BlockInfo {
    fn from(core: fynd_core::BlockInfo) -> Self {
        Self { number: core.number, hash: core.hash, timestamp: core.timestamp }
    }
}

impl From<fynd_core::Route> for Route {
    fn from(core: fynd_core::Route) -> Self {
        Self {
            swaps: core
                .swaps
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<fynd_core::Swap> for Swap {
    fn from(core: fynd_core::Swap) -> Self {
        Self {
            component_id: core.component_id,
            protocol: core.protocol,
            token_in: core.token_in,
            token_out: core.token_out,
            amount_in: core.amount_in,
            amount_out: core.amount_out,
            gas_estimate: core.gas_estimate,
            protocol_component: core.protocol_component.into(),
            protocol_state: core.protocol_state,
        }
    }
}

// ============================================================================
// PRIVATE HELPERS
// ============================================================================

/// Generates a unique order ID using UUID v4.
fn generate_order_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn test_solution_request_conversion_roundtrip() {
        let dto = SolutionRequest {
            orders: vec![Order {
                id: "test-id".to_string(),
                token_in: make_address(0x01),
                token_out: make_address(0x02),
                amount: BigUint::from(1000u64),
                side: OrderSide::Sell,
                sender: make_address(0xAA),
                receiver: None,
            }],
            options: SolutionOptions {
                timeout_ms: Some(5000),
                min_responses: None,
                max_gas: None,
                encoding_options: None,
            },
        };

        let core: fynd_core::SolutionRequest = dto.clone().into();
        assert_eq!(core.orders.len(), 1);
        assert_eq!(core.orders[0].id, "test-id");
        assert_eq!(core.options.timeout_ms, Some(5000));
    }

    #[test]
    fn test_solution_conversion() {
        let core = fynd_core::Solution {
            orders: vec![],
            total_gas_estimate: BigUint::from(100_000u64),
            solve_time_ms: 50,
        };

        let dto: Solution = core.into();
        assert_eq!(dto.total_gas_estimate, BigUint::from(100_000u64));
        assert_eq!(dto.solve_time_ms, 50);
    }

    #[test]
    fn test_order_side_conversion() {
        let dto = OrderSide::Sell;
        let core: fynd_core::OrderSide = dto.into();
        assert_eq!(core, fynd_core::OrderSide::Sell);
    }

    #[test]
    fn test_status_conversion() {
        let statuses = vec![
            (fynd_core::SolutionStatus::Success, SolutionStatus::Success),
            (fynd_core::SolutionStatus::NoRouteFound, SolutionStatus::NoRouteFound),
            (
                fynd_core::SolutionStatus::InsufficientLiquidity,
                SolutionStatus::InsufficientLiquidity,
            ),
            (fynd_core::SolutionStatus::Timeout, SolutionStatus::Timeout),
            (fynd_core::SolutionStatus::NotReady, SolutionStatus::NotReady),
        ];

        for (core, expected_dto) in statuses {
            let dto: SolutionStatus = core.into();
            assert_eq!(dto, expected_dto);
        }
    }
}
