//! Data Transfer Objects (DTOs) for the Fynd RPC HTTP API.
//!
//! This crate contains only the wire-format types shared between the Fynd RPC server
//! (`fynd-rpc`) and its clients (`fynd-client`). It has no server-side infrastructure
//! dependencies (no actix-web, no server logic).
//!
//! Enable the `openapi` feature to derive `utoipa::ToSchema` on all types for use in
//! API documentation generation.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tycho_simulation::{tycho_common::models::Address, tycho_core::Bytes};
use uuid::Uuid;

// ============================================================================
// REQUEST TYPES
// ============================================================================

/// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct QuoteRequest {
    /// Orders to solve.
    pub orders: Vec<Order>,
    /// Optional solving parameters that apply to all orders.
    #[serde(default)]
    pub options: QuoteOptions,
}

/// Options to customize the solving behavior.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct QuoteOptions {
    /// Timeout in milliseconds. If `None`, uses server default.
    #[cfg_attr(feature = "openapi", schema(example = 2000))]
    pub timeout_ms: Option<u64>,
    /// Minimum number of solver responses to wait for before returning.
    /// If `None` or `0`, waits for all solvers to respond (or timeout).
    ///
    /// Use the `/health` endpoint to check `num_solver_pools` before setting this value.
    /// Values exceeding the number of active solver pools are clamped internally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_responses: Option<usize>,
    /// Maximum gas cost allowed for a solution. Quotes exceeding this are filtered out.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, example = "500000"))]
    pub max_gas: Option<BigUint>,
    // Options during encoding. If None, quote will be returned without calldata
    pub encoding_options: Option<EncodingOptions>,
}

/// Token transfer method for moving funds into Tycho execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
/// Options to customize the encoding behavior.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EncodingOptions {
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(example = "0.001"))]
    pub slippage: f64,
    /// Token transfer method. Defaults to `transfer_from`.
    #[serde(default)]
    pub transfer_type: UserTransferType,
    /// Permit2 single-token authorization. Required when using `transfer_from_permit2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit: Option<PermitSingle>,
    /// Permit2 signature (65 bytes, hex-encoded). Required when `permit` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, example = "0xabcd..."))]
    pub permit2_signature: Option<Bytes>,
}
/// A single permit for permit2 token transfer authorization.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PermitSingle {
    /// The permit details (token, amount, expiration, nonce).
    pub details: PermitDetails,
    /// Address authorized to spend the tokens (typically the router).
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))]
    pub spender: Bytes,
    /// Deadline timestamp for the permit signature.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "1893456000"))]
    pub sig_deadline: BigUint,
}
/// Details for a permit2 single-token permit.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PermitDetails {
    /// Token address for which the permit is granted.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))]
    pub token: Bytes,
    /// Amount of tokens approved.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "1000000000000000000"))]
    pub amount: BigUint,
    /// Expiration timestamp for the permit.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "1893456000"))]
    pub expiration: BigUint,
    /// Nonce to prevent replay attacks.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0"))]
    pub nonce: BigUint,
}

// ============================================================================
// RESPONSE TYPES
// ============================================================================

/// Complete solution for a [`QuoteRequest`].
///
/// Contains a solution for each order in the request, along with aggregate
/// gas estimates and timing information.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Quote {
    /// Quotes for each order, in the same order as the request.
    pub orders: Vec<OrderQuote>,
    /// Total estimated gas for executing all swaps (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "150000"))]
    pub total_gas_estimate: BigUint,
    /// Time taken to compute this solution, in milliseconds.
    #[cfg_attr(feature = "openapi", schema(example = 12))]
    pub solve_time_ms: u64,
}

/// A single swap order to be solved.
///
/// An order specifies an intent to swap one token for another.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Order {
    /// Unique identifier for this order.
    ///
    /// Auto-generated by the API.
    #[serde(default = "generate_order_id", skip_deserializing)]
    pub id: String,
    /// Input token address (the token being sold).
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
    )]
    pub token_in: Address,
    /// Output token address (the token being bought).
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
    )]
    pub token_out: Address,
    /// Amount to swap, interpreted according to `side` (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "1000000000000000000")
    )]
    pub amount: BigUint,
    /// Whether this is a sell (exact input) or buy (exact output) order.
    pub side: OrderSide,
    /// Address that will send the input tokens.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
    )]
    pub sender: Address,
    /// Address that will receive the output tokens.
    ///
    /// Defaults to `sender` if not specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = Option<String>, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
    )]
    pub receiver: Option<Address>,
}

/// Specifies the side of an order: sell (exact input) or buy (exact output).
///
/// Currently only `Sell` is supported. `Buy` will be added in a future version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    /// Sell exactly the specified amount of the input token.
    Sell,
}

