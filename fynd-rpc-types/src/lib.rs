#![deny(missing_docs)]
//! Wire-format types for the [Fynd](https://fynd.xyz) RPC HTTP API.
//!
//! This crate contains only the serialisation types shared between the Fynd RPC server
//! (`fynd-rpc`) and its clients (`fynd-client`). It has no server-side infrastructure
//! dependencies (no actix-web, no server logic).
//!
//! For documentation and API reference see **<https://docs.fynd.xyz/>**.
//!
//! ## Features
//!
//! - **`openapi`** — derives `utoipa::ToSchema` on all types for OpenAPI spec generation.
//! - **`core`** — enables `Into` conversions between wire DTOs and `fynd-core` domain types.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use uuid::Uuid;

// ── Primitive byte types ──────────────────────────────────────────────────────
//
// Wire-format: `"0x{lowercase hex}"` on serialize; accepts with or without the
// `0x` prefix on deserialize. Replaces the unconditional tycho-simulation dep
// so crates that don't need the `core` feature (e.g. fynd-client) compile
// without the full simulation stack.

mod hex_bytes_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(x: &bytes::Bytes, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&format!("0x{}", hex::encode(x.as_ref())))
    }

    pub fn deserialize<'de, D>(d: D) -> Result<bytes::Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        let stripped = s.strip_prefix("0x").unwrap_or(&s);
        hex::decode(stripped)
            .map(bytes::Bytes::from)
            .map_err(serde::de::Error::custom)
    }
}

/// A byte sequence that serializes as `"0x{lowercase hex}"` in JSON.
///
/// Deserialization accepts hex strings with or without the `0x` prefix.
///
/// The inner `bytes::Bytes` is `pub` to allow zero-copy conversions with other
/// crates that also wrap `bytes::Bytes` (e.g. the `core` feature bridge to tycho).
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bytes(#[serde(with = "hex_bytes_serde")] pub bytes::Bytes);

impl Bytes {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes(0x{})", hex::encode(self.0.as_ref()))
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<&[u8]> for Bytes {
    fn from(src: &[u8]) -> Self {
        Self(bytes::Bytes::copy_from_slice(src))
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(src: Vec<u8>) -> Self {
        Self(src.into())
    }
}

impl From<bytes::Bytes> for Bytes {
    fn from(src: bytes::Bytes) -> Self {
        Self(src)
    }
}

impl<const N: usize> From<[u8; N]> for Bytes {
    fn from(src: [u8; N]) -> Self {
        Self(bytes::Bytes::copy_from_slice(&src))
    }
}

/// An EVM address — 20 bytes, same wire format as `Bytes`.
pub type Address = Bytes;

// ============================================================================
// REQUEST TYPES
// ============================================================================

/// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct QuoteRequest {
    /// Orders to solve.
    orders: Vec<Order>,
    /// Optional solving parameters that apply to all orders.
    #[serde(default)]
    options: QuoteOptions,
}

impl QuoteRequest {
    /// Create a new quote request for the given orders with default options.
    pub fn new(orders: Vec<Order>) -> Self {
        Self { orders, options: QuoteOptions::default() }
    }

    /// Override the solving options.
    pub fn with_options(mut self, options: QuoteOptions) -> Self {
        self.options = options;
        self
    }

    /// Orders to solve.
    pub fn orders(&self) -> &[Order] {
        &self.orders
    }

    /// Solving options.
    pub fn options(&self) -> &QuoteOptions {
        &self.options
    }
}

/// Options to customize the solving behavior.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct QuoteOptions {
    /// Timeout in milliseconds. If `None`, uses server default.
    #[cfg_attr(feature = "openapi", schema(example = 2000))]
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
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, example = "500000"))]
    max_gas: Option<BigUint>,
    /// Options during encoding. If None, quote will be returned without calldata.
    encoding_options: Option<EncodingOptions>,
    /// Per-request price guard overrides. If `None`, uses server defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    price_guard: Option<PriceGuardConfig>,
}

impl QuoteOptions {
    /// Set the timeout in milliseconds.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// Set the minimum number of solver responses to wait for.
    pub fn with_min_responses(mut self, n: usize) -> Self {
        self.min_responses = Some(n);
        self
    }

    /// Set the maximum gas cost allowed for a solution.
    pub fn with_max_gas(mut self, gas: BigUint) -> Self {
        self.max_gas = Some(gas);
        self
    }

    /// Set the encoding options (required for calldata to be returned).
    pub fn with_encoding_options(mut self, opts: EncodingOptions) -> Self {
        self.encoding_options = Some(opts);
        self
    }

    /// Timeout in milliseconds, if set.
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    /// Minimum solver responses to await, if set.
    pub fn min_responses(&self) -> Option<usize> {
        self.min_responses
    }

    /// Maximum allowed gas cost, if set.
    pub fn max_gas(&self) -> Option<&BigUint> {
        self.max_gas.as_ref()
    }

    /// Encoding options, if set.
    pub fn encoding_options(&self) -> Option<&EncodingOptions> {
        self.encoding_options.as_ref()
    }

