use alloy::{
    primitives::{keccak256, U256},
    sol_types::SolValue,
};
use bytes::Bytes;
use num_bigint::BigUint;

use crate::mapping::biguint_to_u256;

// ============================================================================
// ENCODING TYPES
// ============================================================================

/// Token transfer method used when building an on-chain swap transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserTransferType {
    /// Use standard ERC-20 `approve` + `transferFrom`. Default.
    #[default]
    TransferFrom,
    /// Use Permit2 single-token authorization. Requires [`EncodingOptions::with_permit2`].
    TransferFromPermit2,
    /// Use funds from the Tycho Router vault (no token transfer performed).
    UseVaultsFunds,
}

/// Per-token details for a Permit2 single-token authorization.
#[derive(Debug, Clone)]
pub struct PermitDetails {
    pub(crate) token: bytes::Bytes,
    pub(crate) amount: num_bigint::BigUint,
    pub(crate) expiration: num_bigint::BigUint,
    pub(crate) nonce: num_bigint::BigUint,
}

impl PermitDetails {
    pub fn new(
        token: bytes::Bytes,
        amount: num_bigint::BigUint,
        expiration: num_bigint::BigUint,
        nonce: num_bigint::BigUint,
    ) -> Self {
        Self { token, amount, expiration, nonce }
    }
}

/// A single Permit2 authorization, covering one token for one spender.
#[derive(Debug, Clone)]
pub struct PermitSingle {
    pub(crate) details: PermitDetails,
    pub(crate) spender: bytes::Bytes,
    pub(crate) sig_deadline: num_bigint::BigUint,
}

impl PermitSingle {
    pub fn new(
        details: PermitDetails,
        spender: bytes::Bytes,
        sig_deadline: num_bigint::BigUint,
    ) -> Self {
        Self { details, spender, sig_deadline }
    }

    /// Compute the Permit2 EIP-712 signing hash for this permit.
    ///
    /// Pass the returned bytes to your signer's `sign_hash` method, then supply the
    /// 65-byte result as the `signature` argument to [`EncodingOptions::with_permit2`].
    ///
    /// `permit2_address` must be the 20-byte address of the Permit2 contract
    /// (canonical cross-chain deployment: `0x000000000022D473030F116dDEE9F6B43aC78BA3`).
    ///
    /// # Errors
    ///
    /// Returns [`crate::FyndError::Protocol`] if any address field is not exactly 20 bytes,
    /// or if `amount` / `expiration` / `nonce` exceed their respective Solidity types.
    pub fn eip712_signing_hash(
        &self,
        chain_id: u64,
        permit2_address: &bytes::Bytes,
    ) -> Result<[u8; 32], crate::error::FyndError> {
        use alloy::sol_types::{eip712_domain, SolStruct};

        let permit2_addr = p2_bytes_to_address(permit2_address, "permit2_address")?;
        let token = p2_bytes_to_address(&self.details.token, "token")?;
        let spender = p2_bytes_to_address(&self.spender, "spender")?;

        let amount = p2_biguint_to_uint160(&self.details.amount)?;
        let expiration = p2_biguint_to_uint48(&self.details.expiration)?;
        let nonce = p2_biguint_to_uint48(&self.details.nonce)?;
        let sig_deadline = crate::mapping::biguint_to_u256(&self.sig_deadline);

        let domain = eip712_domain! {
            name: "Permit2",
            chain_id: chain_id,
            verifying_contract: permit2_addr,
        };
        #[allow(non_snake_case)]
        let permit = permit2_sol::PermitSingle {
            details: permit2_sol::PermitDetails { token, amount, expiration, nonce },
            spender,
            sigDeadline: sig_deadline,
        };
        Ok(permit.eip712_signing_hash(&domain).0)
    }
}

/// Client fee configuration for the Tycho Router.
///
/// When attached to [`EncodingOptions`] via [`EncodingOptions::with_client_fee`], the router
/// charges a client fee on the swap output. The `signature` must be an EIP-712 signature by the
/// `receiver` over the `ClientFee` typed data — compute the hash with
/// [`ClientFeeParams::eip712_signing_hash`].
#[derive(Debug, Clone)]
pub struct ClientFeeParams {
    pub(crate) bps: u16,
    pub(crate) receiver: Bytes,
    pub(crate) max_contribution: BigUint,
    pub(crate) deadline: u64,
    pub(crate) signature: Option<Bytes>,
}

