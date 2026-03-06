use bytes::Bytes;
use num_bigint::BigUint;

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

    /// Discard solutions whose estimated gas cost exceeds `gas`.
    pub fn with_max_gas(mut self, gas: BigUint) -> Self {
        self.max_gas = Some(gas);
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

/// Which backend solver produced a given [`OrderSolution`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The native Fynd solver.
    Fynd,
    /// The Turbine solver (integration in progress).
    Turbine,
}

/// High-level status of a single-order solution returned by the solver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolutionStatus {
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
    pool_id: String,
    protocol: String,
    token_in: Bytes,
    token_out: Bytes,
    amount_in: BigUint,
    amount_out: BigUint,
    gas_estimate: BigUint,
}

impl Swap {
    /// The identifier of the liquidity pool component (e.g. a pool address).
    pub fn pool_id(&self) -> &str {
        &self.pool_id
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

    pub(crate) fn new(
        pool_id: String,
        protocol: String,
        token_in: Bytes,
        token_out: Bytes,
        amount_in: BigUint,
        amount_out: BigUint,
        gas_estimate: BigUint,
    ) -> Self {
        Self { pool_id, protocol, token_in, token_out, amount_in, amount_out, gas_estimate }
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
    status: SolutionStatus,
    backend: BackendKind,
    route: Option<Route>,
    amount_in: BigUint,
    amount_out: BigUint,
    gas_estimate: BigUint,
    price_impact_bps: Option<i32>,
    block: BlockInfo,
    /// Output token address from the original order (20 raw bytes).
    /// Populated by `quote()` from the corresponding `Order`.
    token_out: Bytes,
    /// Receiver address from the original order (20 raw bytes).
    /// Defaults to `sender` if the order had no explicit receiver.
    /// Populated by `quote()` from the corresponding `Order`.
    receiver: Bytes,
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
    pub fn status(&self) -> SolutionStatus {
        self.status
    }

    /// Which backend produced this solution.
    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// The route to execute, if [`status`](Self::status) is [`SolutionStatus::Success`].
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

    /// Price impact in basis points (1 bps = 0.01%). May be `None` for failed solutions.
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

    /// Wall-clock time the server spent solving this request, in milliseconds.
    ///
    /// Populated by [`FyndClient::quote`](crate::FyndClient::quote). Returns `0` if not set.
    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        order_id: String,
        status: SolutionStatus,
        backend: BackendKind,
        route: Option<Route>,
        amount_in: BigUint,
        amount_out: BigUint,
        gas_estimate: BigUint,
        price_impact_bps: Option<i32>,
        block: BlockInfo,
        token_out: Bytes,
        receiver: Bytes,
    ) -> Self {
        Self {
            order_id,
            status,
            backend,
            route,
            amount_in,
            amount_out,
            gas_estimate,
            price_impact_bps,
            block,
            token_out,
            receiver,
            solve_time_ms: 0,
        }
    }
}

/// The solver's response to a [`QuoteParams`] request, containing solutions for every order.
#[derive(Debug)]
pub(crate) struct BatchQuote {
    quotes: Vec<Quote>,
    #[allow(dead_code)]
    total_gas_estimate: BigUint,
    #[allow(dead_code)]
    solve_time_ms: u64,
}

impl BatchQuote {
    /// Solutions for each order, in the same order as the request.
    pub fn quotes(&self) -> &[Quote] {
        &self.quotes
    }

    /// Aggregate estimated gas units for executing all solutions.
    #[allow(dead_code)]
    pub fn total_gas_estimate(&self) -> &BigUint {
        &self.total_gas_estimate
    }

    /// Wall-clock time the server took to compute this quote, in milliseconds.
    #[allow(dead_code)]
    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }

    pub(crate) fn new(quotes: Vec<Quote>, total_gas_estimate: BigUint, solve_time_ms: u64) -> Self {
        Self { quotes, total_gas_estimate, solve_time_ms }
    }
}

/// Health information from the Fynd RPC server's `/v1/health` endpoint.
#[derive(Debug)]
pub struct HealthStatus {
    healthy: bool,
    last_update_ms: u64,
    num_solver_pools: usize,
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

    pub(crate) fn new(healthy: bool, last_update_ms: u64, num_solver_pools: usize) -> Self {
        Self { healthy, last_update_ms, num_solver_pools }
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
}
