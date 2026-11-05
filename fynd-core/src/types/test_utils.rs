//! Builders for quote types, for tests in this crate and in crates that depend on it.
//!
//! The router assembles [`OrderQuote`] from worker responses, so its constructor is
//! crate-internal. Deserializing one from JSON instead drops every `#[serde(skip)]` field —
//! `algorithm` and `no_route_cause` among them — which is what a test asserting on those
//! fields needs to set.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::Bytes;

use crate::types::{
    quote::{BlockInfo, OrderQuote, QuoteStatus, Route},
    SolveError,
};

/// Assembles an [`OrderQuote`] the way the router would.
///
/// Every field starts where a routed order sits when nothing interesting happened: zero
/// amounts, block 1, no route. Set the ones the test is about.
#[must_use]
pub struct OrderQuoteBuilder {
    order_id: String,
    status: QuoteStatus,
    algorithm: String,
    amounts: [u64; 4],
    block: BlockInfo,
    gas_price: Option<u64>,
    price_impact_bps: Option<i32>,
    route: Option<Route>,
    no_route_cause: Option<SolveError>,
    subsidy_amount: Option<u64>,
    sender: Bytes,
    receiver: Bytes,
}

impl OrderQuoteBuilder {
    /// Starts a quote for `order_id` that ended in `status`, solved by `algorithm`.
    pub fn new(order_id: &str, status: QuoteStatus, algorithm: &str) -> Self {
        Self {
            order_id: order_id.to_string(),
            status,
            algorithm: algorithm.to_string(),
            amounts: [0; 4],
            block: BlockInfo::new(1, "0x01".to_string(), 1),
            gas_price: None,
            price_impact_bps: None,
            route: None,
            no_route_cause: None,
            subsidy_amount: None,
            sender: Bytes::default(),
            receiver: Bytes::default(),
        }
    }

    /// Sets `amount_in`, `amount_out`, `gas_estimate` and `amount_out_net_gas`, in that order.
    pub fn amounts(mut self, amounts: [u64; 4]) -> Self {
        self.amounts = amounts;
        self
    }

    /// Sets the block this order priced against. Number 0 is the router's "no worker answered"
    /// placeholder.
    pub fn block(mut self, number: u64, hash: &str, timestamp: u64) -> Self {
        self.block = BlockInfo::new(number, hash.to_string(), timestamp);
        self
    }

    /// Sets the gas price, in wei.
    pub fn gas_price(mut self, gas_price: u64) -> Self {
        self.gas_price = Some(gas_price);
        self
    }

    /// Sets the price impact, in basis points.
    pub fn price_impact_bps(mut self, bps: i32) -> Self {
        self.price_impact_bps = Some(bps);
        self
    }

    /// Attaches the route the solver found.
    pub fn route(mut self, route: Route) -> Self {
        self.route = Some(route);
        self
    }

    /// Sets the address that sends the input. The encoder needs a 20-byte address.
    pub fn sender(mut self, sender: Bytes) -> Self {
        self.sender = sender;
        self
    }

    /// Sets the address that receives the output. The encoder needs a 20-byte address.
    pub fn receiver(mut self, receiver: Bytes) -> Self {
        self.receiver = receiver;
        self
    }

    /// Sets why the order found no route, which is what drives its failure reason.
    pub fn no_route_cause(mut self, cause: SolveError) -> Self {
        self.no_route_cause = Some(cause);
        self
    }

    /// Sets the subsidy amount the encoded solution carries. It does not change `amount_out`.
    pub fn subsidy_amount(mut self, amount: u64) -> Self {
        self.subsidy_amount = Some(amount);
        self
    }

    /// Returns the assembled quote.
    pub fn build(self) -> OrderQuote {
        let [amount_in, amount_out, gas_estimate, amount_out_net_gas] = self.amounts;
        let mut quote = OrderQuote::new(
            self.order_id,
            self.status,
            BigUint::from(amount_in),
            BigUint::from(amount_out),
            BigUint::from(gas_estimate),
            BigUint::from(amount_out_net_gas),
            self.block,
            self.algorithm,
            self.sender,
            self.receiver,
            "1".to_string(),
        );
        if let Some(route) = self.route {
            quote = quote.with_route(route);
        }
        if let Some(gas_price) = self.gas_price {
            quote = quote.with_gas_price(BigUint::from(gas_price));
        }
        if let Some(bps) = self.price_impact_bps {
            quote = quote.with_price_impact_bps(bps);
        }
        if self.no_route_cause.is_some() {
            quote.set_no_route_cause(self.no_route_cause);
        }
        quote.set_subsidy_amount(self.subsidy_amount.map(BigUint::from));
        quote
    }
}