    /// Set per-request price guard overrides.
    pub fn with_price_guard(mut self, config: PriceGuardConfig) -> Self {
        self.price_guard = Some(config);
        self
    }

    /// Per-request price guard config, if set.
    pub fn price_guard(&self) -> Option<&PriceGuardConfig> {
        self.price_guard.as_ref()
    }
}

/// Per-request overrides for price guard validation.
///
/// All fields are optional. When `None`, the server's configured defaults are used.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PriceGuardConfig {
    /// Maximum allowed deviation when `amount_out < expected`, in basis points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = 300))]
    lower_tolerance_bps: Option<u32>,
    /// Maximum allowed deviation when `amount_out >= expected`, in basis points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = 10000))]
    upper_tolerance_bps: Option<u32>,
    /// Whether to let solutions pass when no provider can return a price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_on_provider_error: Option<bool>,
    /// Whether to let solutions pass when no provider returns price for token pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_on_token_price_not_found: Option<bool>,
    /// Whether price guard validation is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

impl PriceGuardConfig {
    /// Set the lower tolerance in basis points.
    pub fn with_lower_tolerance_bps(mut self, bps: u32) -> Self {
        self.lower_tolerance_bps = Some(bps);
        self
    }

    /// Set the upper tolerance in basis points.
    pub fn with_upper_tolerance_bps(mut self, bps: u32) -> Self {
        self.upper_tolerance_bps = Some(bps);
        self
    }

    /// Set whether to allow solutions when providers error.
    pub fn with_allow_on_provider_error(mut self, allow: bool) -> Self {
        self.allow_on_provider_error = Some(allow);
        self
    }

    /// Set whether to allow solutions when no provider returns price for token pair.
    pub fn with_allow_on_token_price_not_found(mut self, allow: bool) -> Self {
        self.allow_on_token_price_not_found = Some(allow);
        self
    }

    /// Set whether price guard validation is enabled.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Lower tolerance in basis points, if set.
    pub fn lower_tolerance_bps(&self) -> Option<u32> {
        self.lower_tolerance_bps
    }

    /// Upper tolerance in basis points, if set.
    pub fn upper_tolerance_bps(&self) -> Option<u32> {
        self.upper_tolerance_bps
    }

    /// Whether to allow on provider error, if set.
    pub fn allow_on_provider_error(&self) -> Option<bool> {
        self.allow_on_provider_error
    }

    /// Whether to allow on token price not found, if set.
    pub fn allow_on_token_price_not_found(&self) -> Option<bool> {
        self.allow_on_token_price_not_found
    }

    /// Whether price guard is enabled, if set.
    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

/// Token transfer method for moving funds into Tycho execution.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum UserTransferType {
    /// Use Permit2 for token transfer. Requires `permit` and `signature`.
    TransferFromPermit2,
    /// Use standard ERC-20 approval and `transferFrom`. Default.
    #[default]
    TransferFrom,
    /// Use funds from the Tycho Router vault (no transfer performed).
    UseVaultsFunds,
}

/// Client fee configuration for the Tycho Router.
///
/// When provided, the router charges a client fee on the swap output. The `signature`
/// must be an EIP-712 signature by the `receiver` over the `ClientFee` typed data.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ClientFeeParams {
    /// Fee in basis points (0–10,000). 100 = 1%.
    #[cfg_attr(feature = "openapi", schema(example = 100))]
    bps: u16,
    /// Address that receives the fee (also the required EIP-712 signer).
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
    )]
    receiver: Bytes,
    /// Maximum subsidy from the client's vault balance.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0"))]
    max_contribution: BigUint,
    /// Unix timestamp after which the signature is invalid.
    #[cfg_attr(feature = "openapi", schema(example = 1893456000))]
    deadline: u64,
    /// 65-byte EIP-712 ECDSA signature by `receiver` (hex-encoded).
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0xabcd..."))]
    signature: Bytes,
}

impl ClientFeeParams {
    /// Create new client fee params.
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FeeBreakdown {
    /// Router protocol fee (fee on output + router's share of client fee).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "350000"))]
    router_fee: BigUint,
    /// Client's portion of the fee (after the router takes its share).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "2800000"))]
    client_fee: BigUint,
    /// Maximum slippage: (amount_out - router_fee - client_fee) * slippage.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3496850"))]
    max_slippage: BigUint,
    /// Minimum amount the user receives on-chain.
    /// Equal to amount_out - router_fee - client_fee - max_slippage.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3493353150"))]
    min_amount_received: BigUint,
}

impl FeeBreakdown {
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EncodingOptions {
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(example = "0.001"))]
    slippage: f64,
    /// Token transfer method. Defaults to `transfer_from`.
    #[serde(default)]
    transfer_type: UserTransferType,
    /// Permit2 single-token authorization. Required when using `transfer_from_permit2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permit: Option<PermitSingle>,
    /// Permit2 signature (65 bytes, hex-encoded). Required when `permit` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, example = "0xabcd..."))]
    permit2_signature: Option<Bytes>,
    /// Client fee configuration. When absent, no fee is charged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_fee_params: Option<ClientFeeParams>,
}