impl ClientFeeParams {
    /// Create client fee params.
    ///
    /// `signature` must be a 65-byte EIP-712 signature by `receiver`.
    pub fn new(bps: u16, receiver: Bytes, max_contribution: BigUint, deadline: u64) -> Self {
        Self { bps, receiver, max_contribution, deadline, signature: None }
    }

    /// Set the EIP-712 signature.
    pub fn with_signature(mut self, signature: Bytes) -> Self {
        self.signature = Some(signature);
        self
    }

    /// Compute the EIP-712 signing hash for the client fee params.
    ///
    /// Pass the returned hash to the fee receiver's signer, then supply the
    /// 65-byte result as `signature` when constructing [`ClientFeeParams`].
    ///
    /// `router_address` is the 20-byte address of the TychoRouter contract.
    pub fn eip712_signing_hash(
        &self,
        chain_id: u64,
        router_address: &Bytes,
    ) -> Result<[u8; 32], crate::error::FyndError> {
        let router_addr = p2_bytes_to_address(router_address, "router_address")?;
        let fee_receiver = p2_bytes_to_address(&self.receiver, "receiver")?;
        let max_contrib = biguint_to_u256(&self.max_contribution);
        let dl = U256::from(self.deadline);

        let type_hash = keccak256(
            b"ClientFee(uint16 clientFeeBps,address clientFeeReceiver,\
uint256 maxClientContribution,uint256 deadline)",
        );

        let domain_type_hash = keccak256(
            b"EIP712Domain(string name,string version,\
uint256 chainId,address verifyingContract)",
        );
        let domain_separator = keccak256(
            (
                domain_type_hash,
                keccak256(b"TychoRouter"),
                keccak256(b"1"),
                U256::from(chain_id),
                router_addr,
            )
                .abi_encode(),
        );

        let struct_hash = keccak256(
            (type_hash, U256::from(self.bps), fee_receiver, max_contrib, dl).abi_encode(),
        );

        let mut data = [0u8; 66];
        data[0] = 0x19;
        data[1] = 0x01;
        data[2..34].copy_from_slice(domain_separator.as_ref());
        data[34..66].copy_from_slice(struct_hash.as_ref());
        Ok(keccak256(data).0)
    }
}

// ---------------------------------------------------------------------------
// Private helpers for eip712_signing_hash
// ---------------------------------------------------------------------------

mod permit2_sol {
    use alloy::sol;

    sol! {
        struct PermitDetails {
            address token;
            uint160 amount;
            uint48 expiration;
            uint48 nonce;
        }
        struct PermitSingle {
            PermitDetails details;
            address spender;
            uint256 sigDeadline;
        }
    }
}

fn p2_bytes_to_address(
    b: &bytes::Bytes,
    field: &str,
) -> Result<alloy::primitives::Address, crate::error::FyndError> {
    let arr: [u8; 20] = b.as_ref().try_into().map_err(|_| {
        crate::error::FyndError::Protocol(format!(
            "expected 20-byte address for {field}, got {} bytes",
            b.len()
        ))
    })?;
    Ok(alloy::primitives::Address::from(arr))
}

