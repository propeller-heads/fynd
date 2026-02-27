use bytes::Bytes;
use num_bigint::BigUint;

// ============================================================================
// ORDER SIDE
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Sell,
}

// ============================================================================
// REQUEST TYPES
// ============================================================================

pub struct Order {
    token_in: Bytes,
    token_out: Bytes,
    amount: BigUint,
    side: OrderSide,
    sender: Bytes,
    receiver: Option<Bytes>,
}

impl Order {
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

    pub fn token_in(&self) -> &Bytes {
        &self.token_in
    }

    pub fn token_out(&self) -> &Bytes {
        &self.token_out
    }

    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    pub fn side(&self) -> OrderSide {
        self.side
    }

    pub fn sender(&self) -> &Bytes {
        &self.sender
    }

    pub fn receiver(&self) -> Option<&Bytes> {
        self.receiver.as_ref()
    }
}

#[derive(Default)]
pub struct QuoteOptions {
    pub timeout_ms: Option<u64>,
    pub min_responses: Option<usize>,
    pub max_gas: Option<BigUint>,
}

impl QuoteOptions {
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    pub fn with_min_responses(mut self, n: usize) -> Self {
        self.min_responses = Some(n);
        self
    }

    pub fn with_max_gas(mut self, gas: BigUint) -> Self {
        self.max_gas = Some(gas);
        self
    }
}

pub struct QuoteParams {
    pub orders: Vec<Order>,
    pub options: QuoteOptions,
}

// ============================================================================
// RESPONSE TYPES
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Fynd,
    Turbine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolutionStatus {
    Success,
    NoRouteFound,
    InsufficientLiquidity,
    Timeout,
    NotReady,
}

pub struct BlockInfo {
    number: u64,
    hash: String,
    timestamp: u64,
}

impl BlockInfo {
    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub(crate) fn new(number: u64, hash: String, timestamp: u64) -> Self {
        Self { number, hash, timestamp }
    }
}

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
    pub fn pool_id(&self) -> &str {
        &self.pool_id
    }

    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    pub fn token_in(&self) -> &Bytes {
        &self.token_in
    }

    pub fn token_out(&self) -> &Bytes {
        &self.token_out
    }

    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

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

pub struct Route {
    swaps: Vec<Swap>,
}

impl Route {
    pub fn swaps(&self) -> &[Swap] {
        &self.swaps
    }

    pub(crate) fn new(swaps: Vec<Swap>) -> Self {
        Self { swaps }
    }
}

pub struct OrderSolution {
    pub(crate) order_id: String,
    pub(crate) status: SolutionStatus,
    pub(crate) backend: BackendKind,
    pub(crate) route: Option<Route>,
    pub(crate) amount_in: BigUint,
    pub(crate) amount_out: BigUint,
    pub(crate) gas_estimate: BigUint,
    pub(crate) price_impact_bps: Option<i32>,
    pub(crate) block: BlockInfo,
}

impl OrderSolution {
    pub fn order_id(&self) -> &str {
        &self.order_id
    }

    pub fn status(&self) -> SolutionStatus {
        self.status
    }

    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    pub fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    pub fn gas_estimate(&self) -> &BigUint {
        &self.gas_estimate
    }

    pub fn price_impact_bps(&self) -> Option<i32> {
        self.price_impact_bps
    }

    pub fn block(&self) -> &BlockInfo {
        &self.block
    }
}

pub struct Quote {
    orders: Vec<OrderSolution>,
    total_gas_estimate: BigUint,
    solve_time_ms: u64,
}

impl Quote {
    pub fn orders(&self) -> &[OrderSolution] {
        &self.orders
    }

    pub fn total_gas_estimate(&self) -> &BigUint {
        &self.total_gas_estimate
    }

    pub fn solve_time_ms(&self) -> u64 {
        self.solve_time_ms
    }

    pub(crate) fn new(
        orders: Vec<OrderSolution>,
        total_gas_estimate: BigUint,
        solve_time_ms: u64,
    ) -> Self {
        Self { orders, total_gas_estimate, solve_time_ms }
    }
}

pub struct HealthStatus {
    healthy: bool,
    last_update_ms: u64,
    num_solver_pools: usize,
}

impl HealthStatus {
    pub fn healthy(&self) -> bool {
        self.healthy
    }

    pub fn last_update_ms(&self) -> u64 {
        self.last_update_ms
    }

    pub fn num_solver_pools(&self) -> usize {
        self.num_solver_pools
    }

    pub(crate) fn new(healthy: bool, last_update_ms: u64, num_solver_pools: usize) -> Self {
        Self { healthy, last_update_ms, num_solver_pools }
    }
}