impl EncodingOptions {
    /// Create encoding options with the given slippage and default transfer type.
    pub fn new(slippage: f64) -> Self {
        Self {
            slippage,
            transfer_type: UserTransferType::default(),
            permit: None,
            permit2_signature: None,
            client_fee_params: None,
        }
    }

    /// Override the token transfer method.
    pub fn with_transfer_type(mut self, t: UserTransferType) -> Self {
        self.transfer_type = t;
        self
    }

    /// Set the Permit2 single-token authorization and its signature.
    pub fn with_permit2(mut self, permit: PermitSingle, sig: Bytes) -> Self {
        self.permit = Some(permit);
        self.permit2_signature = Some(sig);
        self
    }

    /// Slippage tolerance (e.g. `0.001` = 0.1%).
    pub fn slippage(&self) -> f64 {
        self.slippage
    }

    /// Token transfer method.
    pub fn transfer_type(&self) -> &UserTransferType {
        &self.transfer_type
    }

    /// Permit2 single-token authorization, if set.
    pub fn permit(&self) -> Option<&PermitSingle> {
        self.permit.as_ref()
    }

    /// Permit2 signature, if set.
    pub fn permit2_signature(&self) -> Option<&Bytes> {
        self.permit2_signature.as_ref()
    }

    /// Set the client fee params.
    pub fn with_client_fee_params(mut self, params: ClientFeeParams) -> Self {
        self.client_fee_params = Some(params);
        self
    }

    /// Client fee params, if set.
    pub fn client_fee_params(&self) -> Option<&ClientFeeParams> {
        self.client_fee_params.as_ref()
    }
}

/// A single permit for permit2 token transfer authorization.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PermitSingle {
    /// The permit details (token, amount, expiration, nonce).
    details: PermitDetails,
    /// Address authorized to spend the tokens (typically the router).
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))]
    spender: Bytes,
    /// Deadline timestamp for the permit signature.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "1893456000"))]
    sig_deadline: BigUint,
}

impl PermitSingle {
    /// Create a new permit with the given details, spender, and signature deadline.
    pub fn new(details: PermitDetails, spender: Bytes, sig_deadline: BigUint) -> Self {
        Self { details, spender, sig_deadline }
    }

    /// Permit details (token, amount, expiration, nonce).
    pub fn details(&self) -> &PermitDetails {
        &self.details
    }

    /// Address authorized to spend the tokens.
    pub fn spender(&self) -> &Bytes {
        &self.spender
    }

    /// Signature deadline timestamp.
    pub fn sig_deadline(&self) -> &BigUint {
        &self.sig_deadline
    }
}

/// Details for a permit2 single-token permit.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PermitDetails {
    /// Token address for which the permit is granted.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))]
    token: Bytes,
    /// Amount of tokens approved.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "1000000000000000000"))]
    amount: BigUint,
    /// Expiration timestamp for the permit.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "1893456000"))]
    expiration: BigUint,
    /// Nonce to prevent replay attacks.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0"))]
    nonce: BigUint,
}

impl PermitDetails {
    /// Create permit details with the given token, amount, expiration, and nonce.
    pub fn new(token: Bytes, amount: BigUint, expiration: BigUint, nonce: BigUint) -> Self {
        Self { token, amount, expiration, nonce }
    }

    /// Token address for which the permit is granted.
    pub fn token(&self) -> &Bytes {
        &self.token
    }

    /// Amount of tokens approved.
    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    /// Expiration timestamp for the permit.
    pub fn expiration(&self) -> &BigUint {
        &self.expiration
    }

    /// Nonce to prevent replay attacks.
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Quote {
    /// Quotes for each order, in the same order as the request.
    orders: Vec<OrderQuote>,
    /// Total estimated gas for executing all swaps (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "150000"))]
    total_gas_estimate: BigUint,
    /// Time taken to compute this solution, in milliseconds.
    #[cfg_attr(feature = "openapi", schema(example = 12))]
    solve_time_ms: u64,
}

impl Quote {
    /// Create a new quote.
    pub fn new(orders: Vec<OrderQuote>, total_gas_estimate: BigUint, solve_time_ms: u64) -> Self {
        Self { orders, total_gas_estimate, solve_time_ms }
    }

    /// Quotes for each order.
    pub fn orders(&self) -> &[OrderQuote] {
        &self.orders
    }

    /// Consume this quote and return the order quotes.
    pub fn into_orders(self) -> Vec<OrderQuote> {
        self.orders
    }

    /// Total estimated gas for executing all swaps.
    pub fn total_gas_estimate(&self) -> &BigUint {
        &self.total_gas_estimate
    }