/// Internal wrapper used by workers when returning a solution.
///
/// This wraps [`OrderQuote`] with per-worker timing information.
/// The `solve_time_ms` here is the time taken by an individual worker/algorithm,
/// not the total WorkerPoolRouter orchestration time (which is in [`Quote`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SingleOrderQuote {
    /// The solution for the order.
    pub order: OrderQuote,
    /// Time taken by this specific worker to compute the solution, in milliseconds.
    pub solve_time_ms: u64,
}

/// Quote for a single [`Order`].
///
/// Contains the route to execute (if found), along with expected amounts,
/// gas estimates, and status information.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OrderQuote {
    /// ID of the order this solution corresponds to.
    #[cfg_attr(feature = "openapi", schema(example = "f47ac10b-58cc-4372-a567-0e02b2c3d479"))]
    pub order_id: String,
    /// Status indicating whether a route was found.
    pub status: QuoteStatus,
    /// The route to execute, if a valid route was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "1000000000000000000")
    )]
    pub amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3500000000"))]
    pub amount_out: BigUint,
    /// Estimated gas cost for executing this route (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "150000"))]
    pub gas_estimate: BigUint,
    /// Price impact in basis points (1 bip = 0.01%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_impact_bps: Option<i32>,
    /// Amount out minus gas cost in output token terms.
    /// Used by WorkerPoolRouter to compare solutions from different solvers.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3498000000"))]
    pub amount_out_net_gas: BigUint,
    /// Block at which this quote was computed.
    pub block: BlockInfo,
    /// Effective gas price (in wei) at the time the route was computed.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, example = "20000000000"))]
    pub gas_price: Option<BigUint>,
    /// An encoded EVM transaction ready to be submitted on-chain.
    pub transaction: Option<Transaction>,
}

/// Status of an order quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
}

/// Block information at which a quote was computed.
///
/// Quotes are only valid for the block at which they were computed. Market
/// conditions may change in subsequent blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BlockInfo {
    /// Block number.
    #[cfg_attr(feature = "openapi", schema(example = 21000000))]
    pub number: u64,
    /// Block hash as a hex string.
    #[cfg_attr(
        feature = "openapi",
        schema(example = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd")
    )]
    pub hash: String,
    /// Block timestamp in Unix seconds.
    #[cfg_attr(feature = "openapi", schema(example = 1730000000))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Route {
    /// Ordered sequence of swaps to execute.
    pub swaps: Vec<Swap>,
}

/// A single swap within a route.
///
/// Represents an atomic swap on a specific liquidity pool (component).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Swap {
    /// Identifier of the liquidity pool component.
    #[cfg_attr(
        feature = "openapi",
        schema(example = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc")
    )]
    pub component_id: String,
    /// Protocol system identifier (e.g., "uniswap_v2", "uniswap_v3", "vm:balancer").
    #[cfg_attr(feature = "openapi", schema(example = "uniswap_v2"))]
    pub protocol: String,
    /// Input token address.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
    )]
    pub token_in: Address,
    /// Output token address.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
    )]
    pub token_out: Address,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "1000000000000000000")
    )]
    pub amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3500000000"))]
    pub amount_out: BigUint,
    /// Estimated gas cost for this swap (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "150000"))]
    pub gas_estimate: BigUint,
    /// Decimal of the amount to be swapped in this operation (for example, 0.5 means 50%)
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(example = "0.0"))]
    pub split: f64,
}

// ============================================================================
// HEALTH CHECK TYPES
// ============================================================================

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HealthStatus {
    /// Whether the service is healthy.
    #[cfg_attr(feature = "openapi", schema(example = true))]
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    #[cfg_attr(feature = "openapi", schema(example = 1250))]
    pub last_update_ms: u64,
    /// Number of active solver pools.
    #[cfg_attr(feature = "openapi", schema(example = 2))]
    pub num_solver_pools: usize,
    /// Whether derived data has been computed at least once.
    ///
    /// This indicates overall readiness, not per-block freshness. Some algorithms
    /// require fresh derived data for each block — they are ready to receive orders
    /// but will wait for recomputation before solving.
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(example = true))]
    pub derived_data_ready: bool,
    /// Time since last gas price update in milliseconds, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = 12000))]
    pub gas_price_age_ms: Option<u64>,
}

/// Error response body.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ErrorResponse {
    #[cfg_attr(feature = "openapi", schema(example = "bad request: no orders provided"))]
    pub error: String,
    #[cfg_attr(feature = "openapi", schema(example = "BAD_REQUEST"))]
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

