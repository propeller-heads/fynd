use alloy::primitives::{Address, U256};

/// An order to get a quote for.
#[derive(Debug, Clone)]
pub struct Order {
    pub token_in: Address,
    pub token_out: Address,
    /// Amount in token units.
    pub amount: U256,
    pub side: OrderSide,
    pub sender: Address,
    pub receiver: Option<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Sell,
}

/// A priced route returned by the solver, bound to a specific block.
///
/// Re-quote if submission is delayed by more than a block or two.
#[derive(Debug, Clone)]
pub struct OrderSolution {
    pub order_id: String,
    pub amount_in: U256,
    pub amount_out: U256,
    pub gas_estimate: U256,
    pub price_impact_bps: Option<i32>,
    pub block: BlockInfo,
    pub route: Option<Route>,
    /// Internal raw response needed to build `SignablePayload`.
    pub(crate) backend: SolutionBackend,
}

#[derive(Debug, Clone)]
pub(crate) enum SolutionBackend {
    Fynd { calldata: Vec<u8> },
}

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub number: u64,
    pub hash: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub swaps: Vec<Swap>,
}

#[derive(Debug, Clone)]
pub struct Swap {
    pub component_id: String,
    pub protocol: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
    pub gas_estimate: U256,
}