    /// Time taken to compute this solution, in milliseconds.
    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }
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
    id: String,
    /// Input token address (the token being sold).
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
    )]
    token_in: Address,
    /// Output token address (the token being bought).
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
    )]
    token_out: Address,
    /// Amount to swap, interpreted according to `side` (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "1000000000000000000")
    )]
    amount: BigUint,
    /// Whether this is a sell (exact input) or buy (exact output) order.
    side: OrderSide,
    /// Address that will send the input tokens.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
    )]
    sender: Address,
    /// Address that will receive the output tokens.
    ///
    /// Defaults to `sender` if not specified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = Option<String>, example = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045")
    )]
    receiver: Option<Address>,
}

impl Order {
    /// Create a new order. The `id` is left empty and filled by the server on receipt.
    pub fn new(
        token_in: Address,
        token_out: Address,
        amount: BigUint,
        side: OrderSide,
        sender: Address,
    ) -> Self {
        Self { id: String::new(), token_in, token_out, amount, side, sender, receiver: None }
    }

    /// Override the order ID (used in tests and internal conversions).
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Set the receiver address (defaults to sender if not set).
    pub fn with_receiver(mut self, receiver: Address) -> Self {
        self.receiver = Some(receiver);
        self
    }

    /// Order ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Input token address.
    pub fn token_in(&self) -> &Address {
        &self.token_in
    }

    /// Output token address.
    pub fn token_out(&self) -> &Address {
        &self.token_out
    }

    /// Amount to swap.
    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    /// Order side (sell or buy).
    pub fn side(&self) -> OrderSide {
        self.side
    }

    /// Sender address.
    pub fn sender(&self) -> &Address {
        &self.sender
    }

    /// Receiver address, if set.
    pub fn receiver(&self) -> Option<&Address> {
        self.receiver.as_ref()
    }
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

/// Quote for a single [`Order`].
///
/// Contains the route to execute (if found), along with expected amounts,
/// gas estimates, and status information.
#[must_use]
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OrderQuote {
    /// ID of the order this solution corresponds to.
    #[cfg_attr(feature = "openapi", schema(example = "f47ac10b-58cc-4372-a567-0e02b2c3d479"))]
    order_id: String,
    /// Status indicating whether a route was found.
    status: QuoteStatus,
    /// The route to execute, if a valid route was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<Route>,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "1000000000000000000")
    )]
    amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3500000000"))]
    amount_out: BigUint,
    /// Estimated gas cost for executing this route (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "150000"))]
    gas_estimate: BigUint,
    /// Price impact in basis points (1 bip = 0.01%).
    #[serde(skip_serializing_if = "Option::is_none")]
    price_impact_bps: Option<i32>,
    /// Amount out minus gas cost in output token terms.
    /// Used by WorkerPoolRouter to compare solutions from different solvers.
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3498000000"))]
    amount_out_net_gas: BigUint,
    /// Block at which this quote was computed.
    block: BlockInfo,
    /// Effective gas price (in wei) at the time the route was computed.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, example = "20000000000"))]
    gas_price: Option<BigUint>,
    /// An encoded EVM transaction ready to be submitted on-chain.
    transaction: Option<Transaction>,
    /// Fee breakdown (populated when encoding options are provided).
    #[serde(skip_serializing_if = "Option::is_none")]
    fee_breakdown: Option<FeeBreakdown>,
}

impl OrderQuote {
    /// Order ID this solution corresponds to.
    pub fn order_id(&self) -> &str {
        &self.order_id
    }

    /// Status indicating whether a route was found.
    pub fn status(&self) -> QuoteStatus {
        self.status
    }

    /// The route to execute, if a valid route was found.
    pub fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// Amount of input token.
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// Amount of output token.
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Estimated gas cost for executing this route.
    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    /// Price impact in basis points, if available.
    pub fn price_impact_bps(&self) -> Option<i32> {
        self.price_impact_bps
    }

    /// Amount out minus gas cost in output token terms.
    pub fn amount_out_net_gas(&self) -> &BigUint {
        &self.amount_out_net_gas
    }

    /// Block at which this quote was computed.
    pub fn block(&self) -> &BlockInfo {
        &self.block
    }

    /// Effective gas price at the time the route was computed, if available.
    pub fn gas_price(&self) -> Option<&BigUint> {
        self.gas_price.as_ref()
    }

    /// Encoded EVM transaction, if encoding options were provided in the request.
    pub fn transaction(&self) -> Option<&Transaction> {
        self.transaction.as_ref()
    }

    /// Fee breakdown, if encoding options were provided in the request.
    pub fn fee_breakdown(&self) -> Option<&FeeBreakdown> {
        self.fee_breakdown.as_ref()
    }
}

/// Status of an order quote.
#[non_exhaustive]
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
    /// The solution failed external price validation.
    PriceCheckFailed,
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
    number: u64,
    /// Block hash as a hex string.
    #[cfg_attr(
        feature = "openapi",
        schema(example = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd")
    )]
    hash: String,
    /// Block timestamp in Unix seconds.
    #[cfg_attr(feature = "openapi", schema(example = 1730000000))]
    timestamp: u64,
}

impl BlockInfo {
    /// Create a new block info.
    pub fn new(number: u64, hash: String, timestamp: u64) -> Self {
        Self { number, hash, timestamp }
    }