// ============================================================================
// ENCODING TYPES
// ============================================================================
/// An encoded EVM transaction ready to be submitted on-chain.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Transaction {
    /// Contract address to call.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))]
    pub to: Bytes,
    /// Native token value to send with the transaction (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0"))]
    pub value: BigUint,
    /// ABI-encoded calldata as hex string.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0x1234567890abcdef"))]
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
// PRIVATE HELPERS
// ============================================================================

/// Generates a unique order ID using UUID v4.
fn generate_order_id() -> String {
    Uuid::new_v4().to_string()
}

// ============================================================================
// CONVERSIONS: fynd-core integration (feature = "core")
// ============================================================================

/// Conversions between DTO types and [`fynd_core`] domain types.
///
/// - [`From<fynd_core::X>`] for DTO types handles the Core → DTO direction.
/// - [`Into<fynd_core::X>`] for DTO types handles the DTO → Core direction. (`From` cannot be used
///   in that direction: `fynd_core` types are external, so implementing `From<DTO>` on them would
///   violate the orphan rule.)
#[cfg(feature = "core")]
mod conversions {
    use super::*;

    // -------------------------------------------------------------------------
    // DTO → Core  (use Into; From<DTO> on core types would violate orphan rules)
    // -------------------------------------------------------------------------

    impl Into<fynd_core::QuoteRequest> for QuoteRequest {
        fn into(self) -> fynd_core::QuoteRequest {
            fynd_core::QuoteRequest::new(
                self.orders
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                self.options.into(),
            )
        }
    }

    impl Into<fynd_core::QuoteOptions> for QuoteOptions {
        fn into(self) -> fynd_core::QuoteOptions {
            let mut opts = fynd_core::QuoteOptions::default();
            if let Some(ms) = self.timeout_ms {
                opts = opts.with_timeout_ms(ms);
            }
            if let Some(n) = self.min_responses {
                opts = opts.with_min_responses(n);
            }
            if let Some(gas) = self.max_gas {
                opts = opts.with_max_gas(gas);
            }
            if let Some(enc) = self.encoding_options {
                opts = opts.with_encoding_options(enc.into());
            }
            opts
        }
    }

    impl Into<fynd_core::EncodingOptions> for EncodingOptions {
        fn into(self) -> fynd_core::EncodingOptions {
            let mut opts = fynd_core::EncodingOptions::new(self.slippage)
                .with_transfer_type(self.transfer_type.into());
            if let Some(permit) = self.permit {
                opts = opts.with_permit(permit.into());
            }
            if let Some(sig) = self.permit2_signature {
                opts = opts.with_signature(sig);
            }
            opts
        }
    }

    impl Into<fynd_core::UserTransferType> for UserTransferType {
        fn into(self) -> fynd_core::UserTransferType {
            match self {
                UserTransferType::TransferFromPermit2 => {
                    fynd_core::UserTransferType::TransferFromPermit2
                }
                UserTransferType::TransferFrom => fynd_core::UserTransferType::TransferFrom,
                UserTransferType::None => fynd_core::UserTransferType::None,
            }
        }
    }

    impl Into<fynd_core::PermitSingle> for PermitSingle {
        fn into(self) -> fynd_core::PermitSingle {
            fynd_core::PermitSingle::new(self.details.into(), self.spender, self.sig_deadline)
        }
    }

    impl Into<fynd_core::PermitDetails> for PermitDetails {
        fn into(self) -> fynd_core::PermitDetails {
            fynd_core::PermitDetails::new(self.token, self.amount, self.expiration, self.nonce)
        }
    }

    impl Into<fynd_core::Order> for Order {
        fn into(self) -> fynd_core::Order {
            let mut order = fynd_core::Order::new(
                self.token_in,
                self.token_out,
                self.amount,
                self.side.into(),
                self.sender,
            )
            .with_id(self.id);
            if let Some(r) = self.receiver {
                order = order.with_receiver(r);
            }
            order
        }
    }

    impl Into<fynd_core::OrderSide> for OrderSide {
        fn into(self) -> fynd_core::OrderSide {
            match self {
                OrderSide::Sell => fynd_core::OrderSide::Sell,
            }
        }
    }

    // -------------------------------------------------------------------------
    // Core → DTO  (From is fine; DTO types are local to this crate)
    // -------------------------------------------------------------------------

    impl From<fynd_core::Quote> for Quote {
        fn from(core: fynd_core::Quote) -> Self {
            let solve_time_ms = core.solve_time_ms();
            let total_gas_estimate = core.total_gas_estimate().clone();
            Self {
                orders: core
                    .into_orders()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                total_gas_estimate,
                solve_time_ms,
            }
        }
    }