fn p2_biguint_to_uint160(
    n: &num_bigint::BigUint,
) -> Result<alloy::primitives::Uint<160, 3>, crate::error::FyndError> {
    let bytes = n.to_bytes_be();
    if bytes.len() > 20 {
        return Err(crate::error::FyndError::Protocol(format!(
            "permit amount exceeds uint160 ({} bytes)",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 20];
    arr[20 - bytes.len()..].copy_from_slice(&bytes);
    Ok(alloy::primitives::Uint::<160, 3>::from_be_bytes(arr))
}

fn p2_biguint_to_uint48(
    n: &num_bigint::BigUint,
) -> Result<alloy::primitives::Uint<48, 1>, crate::error::FyndError> {
    let bytes = n.to_bytes_be();
    if bytes.len() > 6 {
        return Err(crate::error::FyndError::Protocol(format!(
            "permit value exceeds uint48 ({} bytes)",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 6];
    arr[6 - bytes.len()..].copy_from_slice(&bytes);
    Ok(alloy::primitives::Uint::<48, 1>::from_be_bytes(arr))
}

/// Options that instruct the server to return ABI-encoded calldata in the quote response.
///
/// Pass via [`QuoteOptions::with_encoding_options`] to opt into calldata generation. Without this,
/// the server returns routing information only and [`Quote::transaction`] will be `None`.
#[derive(Debug, Clone)]
pub struct EncodingOptions {
    pub(crate) slippage: f64,
    pub(crate) transfer_type: UserTransferType,
    pub(crate) permit: Option<PermitSingle>,
    pub(crate) permit2_signature: Option<Bytes>,
    pub(crate) client_fee_params: Option<ClientFeeParams>,
}

impl EncodingOptions {
    /// Create encoding options with the given slippage tolerance.
    ///
    /// `slippage` is a fraction (e.g. `0.005` for 0.5%). The transfer type defaults to
    /// [`UserTransferType::TransferFrom`].
    pub fn new(slippage: f64) -> Self {
        Self {
            slippage,
            transfer_type: UserTransferType::TransferFrom,
            permit: None,
            permit2_signature: None,
            client_fee_params: None,
        }
    }

    /// Enable Permit2 token transfer with a pre-computed EIP-712 signature.
    ///
    /// `signature` must be the 65-byte result of signing the Permit2 typed-data hash
    /// externally (ECDSA: 32-byte r, 32-byte s, 1-byte v).
    ///
    /// # Errors
    ///
    /// Returns [`crate::FyndError::Protocol`] if `signature` is not exactly 65 bytes.
    pub fn with_permit2(
        mut self,
        permit: PermitSingle,
        signature: bytes::Bytes,
    ) -> Result<Self, crate::error::FyndError> {
        if signature.len() != 65 {
            return Err(crate::error::FyndError::Protocol(format!(
                "Permit2 signature must be exactly 65 bytes, got {}",
                signature.len()
            )));
        }
        self.transfer_type = UserTransferType::TransferFromPermit2;
        self.permit = Some(permit);
        self.permit2_signature = Some(signature);
        Ok(self)
    }

    /// Use funds from the Tycho Router vault (no token transfer performed).
    pub fn with_vault_funds(mut self) -> Self {
        self.transfer_type = UserTransferType::UseVaultsFunds;
        self
    }

    /// Attach client fee configuration with a pre-signed EIP-712 signature.
    pub fn with_client_fee(mut self, params: ClientFeeParams) -> Self {
        self.client_fee_params = Some(params);
        self
    }
}

/// An encoded EVM transaction returned by the server when [`EncodingOptions`] was set.
///
/// Contains everything needed to submit the swap on-chain.
#[derive(Debug, Clone)]
pub struct Transaction {
    to: Bytes,
    value: BigUint,
    data: Vec<u8>,
}

impl Transaction {
    pub(crate) fn new(to: Bytes, value: BigUint, data: Vec<u8>) -> Self {
        Self { to, value, data }
    }

    /// Router contract address (20 raw bytes).
    pub fn to(&self) -> &Bytes {
        &self.to
    }

    /// Native value to send with the transaction (token units; usually `0` for ERC-20 swaps).
    pub fn value(&self) -> &BigUint {
        &self.value
    }

    /// ABI-encoded calldata.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

// ============================================================================
// ORDER SIDE
// ============================================================================

/// The direction of a swap order.
///
/// Currently only [`Sell`](Self::Sell) (exact-input) is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    /// Sell exactly the specified `amount` of `token_in` for as much `token_out` as possible.
    Sell,
}

// ============================================================================
// REQUEST TYPES
// ============================================================================

/// A single swap intent submitted to the Fynd solver.
///
/// Addresses are raw 20-byte values (`bytes::Bytes`). The amount is denominated
/// in the smallest unit of the input token (e.g. wei for ETH, atomic units for ERC-20).
pub struct Order {
    token_in: Bytes,
    token_out: Bytes,
    amount: BigUint,
    side: OrderSide,
    sender: Bytes,
    receiver: Option<Bytes>,
}

impl Order {
    /// Construct a new order.
    ///
    /// - `token_in`: 20-byte ERC-20 address of the token to sell.
    /// - `token_out`: 20-byte ERC-20 address of the token to receive.
    /// - `amount`: exact amount to sell (token units, not wei unless the token is WETH).
    /// - `side`: must be [`OrderSide::Sell`]; buy orders are not yet supported.
    /// - `sender`: 20-byte address of the wallet sending `token_in`.
    /// - `receiver`: 20-byte address that receives `token_out`. Defaults to `sender` if `None`.
    pub fn new(
        token_in: Bytes,
        token_out: Bytes,
        amount: BigUint,
        side: OrderSide,
        sender: Bytes,
        receiver: Option<Bytes>,
    ) -> Self {
        Self { token_in, token_out, amount, side, sender, receiver }
    }

    /// The address of the token being sold (20 raw bytes).
    pub fn token_in(&self) -> &Bytes {
        &self.token_in
    }

    /// The address of the token being bought (20 raw bytes).
    pub fn token_out(&self) -> &Bytes {
        &self.token_out
    }

    /// The amount to sell, in token units.
    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    /// Whether this is a sell (exact-input) or buy (exact-output) order.
    pub fn side(&self) -> OrderSide {
        self.side
    }

    /// The address that will send `token_in` (20 raw bytes).
    pub fn sender(&self) -> &Bytes {
        &self.sender
    }

    /// The address that will receive `token_out` (20 raw bytes), or `None` if it defaults to
    /// [`sender`](Self::sender).
    pub fn receiver(&self) -> Option<&Bytes> {
        self.receiver.as_ref()
    }
}

/// Optional parameters that tune solving behaviour for a [`QuoteParams`] request.
///
/// Build via the builder methods; unset options use server defaults.
#[derive(Default)]
pub struct QuoteOptions {
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) min_responses: Option<usize>,
    pub(crate) max_gas: Option<BigUint>,
    pub(crate) encoding_options: Option<EncodingOptions>,
}

impl QuoteOptions {
    /// Cap the solver's wall-clock budget to `ms` milliseconds.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    /// Return as soon as at least `n` solver pools have responded, rather than waiting for all.
    ///
    /// Use [`HealthStatus::num_solver_pools`] to discover how many pools are active before
    /// setting this value. Values exceeding the active pool count are clamped by the server.
    pub fn with_min_responses(mut self, n: usize) -> Self {
        self.min_responses = Some(n);
        self
    }

    /// Discard quotes whose estimated gas cost exceeds `gas`.
    pub fn with_max_gas(mut self, gas: BigUint) -> Self {
        self.max_gas = Some(gas);
        self
    }

    /// Request server-side calldata generation. The resulting [`Quote::transaction`] will be
    /// populated when this option is set.
    pub fn with_encoding_options(mut self, opts: EncodingOptions) -> Self {
        self.encoding_options = Some(opts);
        self
    }

    /// The configured timeout in milliseconds, or `None` if using the server default.
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    /// The configured minimum response count, or `None` if using the server default.
    pub fn min_responses(&self) -> Option<usize> {
        self.min_responses
    }

    /// The configured gas cap, or `None` if no cap was set.
    pub fn max_gas(&self) -> Option<&BigUint> {
        self.max_gas.as_ref()
    }
}

/// All inputs needed to call [`FyndClient::quote`](crate::FyndClient::quote).
pub struct QuoteParams {
    pub(crate) order: Order,
    pub(crate) options: QuoteOptions,
}

impl QuoteParams {
    /// Create a new request from a list of orders and optional solver options.
    pub fn new(order: Order, options: QuoteOptions) -> Self {
        Self { order, options }
    }
}

// ============================================================================
// RESPONSE TYPES
// ============================================================================

/// Which backend solver produced a given order quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The native Fynd solver.
    Fynd,
    /// The Turbine solver (integration in progress).
    Turbine,
}

/// High-level status of a single-order quote returned by the solver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStatus {
    /// A valid route was found and `route`, `amount_out`, and `gas_estimate` are populated.
    Success,
    /// No swap path exists between the requested token pair on any available pool.
    NoRouteFound,
    /// A path exists but available liquidity is too low for the requested amount.
    InsufficientLiquidity,
    /// The solver timed out before finding a route.
    Timeout,
    /// No solver workers are initialised yet (e.g. market data not loaded).
    NotReady,
}

/// Ethereum block at which a quote was computed.
///
/// Quotes are only valid for the block at which they were produced. Conditions may have changed
/// by the time you submit the transaction.
#[derive(Debug, Clone)]
pub struct BlockInfo {
    number: u64,
    hash: String,
    timestamp: u64,
}

impl BlockInfo {
    /// The block number.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The block hash as a hex string (e.g. `"0xabcd..."`).
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The block timestamp in Unix seconds.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub(crate) fn new(number: u64, hash: String, timestamp: u64) -> Self {
        Self { number, hash, timestamp }
    }
}

/// A single atomic swap on one liquidity pool within a [`Route`].
#[derive(Debug, Clone)]
pub struct Swap {
    component_id: String,
    protocol: String,
    token_in: Bytes,
    token_out: Bytes,
    amount_in: BigUint,
    amount_out: BigUint,
    gas_estimate: BigUint,
    #[allow(dead_code)]
    split: f64,
}

impl Swap {
    /// The identifier of the liquidity pool component (e.g. a pool address).
    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// The protocol identifier (e.g. `"uniswap_v3"`, `"vm:balancer"`).
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// Input token address (20 raw bytes).
    pub fn token_in(&self) -> &Bytes {
        &self.token_in
    }

    /// Output token address (20 raw bytes).
    pub fn token_out(&self) -> &Bytes {
        &self.token_out
    }

    /// Amount of `token_in` consumed by this swap (token units).
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// Amount of `token_out` produced by this swap (token units).
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Estimated gas units required to execute this swap.
    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        component_id: String,
        protocol: String,
        token_in: Bytes,
        token_out: Bytes,
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
}

/// An ordered sequence of swaps that together execute a complete token swap.
///
/// For multi-hop routes the output of each [`Swap`] is the input of the next.
#[derive(Debug, Clone)]
pub struct Route {
    swaps: Vec<Swap>,
}

impl Route {
    /// The ordered sequence of swaps to execute.
    pub fn swaps(&self) -> &[Swap] {
        &self.swaps
    }

    pub(crate) fn new(swaps: Vec<Swap>) -> Self {
        Self { swaps }
    }
}

/// The solver's response for a single order.
#[derive(Debug, Clone)]
pub struct Quote {
    order_id: String,
    status: QuoteStatus,
    backend: BackendKind,
    route: Option<Route>,
    amount_in: BigUint,
    amount_out: BigUint,
    gas_estimate: BigUint,
    amount_out_net_gas: BigUint,
    price_impact_bps: Option<i32>,
    block: BlockInfo,
    /// Output token address from the original order (20 raw bytes).
    /// Populated by `quote()` from the corresponding `Order`.
    token_out: Bytes,
    /// Receiver address from the original order (20 raw bytes).
    /// Defaults to `sender` if the order had no explicit receiver.
    /// Populated by `quote()` from the corresponding `Order`.
    receiver: Bytes,
    /// ABI-encoded on-chain transaction. Present only when [`EncodingOptions`] was set in the
    /// request via [`QuoteOptions::with_encoding_options`].
    transaction: Option<Transaction>,
    /// Wall-clock time the server spent solving this request, in milliseconds.
    /// Populated by [`FyndClient::quote`](crate::FyndClient::quote).
    pub(crate) solve_time_ms: u64,
}

impl Quote {
    /// The server-assigned order ID (UUID v4).
    pub fn order_id(&self) -> &str {
        &self.order_id
    }

    /// Whether the solver found a valid route for this order.
    pub fn status(&self) -> QuoteStatus {
        self.status
    }

    /// Which backend produced this quote.
    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// The route to execute, if [`status`](Self::status) is [`QuoteStatus::Success`].
    pub fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// The amount of `token_in` the solver expects to consume (token units).
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// The expected amount of `token_out` received after executing the route (token units).
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Estimated gas units required to execute the entire route.
    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    /// Amount out minus estimated gas cost, expressed in output token units.
    ///
    /// Computed server-side using the current gas price and the quote's implied
    /// exchange rate. This is the primary metric the solver uses to rank routes.
    pub fn amount_out_net_gas(&self) -> &BigUint {
        &self.amount_out_net_gas
    }

    /// Price impact in basis points (1 bps = 0.01%). May be `None` for quotes without a route.
    pub fn price_impact_bps(&self) -> Option<i32> {
        self.price_impact_bps
    }

    /// The Ethereum block at which this quote was computed.
    pub fn block(&self) -> &BlockInfo {
        &self.block
    }

    /// The `token_out` address from the originating [`Order`] (20 raw bytes).
    ///
    /// Populated by [`FyndClient::quote`](crate::FyndClient::quote) and used by
    /// [`FyndClient::execute`](crate::FyndClient::execute) to parse the settlement log.
    pub fn token_out(&self) -> &Bytes {
        &self.token_out
    }

    /// The receiver address from the originating [`Order`] (20 raw bytes).
    ///
    /// Defaults to `sender` when the order had no explicit receiver. Populated by
    /// [`FyndClient::quote`](crate::FyndClient::quote) and used by
    /// [`FyndClient::execute`](crate::FyndClient::execute) to verify the Transfer log recipient.
    pub fn receiver(&self) -> &Bytes {
        &self.receiver
    }

    /// The server-encoded on-chain transaction, present when [`EncodingOptions`] was set.
    ///
    /// Contains the router contract address, native value, and ABI-encoded calldata ready to
    /// submit. Returns `None` when no [`EncodingOptions`] were passed in the request.
    pub fn transaction(&self) -> Option<&Transaction> {
        self.transaction.as_ref()
    }

    /// Wall-clock time the server spent solving this request, in milliseconds.
    ///
    /// Populated by [`FyndClient::quote`](crate::FyndClient::quote). Returns `0` if not set.
    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        order_id: String,
        status: QuoteStatus,
        backend: BackendKind,
        route: Option<Route>,
        amount_in: BigUint,
        amount_out: BigUint,
        gas_estimate: BigUint,
        amount_out_net_gas: BigUint,
        price_impact_bps: Option<i32>,
        block: BlockInfo,
        token_out: Bytes,
        receiver: Bytes,
        transaction: Option<Transaction>,
    ) -> Self {
        Self {
            order_id,
            status,
            backend,
            route,
            amount_in,
            amount_out,
            gas_estimate,
            amount_out_net_gas,
            price_impact_bps,
            block,
            token_out,
            receiver,
            transaction,
            solve_time_ms: 0,
        }
    }
}

/// The solver's response to a [`QuoteParams`] request, containing quotes for every order.
#[derive(Debug)]
pub(crate) struct BatchQuote {
    quotes: Vec<Quote>,
}

impl BatchQuote {
    /// Quotes for each order, in the same order as the request.
    pub fn quotes(&self) -> &[Quote] {
        &self.quotes
    }

    pub(crate) fn new(quotes: Vec<Quote>) -> Self {
        Self { quotes }
    }
}

/// Health information from the Fynd RPC server's `/v1/health` endpoint.
#[derive(Debug)]
pub struct HealthStatus {
    healthy: bool,
    last_update_ms: u64,
    num_solver_pools: usize,
    derived_data_ready: bool,
    gas_price_age_ms: Option<u64>,
}

impl HealthStatus {
    /// `true` when the server has up-to-date market data and active solver pools.
    pub fn healthy(&self) -> bool {
        self.healthy
    }

    /// Milliseconds since the last market-data update. High values indicate stale data.
    pub fn last_update_ms(&self) -> u64 {
        self.last_update_ms
    }

    /// Number of active solver pool workers. Use this to set `QuoteOptions::with_min_responses`.
    pub fn num_solver_pools(&self) -> usize {
        self.num_solver_pools
    }

    /// Whether derived data has been computed at least once.
    ///
    /// This indicates overall readiness, not per-block freshness. Some algorithms
    /// require fresh derived data for each block — they are ready to receive orders
    /// but will wait for recomputation before solving.
    pub fn derived_data_ready(&self) -> bool {
        self.derived_data_ready
    }

    /// Time since last gas price update in milliseconds, if available.
    pub fn gas_price_age_ms(&self) -> Option<u64> {
        self.gas_price_age_ms
    }

    pub(crate) fn new(
        healthy: bool,
        last_update_ms: u64,
        num_solver_pools: usize,
        derived_data_ready: bool,
        gas_price_age_ms: Option<u64>,
    ) -> Self {
        Self { healthy, last_update_ms, num_solver_pools, derived_data_ready, gas_price_age_ms }
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::*;

    fn addr(bytes: &[u8; 20]) -> Bytes {
        Bytes::copy_from_slice(bytes)
    }

    #[test]
    fn order_new_and_getters() {
        let token_in = addr(&[0xaa; 20]);
        let token_out = addr(&[0xbb; 20]);
        let amount = BigUint::from(1_000_000u64);
        let sender = addr(&[0xcc; 20]);

        let order = Order::new(
            token_in.clone(),
            token_out.clone(),
            amount.clone(),
            OrderSide::Sell,
            sender.clone(),
            None,
        );

        assert_eq!(order.token_in(), &token_in);
        assert_eq!(order.token_out(), &token_out);
        assert_eq!(order.amount(), &amount);
        assert_eq!(order.sender(), &sender);
        assert!(order.receiver().is_none());
        assert_eq!(order.side(), OrderSide::Sell);
    }

    #[test]
    fn order_with_explicit_receiver() {
        let receiver = Bytes::copy_from_slice(&[0xdd; 20]);
        let order = Order::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(1u32),
            OrderSide::Sell,
            Bytes::copy_from_slice(&[0xcc; 20]),
            Some(receiver.clone()),
        );
        assert_eq!(order.receiver(), Some(&receiver));
    }

    #[test]
    fn quote_options_builder() {
        let opts = QuoteOptions::default()
            .with_timeout_ms(500)
            .with_min_responses(2)
            .with_max_gas(BigUint::from(1_000_000u64));

        assert_eq!(opts.timeout_ms(), Some(500));
        assert_eq!(opts.min_responses(), Some(2));
        assert_eq!(opts.max_gas(), Some(&BigUint::from(1_000_000u64)));
    }

    #[test]
    fn quote_options_default_all_none() {
        let opts = QuoteOptions::default();
        assert!(opts.timeout_ms().is_none());
        assert!(opts.min_responses().is_none());
        assert!(opts.max_gas().is_none());
    }

    #[test]
    fn encoding_options_with_permit2_sets_fields() {
        let token = Bytes::copy_from_slice(&[0xaa; 20]);
        let spender = Bytes::copy_from_slice(&[0xbb; 20]);
        let sig = Bytes::copy_from_slice(&[0xcc; 65]);
        let details = PermitDetails::new(
            token,
            BigUint::from(1_000u32),
            BigUint::from(9_999_999u32),
            BigUint::from(0u32),
        );
        let permit = PermitSingle::new(details, spender, BigUint::from(9_999_999u32));

        let opts = EncodingOptions::new(0.005)
            .with_permit2(permit, sig.clone())
            .unwrap();

        assert_eq!(opts.transfer_type, UserTransferType::TransferFromPermit2);
        assert!(opts.permit.is_some());
        assert_eq!(opts.permit2_signature.as_ref().unwrap(), &sig);
    }

    #[test]
    fn encoding_options_with_permit2_rejects_wrong_signature_length() {
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            BigUint::from(1_000u32),
            BigUint::from(9_999_999u32),
            BigUint::from(0u32),
        );
        let permit = PermitSingle::new(
            details,
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(9_999_999u32),
        );
        let bad_sig = Bytes::copy_from_slice(&[0xcc; 64]); // 64 bytes, not 65
        assert!(matches!(
            EncodingOptions::new(0.005).with_permit2(permit, bad_sig),
            Err(crate::error::FyndError::Protocol(_))
        ));
    }

    #[test]
    fn encoding_options_with_vault_funds_sets_variant() {
        let opts = EncodingOptions::new(0.005).with_vault_funds();
        assert_eq!(opts.transfer_type, UserTransferType::UseVaultsFunds);
        assert!(opts.permit.is_none());
        assert!(opts.permit2_signature.is_none());
    }

    fn sample_permit_single() -> PermitSingle {
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            BigUint::from(1_000u32),
            BigUint::from(9_999_999u32),
            BigUint::from(0u32),
        );
        PermitSingle::new(details, Bytes::copy_from_slice(&[0xbb; 20]), BigUint::from(9_999_999u32))
    }

    #[test]
    fn eip712_signing_hash_returns_32_bytes() {
        let permit = sample_permit_single();
        let permit2_addr = Bytes::copy_from_slice(&[0xcc; 20]);
        let hash = permit
            .eip712_signing_hash(1, &permit2_addr)
            .unwrap();
        assert_eq!(hash.len(), 32);
        // Non-zero: alloy should never hash to all-zeros for a real input
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn eip712_signing_hash_is_deterministic() {
        let permit2_addr = Bytes::copy_from_slice(&[0xcc; 20]);
        let h1 = sample_permit_single()
            .eip712_signing_hash(1, &permit2_addr)
            .unwrap();
        let h2 = sample_permit_single()
            .eip712_signing_hash(1, &permit2_addr)
            .unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn eip712_signing_hash_differs_by_chain_id() {
        let permit2_addr = Bytes::copy_from_slice(&[0xcc; 20]);
        let h1 = sample_permit_single()
            .eip712_signing_hash(1, &permit2_addr)
            .unwrap();
        let h137 = sample_permit_single()
            .eip712_signing_hash(137, &permit2_addr)
            .unwrap();
        assert_ne!(h1, h137);
    }

    #[test]
    fn eip712_signing_hash_invalid_permit2_address() {
        let permit = sample_permit_single();
        let bad_addr = Bytes::copy_from_slice(&[0xcc; 4]);
        assert!(matches!(
            permit.eip712_signing_hash(1, &bad_addr),
            Err(crate::error::FyndError::Protocol(_))
        ));
    }

    #[test]
    fn eip712_signing_hash_invalid_token_address() {
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 4]), // wrong length
            BigUint::from(1u32),
            BigUint::from(1u32),
            BigUint::from(0u32),
        );
        let permit =
            PermitSingle::new(details, Bytes::copy_from_slice(&[0xbb; 20]), BigUint::from(1u32));
        let permit2_addr = Bytes::copy_from_slice(&[0xcc; 20]);
        assert!(matches!(
            permit.eip712_signing_hash(1, &permit2_addr),
            Err(crate::error::FyndError::Protocol(_))
        ));
    }

    #[test]
    fn eip712_signing_hash_amount_exceeds_uint160() {
        // 21 bytes > 20 bytes (uint160 = 160 bits = 20 bytes)
        let oversized_amount = BigUint::from_bytes_be(&[0x01; 21]);
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            oversized_amount,
            BigUint::from(1u32),
            BigUint::from(0u32),
        );
        let permit =
            PermitSingle::new(details, Bytes::copy_from_slice(&[0xbb; 20]), BigUint::from(1u32));
        let permit2_addr = Bytes::copy_from_slice(&[0xcc; 20]);
        assert!(matches!(
            permit.eip712_signing_hash(1, &permit2_addr),
            Err(crate::error::FyndError::Protocol(_))
        ));
    }

    // -------------------------------------------------------------------------
    // ClientFeeParams Tests
    // -------------------------------------------------------------------------

    fn sample_fee_receiver() -> Bytes {
        Bytes::copy_from_slice(&[0x44; 20])
    }

    fn sample_router_address() -> Bytes {
        Bytes::copy_from_slice(&[0x33; 20])
    }

    fn sample_fee_params(bps: u16, receiver: Bytes) -> ClientFeeParams {
        ClientFeeParams::new(bps, receiver, BigUint::ZERO, 1_893_456_000)
    }

    #[test]
    fn client_fee_with_client_fee_sets_fields() {
        let fee = ClientFeeParams::new(
            100,
            sample_fee_receiver(),
            BigUint::from(500_000u64),
            1_893_456_000,
        );
        let opts = EncodingOptions::new(0.01).with_client_fee(fee);
        assert!(opts.client_fee_params.is_some());
        let stored = opts.client_fee_params.as_ref().unwrap();
        assert_eq!(stored.bps, 100);
        assert_eq!(stored.max_contribution, BigUint::from(500_000u64));
    }

    #[test]
    fn client_fee_signing_hash_returns_32_bytes() {
        let fee = sample_fee_params(100, sample_fee_receiver());
        let hash = fee
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        assert_eq!(hash.len(), 32);
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn client_fee_signing_hash_is_deterministic() {
        let fee = sample_fee_params(100, sample_fee_receiver());
        let h1 = fee
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        let h2 = fee
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn client_fee_signing_hash_differs_by_chain_id() {
        let fee = sample_fee_params(100, sample_fee_receiver());
        let h1 = fee
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        let h137 = fee
            .eip712_signing_hash(137, &sample_router_address())
            .unwrap();
        assert_ne!(h1, h137);
    }

    #[test]
    fn client_fee_signing_hash_differs_by_bps() {
        let h100 = sample_fee_params(100, sample_fee_receiver())
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        let h200 = sample_fee_params(200, sample_fee_receiver())
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        assert_ne!(h100, h200);
    }

    #[test]
    fn client_fee_signing_hash_differs_by_receiver() {
        let other_receiver = Bytes::copy_from_slice(&[0x55; 20]);
        let h1 = sample_fee_params(100, sample_fee_receiver())
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        let h2 = sample_fee_params(100, other_receiver)
            .eip712_signing_hash(1, &sample_router_address())
            .unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn client_fee_signing_hash_rejects_bad_receiver_address() {
        let bad_addr = Bytes::copy_from_slice(&[0x44; 4]);
        let fee = sample_fee_params(100, bad_addr);
        assert!(matches!(
            fee.eip712_signing_hash(1, &sample_router_address()),
            Err(crate::error::FyndError::Protocol(_))
        ));
    }

    #[test]
    fn client_fee_signing_hash_rejects_bad_router_address() {
        let bad_addr = Bytes::copy_from_slice(&[0x33; 4]);
        let fee = sample_fee_params(100, sample_fee_receiver());
        assert!(matches!(
            fee.eip712_signing_hash(1, &bad_addr),
            Err(crate::error::FyndError::Protocol(_))
        ));
    }
}