    /// Block number.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// Block hash as a hex string.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Block timestamp in Unix seconds.
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Route {
    /// Ordered sequence of swaps to execute.
    swaps: Vec<Swap>,
}

impl Route {
    /// Create a route from an ordered sequence of swaps.
    pub fn new(swaps: Vec<Swap>) -> Self {
        Self { swaps }
    }

    /// Ordered sequence of swaps to execute.
    pub fn swaps(&self) -> &[Swap] {
        &self.swaps
    }

    /// Consume this route and return the swaps.
    pub fn into_swaps(self) -> Vec<Swap> {
        self.swaps
    }
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
    component_id: String,
    /// Protocol system identifier (e.g., "uniswap_v2", "uniswap_v3", "vm:balancer").
    #[cfg_attr(feature = "openapi", schema(example = "uniswap_v2"))]
    protocol: String,
    /// Input token address.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
    )]
    token_in: Address,
    /// Output token address.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
    )]
    token_out: Address,
    /// Amount of input token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "1000000000000000000")
    )]
    amount_in: BigUint,
    /// Amount of output token (in token units, as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "3500000000"))]
    amount_out: BigUint,
    /// Estimated gas cost for this swap (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "150000"))]
    gas_estimate: BigUint,
    /// Decimal of the amount to be swapped in this operation (for example, 0.5 means 50%)
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(example = "0.0"))]
    split: f64,
}

impl Swap {
    /// Create a new swap.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        component_id: String,
        protocol: String,
        token_in: Address,
        token_out: Address,
        amount_in: BigUint,
        amount_out: BigUint,
        gas_estimate: BigUint,
        split: f64,
    ) -> Self {
        Self {
            component_id,
            protocol,
            token_in,
            token_out,
            amount_in,
            amount_out,
            gas_estimate,
            split,
        }
    }

    /// Liquidity pool component identifier.
    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// Protocol system identifier.
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// Input token address.
    pub fn token_in(&self) -> &Address {
        &self.token_in
    }

    /// Output token address.
    pub fn token_out(&self) -> &Address {
        &self.token_out
    }

    /// Amount of input token.
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// Amount of output token.
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Estimated gas cost for this swap.
    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    /// Fraction of the total amount routed through this swap.
    pub fn split(&self) -> f64 {
        self.split
    }
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
    healthy: bool,
    /// Time since last market update in milliseconds.
    #[cfg_attr(feature = "openapi", schema(example = 1250))]
    last_update_ms: u64,
    /// Number of active solver pools.
    #[cfg_attr(feature = "openapi", schema(example = 2))]
    num_solver_pools: usize,
    /// Whether derived data has been computed at least once.
    ///
    /// This indicates overall readiness, not per-block freshness. Some algorithms
    /// require fresh derived data for each block — they are ready to receive orders
    /// but will wait for recomputation before solving.
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(example = true))]
    derived_data_ready: bool,
    /// Time since last gas price update in milliseconds, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = 12000))]
    gas_price_age_ms: Option<u64>,
}

impl HealthStatus {
    /// Create a new health status.
    pub fn new(
        healthy: bool,
        last_update_ms: u64,
        num_solver_pools: usize,
        derived_data_ready: bool,
        gas_price_age_ms: Option<u64>,
    ) -> Self {
        Self { healthy, last_update_ms, num_solver_pools, derived_data_ready, gas_price_age_ms }
    }

    /// Whether the service is healthy.
    pub fn healthy(&self) -> bool {
        self.healthy
    }

    /// Time since last market update in milliseconds.
    pub fn last_update_ms(&self) -> u64 {
        self.last_update_ms
    }

    /// Number of active solver pools.
    pub fn num_solver_pools(&self) -> usize {
        self.num_solver_pools
    }

    /// Whether derived data has been computed at least once.
    pub fn derived_data_ready(&self) -> bool {
        self.derived_data_ready
    }

    /// Time since last gas price update in milliseconds, if available.
    pub fn gas_price_age_ms(&self) -> Option<u64> {
        self.gas_price_age_ms
    }
}

/// Static metadata about this Fynd instance, returned by `GET /v1/info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InstanceInfo {
    /// EIP-155 chain ID (e.g. 1 for Ethereum mainnet).
    #[cfg_attr(feature = "openapi", schema(example = 1))]
    chain_id: u64,
    /// Address of the Tycho Router contract on this chain.
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35")
    )]
    router_address: Bytes,
    /// Address of the canonical Permit2 contract (same on all EVM chains).
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = String, example = "0x000000000022D473030F116dDEE9F6B43aC78BA3")
    )]
    permit2_address: Bytes,
}

impl InstanceInfo {
    /// Creates a new instance info.
    pub fn new(chain_id: u64, router_address: Bytes, permit2_address: Bytes) -> Self {
        Self { chain_id, router_address, permit2_address }
    }

    /// EIP-155 chain ID.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Address of the Tycho Router contract.
    pub fn router_address(&self) -> &Bytes {
        &self.router_address
    }

