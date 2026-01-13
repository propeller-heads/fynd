//! Route-finding algorithms.
//!
//! This module defines the Algorithm trait and built-in implementations.
//! New algorithms can be added by implementing the trait.
//!
//! Algorithms are generic over their preferred graph type, allowing them to use
//! different graph crates (petgraph, custom, etc.) and leverage built-in algorithms.

pub mod most_liquid;
pub mod stats;

use std::{collections::HashMap, time::Duration};

pub use most_liquid::MostLiquidAlgorithm;
use num_bigint::BigUint;
use tycho_simulation::tycho_core::simulation::protocol_sim::ProtocolSim;

use crate::{
    feed::market_data::SharedMarketData,
    graph::{petgraph::StableDiGraph, GraphManager, Path},
    types::{solution::Order, Route},
    ProtocolSystem, Swap,
};

/// Trait for route-finding algorithms.
///
/// Algorithms are generic over their preferred graph type `G`, allowing them to:
/// - Use different graph crates (petgraph, custom, etc.)
/// - Leverage built-in algorithms from graph libraries
/// - Optimize their graph representation for their specific needs
///
/// # Implementation Notes
///
/// - Algorithms should respect the timeout from `timeout()`
/// - They should use `graph` for path finding (BFS/etc)
/// - They should use `market` to read component states for simulation
/// - They should NOT modify the graph or market data
pub trait Algorithm: Send + Sync {
    /// The graph type this algorithm uses.
    type GraphType: Send + Sync;

    /// The graph manager type for this algorithm.
    /// This allows the solver to automatically create the appropriate graph manager.
    type GraphManager: GraphManager<Self::GraphType> + Default;

    /// Returns the algorithm's name.
    fn name(&self) -> &str;

    /// Finds the best route for the given order.
    ///
    /// # Arguments
    ///
    /// * `graph` - The algorithm's preferred graph type (e.g., petgraph::Graph)
    /// * `market` - Reference to shared market data for state lookups
    /// * `order` - The order to solve
    ///
    /// # Returns
    ///
    /// The best route found, or an error if no route could be found.
    fn find_best_route(
        &self,
        graph: &Self::GraphManager,
        market: &SharedMarketData,
        order: &Order,
    ) -> Result<Route, AlgorithmError>;

    /// Returns whether this algorithm supports exact-out orders.
    fn supports_exact_out(&self) -> bool {
        false
    }

    /// Returns the maximum number of hops to search.
    fn max_hops(&self) -> usize {
        3
    }

    /// Returns the timeout for route finding.
    fn timeout(&self) -> Duration {
        Duration::from_millis(50)
    }
}

/// Errors that can occur during route finding.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AlgorithmError {
    /// No path exists between the tokens.
    #[error("no path found between {from} and {to}")]
    NoPath { from: String, to: String },

    /// Paths exist but none have sufficient liquidity.
    #[error("insufficient liquidity on all paths")]
    InsufficientLiquidity,

    /// Route finding timed out.
    #[error("timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// Exact-out not supported by this algorithm.
    #[error("exact-out orders not supported")]
    ExactOutNotSupported,

    /// Other algorithm-specific error.
    #[error("{0}")]
    Other(String),
}

/// Result of simulating a path.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// The execution-ready route with real amounts and gas
    pub route: Route,
    /// Input amount provided initially
    pub amount_in: BigUint,
    /// Output amount after all swaps
    pub amount_out: BigUint,
    /// Total gas estimate for the entire route
    pub total_gas: BigUint,
}

/// Simulates a path and converts it to a Route with real amounts and gas.
///
/// Takes ownership of the Path and produces an execution-ready Route by simulating
/// each hop using ProtocolSim. Returns both the Route and metadata needed for ranking.
///
/// For each hop:
/// 1. Calls `get_amount_out` on the component's state
/// 2. Stores the new_state for subsequent hops (handles same-pool-twice scenarios)
fn simulate_path(
    path: Path,
    graph: &StableDiGraph,
    market: &SharedMarketData,
    amount_in: BigUint,
) -> Result<SimulationResult, AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut swaps = Vec::with_capacity(path.len());
    let mut total_gas = BigUint::ZERO;

    // Track state overrides for pools we've already swapped through.
    let mut state_overrides: HashMap<&str, Box<dyn ProtocolSim>> = HashMap::new();

    for edge in path {
        let (in_node, out_node) = graph
            .edge_endpoints(edge)
            .ok_or_else(|| AlgorithmError::Other(format!("invalid edge: {:?}", edge)))?;

        let edge_data = &graph[edge];
        let address_in = &graph[in_node];
        let address_out = &graph[out_node];

        // Get token and component data for the simulation call
        let token_in = market
            .get_token(address_in)
            .ok_or_else(|| AlgorithmError::Other(format!("token not found: {:?}", address_in)))?;
        let token_out = market
            .get_token(address_out)
            .ok_or_else(|| AlgorithmError::Other(format!("token not found: {:?}", address_out)))?;
        let component_id = &edge_data.component_id;

        let component_data = market
            .get_component(component_id)
            .ok_or_else(|| {
                AlgorithmError::Other(format!("component not found: {}", component_id))
            })?;

        // Select the correct state for simulation, using override if we've swapped through this
        // pool, and otherwise the original state stored in market data
        // TODO - is this stable for the VM states?
        let state = state_overrides
            .get(component_id.as_str())
            .map(Box::as_ref)
            .unwrap_or(component_data.state.as_ref());

        // Simulate the swap
        let result = state
            .get_amount_out(current_amount.clone(), token_in, token_out)
            .map_err(|e| AlgorithmError::Other(format!("simulation error: {:?}", e)))?;

        // Get protocol for the swap
        let protocol: ProtocolSystem = component_data
            .component
            .protocol_system
            .as_str()
            .try_into()
            .map_err(|e| {
                AlgorithmError::Other(format!(
                    "invalid protocol system: {} ({})",
                    component_data.component.protocol_system, e
                ))
            })?;

        // Record the swap
        swaps.push(Swap {
            component_id: component_id.clone(),
            protocol,
            token_in: token_in.address.clone(),
            token_out: token_out.address.clone(),
            amount_in: current_amount.clone(),
            amount_out: result.amount.clone(),
            gas_estimate: result.gas.clone(),
        });

        // Store new state for the following hops and update running totals
        state_overrides.insert(component_id.as_str(), result.new_state);
        total_gas += result.gas;
        current_amount = result.amount;
    }

    Ok(SimulationResult {
        route: Route::new(swaps),
        amount_in,
        amount_out: current_amount,
        total_gas,
    })
}