    impl From<fynd_core::OrderQuote> for OrderQuote {
        fn from(core: fynd_core::OrderQuote) -> Self {
            let order_id = core.order_id().to_string();
            let status = core.status().into();
            let amount_in = core.amount_in().clone();
            let amount_out = core.amount_out().clone();
            let gas_estimate = core.gas_estimate().clone();
            let price_impact_bps = core.price_impact_bps();
            let amount_out_net_gas = core.amount_out_net_gas().clone();
            let block = core.block().clone().into();
            let gas_price = core.gas_price().cloned();
            let transaction = core
                .transaction()
                .cloned()
                .map(Into::into);
            let route = core.into_route().map(Into::into);
            Self {
                order_id,
                status,
                route,
                amount_in,
                amount_out,
                gas_estimate,
                price_impact_bps,
                amount_out_net_gas,
                block,
                gas_price,
                transaction,
            }
        }
    }

    impl From<fynd_core::QuoteStatus> for QuoteStatus {
        fn from(core: fynd_core::QuoteStatus) -> Self {
            match core {
                fynd_core::QuoteStatus::Success => Self::Success,
                fynd_core::QuoteStatus::NoRouteFound => Self::NoRouteFound,
                fynd_core::QuoteStatus::InsufficientLiquidity => Self::InsufficientLiquidity,
                fynd_core::QuoteStatus::Timeout => Self::Timeout,
                fynd_core::QuoteStatus::NotReady => Self::NotReady,
            }
        }
    }

    impl From<fynd_core::BlockInfo> for BlockInfo {
        fn from(core: fynd_core::BlockInfo) -> Self {
            Self {
                number: core.number(),
                hash: core.hash().to_string(),
                timestamp: core.timestamp(),
            }
        }
    }

    impl From<fynd_core::Route> for Route {
        fn from(core: fynd_core::Route) -> Self {
            Self {
                swaps: core
                    .into_swaps()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            }
        }
    }

    impl From<fynd_core::Swap> for Swap {
        fn from(core: fynd_core::Swap) -> Self {
            Self {
                component_id: core.component_id().to_string(),
                protocol: core.protocol().to_string(),
                token_in: core.token_in().clone(),
                token_out: core.token_out().clone(),
                amount_in: core.amount_in().clone(),
                amount_out: core.amount_out().clone(),
                gas_estimate: core.gas_estimate().clone(),
                split: *core.split(),
            }
        }
    }

    impl From<fynd_core::Transaction> for Transaction {
        fn from(core: fynd_core::Transaction) -> Self {
            Self { to: core.to().clone(), value: core.value().clone(), data: core.data().to_vec() }
        }
    }

    #[cfg(test)]
    mod tests {
        use num_bigint::BigUint;
        use tycho_simulation::tycho_common::models::Address;

        use super::*;

        fn make_address(byte: u8) -> Address {
            Address::from([byte; 20])
        }

        #[test]
        fn test_quote_request_roundtrip() {
            let dto = QuoteRequest {
                orders: vec![Order {
                    id: "test-id".to_string(),
                    token_in: make_address(0x01),
                    token_out: make_address(0x02),
                    amount: BigUint::from(1000u64),
                    side: OrderSide::Sell,
                    sender: make_address(0xAA),
                    receiver: None,
                }],
                options: QuoteOptions {
                    timeout_ms: Some(5000),
                    min_responses: None,
                    max_gas: None,
                    encoding_options: None,
                },
            };

            let core: fynd_core::QuoteRequest = dto.clone().into();
            assert_eq!(core.orders().len(), 1);
            assert_eq!(core.orders()[0].id(), "test-id");
            assert_eq!(core.options().timeout_ms(), Some(5000));
        }

        #[test]
        fn test_quote_from_core() {
            let core: fynd_core::Quote = serde_json::from_str(
                r#"{"orders":[],"total_gas_estimate":"100000","solve_time_ms":50}"#,
            )
            .unwrap();

            let dto = Quote::from(core);
            assert_eq!(dto.total_gas_estimate, BigUint::from(100_000u64));
            assert_eq!(dto.solve_time_ms, 50);
        }

        #[test]
        fn test_order_side_into_core() {
            let core: fynd_core::OrderSide = OrderSide::Sell.into();
            assert_eq!(core, fynd_core::OrderSide::Sell);
        }

        #[test]
        fn test_quote_status_from_core() {
            let cases = [
                (fynd_core::QuoteStatus::Success, QuoteStatus::Success),
                (fynd_core::QuoteStatus::NoRouteFound, QuoteStatus::NoRouteFound),
                (fynd_core::QuoteStatus::InsufficientLiquidity, QuoteStatus::InsufficientLiquidity),
                (fynd_core::QuoteStatus::Timeout, QuoteStatus::Timeout),
                (fynd_core::QuoteStatus::NotReady, QuoteStatus::NotReady),
            ];

            for (core, expected) in cases {
                assert_eq!(QuoteStatus::from(core), expected);
            }
        }
    }
}