    /// Address of the canonical Permit2 contract.
    pub fn permit2_address(&self) -> &Bytes {
        &self.permit2_address
    }
}

/// Error response body.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ErrorResponse {
    #[cfg_attr(feature = "openapi", schema(example = "bad request: no orders provided"))]
    error: String,
    #[cfg_attr(feature = "openapi", schema(example = "BAD_REQUEST"))]
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

impl ErrorResponse {
    /// Create an error response with the given message and code.
    pub fn new(error: String, code: String) -> Self {
        Self { error, code, details: None }
    }

    /// Add structured details to the error response.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Human-readable error message.
    pub fn error(&self) -> &str {
        &self.error
    }

    /// Machine-readable error code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Structured error details, if present.
    pub fn details(&self) -> Option<&serde_json::Value> {
        self.details.as_ref()
    }
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
    to: Bytes,
    /// Native token value to send with the transaction (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0"))]
    value: BigUint,
    /// ABI-encoded calldata as hex string.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "0x1234567890abcdef"))]
    #[serde(serialize_with = "serialize_bytes_hex", deserialize_with = "deserialize_bytes_hex")]
    data: Vec<u8>,
}

impl Transaction {
    /// Create a new transaction.
    pub fn new(to: Bytes, value: BigUint, data: Vec<u8>) -> Self {
        Self { to, value, data }
    }

    /// Contract address to call.
    pub fn to(&self) -> &Bytes {
        &self.to
    }

    /// Native token value to send with the transaction.
    pub fn value(&self) -> &BigUint {
        &self.value
    }

    /// ABI-encoded calldata.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
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
// WIRE FORMAT TESTS
// ============================================================================
//
// These tests pin the JSON wire format for the key API types. They catch
// field renames, enum case changes, wrong numeric types, and structural
// changes that would silently break API clients.

#[cfg(test)]
mod wire_format_tests {
    use num_bigint::BigUint;

    use super::*;

    // ── Bytes: accept hex without 0x prefix ───────────────────────────────────
    //
    // All other Bytes/Address format behaviour is covered implicitly by the
    // struct tests below. This case (no prefix) is the only non-obvious one
    // worth testing in isolation.

    #[test]
    fn bytes_deserializes_without_0x_prefix() {
        let b: Bytes = serde_json::from_str(r#""deadbeef""#).unwrap();
        assert_eq!(b.as_ref(), [0xDE, 0xAD, 0xBE, 0xEF]);
    }

    // ── Order: full request JSON shape ────────────────────────────────────────
    //
    // Verifies field names, side as "sell" (not "Sell"), amount as decimal
    // string (not a number), addresses as "0x..." hex, and receiver absent
    // when not set.

    #[test]
    fn order_serializes_to_full_json() {
        let order = Order::new(
            Bytes::from([0xAAu8; 20]),
            Bytes::from([0xBBu8; 20]),
            BigUint::from(1_000_000_000_000_000_000u64),
            OrderSide::Sell,
            Bytes::from([0xCCu8; 20]),
        )
        .with_id("abc");

        assert_eq!(
            serde_json::to_value(&order).unwrap(),
            serde_json::json!({
                "id": "abc",
                "token_in":  "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "token_out": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "amount":    "1000000000000000000",
                "side":      "sell",
                "sender":    "0xcccccccccccccccccccccccccccccccccccccccc"
            })
        );
    }

    // ── OrderQuote: full response JSON deserialization ────────────────────────
    //
    // Verifies that a realistic server response deserializes correctly:
    // status as "success", BigUint fields from decimal strings, nested block,
    // route with a Swap whose token addresses are hex and split is a string.

    #[test]
    fn order_quote_deserializes_from_json() {
        let json = r#"{
            "order_id": "order-1",
            "status": "success",
            "amount_in": "1000000000000000000",
            "amount_out": "2000000000",
            "gas_estimate": "150000",
            "amount_out_net_gas": "1999000000",
            "price_impact_bps": 5,
            "block": { "number": 21000000, "hash": "0xdeadbeef", "timestamp": 1700000000 },
            "route": { "swaps": [{
                "component_id": "pool-1",
                "protocol": "uniswap_v3",
                "token_in":  "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "token_out": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "amount_in": "1000000000000000000",
                "amount_out": "2000000000",
                "gas_estimate": "150000",
                "split": "0"
            }]}
        }"#;

        let quote: OrderQuote = serde_json::from_str(json).unwrap();

        assert_eq!(quote.status(), QuoteStatus::Success);
        assert_eq!(*quote.amount_in(), BigUint::from(1_000_000_000_000_000_000u64));
        assert_eq!(quote.price_impact_bps(), Some(5));
        assert_eq!(quote.block().number(), 21_000_000);

        let swap = &quote.route().unwrap().swaps()[0];
        assert_eq!(swap.token_in().as_ref(), [0xAAu8; 20]);
        assert_eq!(swap.token_out().as_ref(), [0xBBu8; 20]);
        assert_eq!(swap.split(), 0.0);
    }

