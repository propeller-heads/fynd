use num_bigint::BigUint;
use tycho_execution::encoding::models::Swap;
use tycho_simulation::tycho_common::{models::token::Token, Bytes};

#[derive(Clone, Debug)]
pub struct GasPrice {
    /// Base fee per gas (EIP-1559) - the minimum fee required
    pub base_fee: BigUint,
    /// Maximum priority fee per gas (EIP-1559) - tip for miners/validators
    pub max_priority_fee: BigUint,
    /// Maximum fee per gas (EIP-1559) - total max willing to pay
    pub max_fee: BigUint,
    /// Legacy gas price for pre-EIP-1559 transactions
    pub legacy_gas_price: BigUint,
    /// Estimated gas price for current network conditions (effective gas price)
    pub estimated_gas_price: BigUint,
    /// Timestamp when this gas price was fetched
    pub timestamp: u64,
}

impl GasPrice {
    pub fn new(
        base_fee: BigUint,
        max_priority_fee: BigUint,
        max_fee: BigUint,
        legacy_gas_price: BigUint,
    ) -> Self {
        let estimated_gas_price = &base_fee + &max_priority_fee;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            base_fee,
            max_priority_fee,
            max_fee,
            legacy_gas_price,
            estimated_gas_price,
            timestamp,
        }
    }

    pub fn priority_fee_uint(&self) -> alloy::primitives::Uint<256, 4> {
        let bytes = self.max_priority_fee.to_bytes_be();
        alloy::primitives::Uint::from_be_slice(&bytes)
    }

    pub fn effective_gas_price(&self) -> &BigUint {
        &self.estimated_gas_price
    }

    pub fn is_stale(&self, threshold_secs: u64) -> bool {
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        current_timestamp.saturating_sub(self.timestamp) > threshold_secs
    }

    /// Create a simple gas price from legacy gas price (for testing/fallback)
    pub fn from_legacy(legacy_gas_price: BigUint) -> Self {
        Self::new(
            BigUint::from(0u64),      // No base fee in legacy
            legacy_gas_price.clone(), // Use legacy as priority fee
            legacy_gas_price.clone(), // Use legacy as max fee
            legacy_gas_price,
        )
    }
}

impl std::fmt::Display for GasPrice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GasPrice(base: {}, priority: {}, max: {}, effective: {})",
            self.base_fee, self.max_priority_fee, self.max_fee, self.estimated_gas_price
        )
    }
}

#[derive(Clone, Debug)]
pub struct Order {
    /// An external id for this order can be used to identify it if dealing with multiple orders.
    external_id: String,
    token_in: Token,
    token_out: Token,
    /// Only one of the amount_in or amount_out can be None (depending on the exact_out)
    amount_in: Option<BigUint>,
    amount_out: Option<BigUint>,
    exact_out: bool,
    /// Either in or out depending on the exact_out
    min_amount: BigUint,
    origin_address: Bytes,
    /// Receiver of the swap. Only pass if different from origin_address
    receiver: Option<Bytes>,
}

impl Order {
    pub fn new(
        external_id: String,
        token_in: Token,
        token_out: Token,
        amount_in: Option<BigUint>,
        amount_out: Option<BigUint>,
        exact_out: bool,
        min_amount: BigUint,
        origin_address: Bytes,
        receiver: Option<Bytes>,
    ) -> Self {
        Self {
            external_id,
            token_in,
            token_out,
            amount_in,
            amount_out,
            exact_out,
            min_amount,
            origin_address,
            receiver,
        }
    }

    pub fn external_id(&self) -> &str {
        &self.external_id
    }
    pub fn token_in(&self) -> &Token {
        &self.token_in
    }
    pub fn token_out(&self) -> &Token {
        &self.token_out
    }
    pub fn amount_in(&self) -> &Option<BigUint> {
        &self.amount_in
    }
    pub fn amount_out(&self) -> &Option<BigUint> {
        &self.amount_out
    }
    pub fn exact_out(&self) -> bool {
        self.exact_out
    }
    pub fn min_amount(&self) -> &BigUint {
        &self.min_amount
    }
    pub fn origin_address(&self) -> &Bytes {
        &self.origin_address
    }
    pub fn receiver(&self) -> &Option<Bytes> {
        &self.receiver
    }

    pub fn set_external_id(&mut self, external_id: String) {
        self.external_id = external_id;
    }
    pub fn set_token_in(&mut self, token_in: Token) {
        self.token_in = token_in;
    }
    pub fn set_token_out(&mut self, token_out: Token) {
        self.token_out = token_out;
    }
    pub fn set_amount_in(&mut self, amount_in: Option<BigUint>) {
        self.amount_in = amount_in;
    }
    pub fn set_amount_out(&mut self, amount_out: Option<BigUint>) {
        self.amount_out = amount_out;
    }
    pub fn set_exact_out(&mut self, exact_out: bool) {
        self.exact_out = exact_out;
    }
    pub fn set_min_amount(&mut self, min_amount: BigUint) {
        self.min_amount = min_amount;
    }
    pub fn set_origin_address(&mut self, origin_address: Bytes) {
        self.origin_address = origin_address;
    }
    pub fn set_receiver(&mut self, receiver: Option<Bytes>) {
        self.receiver = receiver;
    }
}

#[derive(Clone, Debug)]
pub struct Route {
    swaps: Vec<Swap>,
    token_in: Token,
    token_out: Token,
    amount_in: BigUint,
    amount_out: BigUint,
    price: BigUint,
    gas: BigUint,
}

impl Route {
    pub fn new(
        swaps: Vec<Swap>,
        token_in: Token,
        token_out: Token,
        amount_in: BigUint,
        amount_out: BigUint,
        price: BigUint,
        gas: BigUint,
    ) -> Self {
        Self { swaps, token_in, token_out, amount_in, amount_out, price, gas }
    }

    pub fn swaps(&self) -> &Vec<Swap> {
        &self.swaps
    }
    pub fn token_in(&self) -> &Token {
        &self.token_in
    }
    pub fn token_out(&self) -> &Token {
        &self.token_out
    }
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }
    pub fn price(&self) -> &BigUint {
        &self.price
    }
    pub fn gas(&self) -> &BigUint {
        &self.gas
    }

    pub fn set_swaps(&mut self, swaps: Vec<Swap>) {
        self.swaps = swaps;
    }
    pub fn set_token_in(&mut self, token_in: Token) {
        self.token_in = token_in;
    }
    pub fn set_token_out(&mut self, token_out: Token) {
        self.token_out = token_out;
    }
    pub fn set_amount_in(&mut self, amount_in: BigUint) {
        self.amount_in = amount_in;
    }
    pub fn set_amount_out(&mut self, amount_out: BigUint) {
        self.amount_out = amount_out;
    }
    pub fn set_price(&mut self, price: BigUint) {
        self.price = price;
    }
    pub fn set_gas(&mut self, gas: BigUint) {
        self.gas = gas;
    }

    fn get_amount_out(&self) -> (BigUint, BigUint) // (amount out, gas)
    {
        // Loop through the paths and calculate the consecutive amount outs
        todo!()
    }
}

/// Solver error types that aggregate lower-level errors  
#[derive(Debug)]
pub enum SolverError {
    Config(String),
    Algorithm(String),
    MarketData(String),
    Execution(String),
    External(String),
}

impl std::fmt::Display for SolverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::Algorithm(msg) => write!(f, "Algorithm error: {}", msg),
            Self::MarketData(msg) => write!(f, "Market data error: {}", msg),
            Self::Execution(msg) => write!(f, "Execution error: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for SolverError {}
