use num_bigint::BigUint;
use tycho_simulation::{
    tycho_common::{models::protocol::ProtocolComponent, simulation::protocol_sim::ProtocolSim},
    tycho_core::Bytes,
};

use crate::models::{GasPrice, Order, Route};

/// Core algorithm error types
#[derive(Debug)]
pub enum AlgorithmError {
    Config(String),
    InvalidInput(String),
    Computation(String),
    RouteNotFound(String),
    External(String),
}

impl std::fmt::Display for AlgorithmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
            Self::Computation(msg) => write!(f, "Computation failed: {}", msg),
            Self::RouteNotFound(msg) => write!(f, "Route not found: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for AlgorithmError {}

pub trait Algorithm {
    /// Create a new algorithm instance with specified maximum hops
    fn new(max_hops: usize) -> Self;

    /// Find the best route for the given order
    fn get_best_route(
        &self,
        order: &Order,
        gas_price: Option<&GasPrice>,
        token_out_price: Option<BigUint>, // Price in native token
    ) -> Option<Route>;

    /// Add market data
    fn add_market_data(
        &mut self,
        state_id: Bytes,
        component: ProtocolComponent,
        state: Box<dyn ProtocolSim>,
    );

    /// Remove market data
    fn remove_market_data(&mut self, state_id: Bytes, component: ProtocolComponent);

    /// Update existing market state with new data
    fn update_market_state(&mut self, state_id: Bytes, new_state: Box<dyn ProtocolSim>);
}