    // ── EncodingOptions: full request JSON shape ──────────────────────────────
    //
    // Verifies transfer_type serializes as "transfer_from" (snake_case, not
    // "TransferFrom"), slippage is a float, and optional fields are absent
    // when not set.

    #[test]
    fn encoding_options_serializes_to_full_json() {
        assert_eq!(
            serde_json::to_value(EncodingOptions::new(0.005)).unwrap(),
            serde_json::json!({
                "slippage":      "0.005",
                "transfer_type": "transfer_from"
            })
        );
    }

    // ── InstanceInfo: response deserialization with forward compat ────────────
    //
    // Verifies the /info endpoint response deserializes correctly, and that
    // unknown fields added in future server versions are silently ignored
    // (no #[serde(deny_unknown_fields)] on this type).

    #[test]
    fn instance_info_deserializes_and_ignores_unknown_fields() {
        let json = r#"{
            "chain_id": 1,
            "router_address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "permit2_address": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "future_field": "ignored"
        }"#;

        let info: InstanceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.chain_id(), 1);
        assert_eq!(info.router_address().as_ref(), [0xAAu8; 20]);
        assert_eq!(info.permit2_address().as_ref(), [0xBBu8; 20]);
    }
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
    use tycho_simulation::tycho_core::Bytes as TychoBytes;

    use super::*;

    // ── Byte-type bridge ─────────────────────────────────────────────────────
    //
    // Both types wrap `bytes::Bytes` and share the same wire format. The inner
    // field is `pub` on TychoBytes, so the conversion is zero-copy.

    impl From<TychoBytes> for Bytes {
        fn from(b: TychoBytes) -> Self {
            Self(b.0)
        }
    }

    impl From<Bytes> for TychoBytes {
        fn from(b: Bytes) -> Self {
            Self(b.0)
        }
    }

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
            if let Some(pg) = self.price_guard {
                opts = opts.with_price_guard(pg.into());
            }
            opts
        }
    }

    impl Into<fynd_core::PriceGuardConfig> for PriceGuardConfig {
        fn into(self) -> fynd_core::PriceGuardConfig {
            let mut config = fynd_core::PriceGuardConfig::default();
            if let Some(bps) = self.lower_tolerance_bps {
                config = config.with_lower_tolerance_bps(bps);
            }
            if let Some(bps) = self.upper_tolerance_bps {
                config = config.with_upper_tolerance_bps(bps);
            }
            if let Some(allow) = self.allow_on_provider_error {
                config = config.with_allow_on_provider_error(allow);
            }
            if let Some(allow) = self.allow_on_token_price_not_found {
                config = config.with_allow_on_token_price_not_found(allow);
            }
            if let Some(enabled) = self.enabled {
                config = config.with_enabled(enabled);
            }
            config
        }
    }

    impl Into<fynd_core::EncodingOptions> for EncodingOptions {
        fn into(self) -> fynd_core::EncodingOptions {
            let mut opts = fynd_core::EncodingOptions::new(self.slippage)
                .with_transfer_type(self.transfer_type.into());
            if let (Some(permit), Some(sig)) = (self.permit, self.permit2_signature) {
                opts = opts
                    .with_permit(permit.into())
                    .with_signature(sig.into());
            }
            if let Some(fee) = self.client_fee_params {
                opts = opts.with_client_fee_params(fee.into());
            }
            opts
        }
    }

    impl Into<fynd_core::ClientFeeParams> for ClientFeeParams {
        fn into(self) -> fynd_core::ClientFeeParams {
            fynd_core::ClientFeeParams::new(
                self.bps,
                self.receiver.into(),
                self.max_contribution,
                self.deadline,
                self.signature.into(),
            )
        }
    }

    impl Into<fynd_core::UserTransferType> for UserTransferType {
        fn into(self) -> fynd_core::UserTransferType {
            match self {
                UserTransferType::TransferFromPermit2 => {
                    fynd_core::UserTransferType::TransferFromPermit2
                }
                UserTransferType::TransferFrom => fynd_core::UserTransferType::TransferFrom,
                UserTransferType::UseVaultsFunds => fynd_core::UserTransferType::UseVaultsFunds,
            }
        }
    }

    impl Into<fynd_core::PermitSingle> for PermitSingle {
        fn into(self) -> fynd_core::PermitSingle {
            fynd_core::PermitSingle::new(
                self.details.into(),
                self.spender.into(),
                self.sig_deadline,
            )
        }
    }

    impl Into<fynd_core::PermitDetails> for PermitDetails {
        fn into(self) -> fynd_core::PermitDetails {
            fynd_core::PermitDetails::new(
                self.token.into(),
                self.amount,
                self.expiration,
                self.nonce,
            )
        }
    }

    impl Into<fynd_core::Order> for Order {
        fn into(self) -> fynd_core::Order {
            let mut order = fynd_core::Order::new(
                self.token_in.into(),
                self.token_out.into(),
                self.amount,
                self.side.into(),
                self.sender.into(),
            )
            .with_id(self.id);
            if let Some(r) = self.receiver {
                order = order.with_receiver(r.into());
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
            let fee_breakdown = core
                .fee_breakdown()
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
                fee_breakdown,
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
                fynd_core::QuoteStatus::PriceCheckFailed => Self::PriceCheckFailed,
                // Fallback for future variants added to fynd_core::QuoteStatus.
                _ => Self::NotReady,
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
                token_in: core.token_in().clone().into(),
                token_out: core.token_out().clone().into(),
                amount_in: core.amount_in().clone(),
                amount_out: core.amount_out().clone(),
                gas_estimate: core.gas_estimate().clone(),
                split: *core.split(),
            }
        }
    }

    impl From<fynd_core::Transaction> for Transaction {
        fn from(core: fynd_core::Transaction) -> Self {
            Self {
                to: core.to().clone().into(),
                value: core.value().clone(),
                data: core.data().to_vec(),
            }
        }
    }

    impl From<fynd_core::FeeBreakdown> for FeeBreakdown {
        fn from(core: fynd_core::FeeBreakdown) -> Self {
            Self {
                router_fee: core.router_fee().clone(),
                client_fee: core.client_fee().clone(),
                max_slippage: core.max_slippage().clone(),
                min_amount_received: core.min_amount_received().clone(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use num_bigint::BigUint;

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
                    price_guard: None,
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
        fn test_client_fee_params_into_core() {
            let dto = ClientFeeParams::new(
                200,
                Bytes::from(make_address(0xBB).as_ref()),
                BigUint::from(1_000_000u64),
                1_893_456_000u64,
                Bytes::from(vec![0xABu8; 65]),
            );
            let core: fynd_core::ClientFeeParams = dto.into();
            assert_eq!(core.bps(), 200);
            assert_eq!(*core.max_contribution(), BigUint::from(1_000_000u64));
            assert_eq!(core.deadline(), 1_893_456_000u64);
            assert_eq!(core.signature().len(), 65);
        }

        #[test]
        fn test_encoding_options_with_client_fee_into_core() {
            let fee = ClientFeeParams::new(
                100,
                Bytes::from(make_address(0xCC).as_ref()),
                BigUint::from(500u64),
                9_999u64,
                Bytes::from(vec![0xDEu8; 65]),
            );
            let dto = EncodingOptions::new(0.005).with_client_fee_params(fee);
            let core: fynd_core::EncodingOptions = dto.into();

            assert!(core.client_fee_params().is_some());
            let core_fee = core.client_fee_params().unwrap();
            assert_eq!(core_fee.bps(), 100);
            assert_eq!(*core_fee.max_contribution(), BigUint::from(500u64));
        }

        #[test]
        fn test_client_fee_params_serde_roundtrip() {
            let fee = ClientFeeParams::new(
                150,
                Bytes::from(make_address(0xDD).as_ref()),
                BigUint::from(999_999u64),
                1_700_000_000u64,
                Bytes::from(vec![0xFFu8; 65]),
            );
            let json = serde_json::to_string(&fee).unwrap();
            assert!(json.contains(r#""max_contribution":"999999""#));
            assert!(json.contains(r#""deadline":1700000000"#));

            let deserialized: ClientFeeParams = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized.bps(), 150);
            assert_eq!(*deserialized.max_contribution(), BigUint::from(999_999u64));
        }

        #[test]
        fn test_price_guard_config_into_core() {
            let dto = PriceGuardConfig::default()
                .with_lower_tolerance_bps(200)
                .with_upper_tolerance_bps(5000)
                .with_allow_on_provider_error(true)
                .with_enabled(false);

            let core: fynd_core::PriceGuardConfig = dto.into();
            assert_eq!(core.lower_tolerance_bps(), 200);
            assert_eq!(core.upper_tolerance_bps(), 5000);
            assert!(core.allow_on_provider_error());
            assert!(!core.enabled());
        }

        #[test]
        fn test_price_guard_config_defaults_preserved() {
            let dto = PriceGuardConfig::default().with_lower_tolerance_bps(100);
            let core: fynd_core::PriceGuardConfig = dto.into();

            assert_eq!(core.lower_tolerance_bps(), 100);
            // Unset fields get core defaults
            assert_eq!(core.upper_tolerance_bps(), 10_000);
            assert!(!core.allow_on_provider_error());
            assert!(!core.enabled());
        }

        #[test]
        fn test_quote_options_with_price_guard_roundtrip() {
            let dto = QuoteRequest {
                orders: vec![Order {
                    id: "pg-test".to_string(),
                    token_in: make_address(0x01),
                    token_out: make_address(0x02),
                    amount: BigUint::from(1000u64),
                    side: OrderSide::Sell,
                    sender: make_address(0xAA),
                    receiver: None,
                }],
                options: QuoteOptions::default()
                    .with_price_guard(PriceGuardConfig::default().with_enabled(false)),
            };

            let core: fynd_core::QuoteRequest = dto.into();
            let pg = core
                .options()
                .price_guard()
                .expect("price_guard should be set");
            assert!(!pg.enabled());
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
