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
use tycho_simulation::{
    evm::{engine_db::tycho_db::PreCachedDB, protocol::vm::state::EVMPoolState},
    tycho_common::models::ComponentId,
    tycho_core::simulation::protocol_sim::ProtocolSim,
};

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

    /// Gas cost exceeds output amount.
    #[error("gas cost exceeds output amount")]
    GasExceedsOutput,

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

/// Simulates swaps along a path using each pool's `ProtocolSim::get_amount_out`.
/// Tracks intermediate state changes to handle routes that revisit the same pool.
fn simulate_path(
    path: Path,
    graph: &StableDiGraph,
    market: &SharedMarketData,
    amount_in: BigUint,
) -> Result<Route, AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut swaps = Vec::with_capacity(path.len());

    // Track state overrides for pools we've already swapped through.
    let mut native_state_overrides: HashMap<&ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
    let mut vm_state_override: Option<Box<dyn ProtocolSim>> = None;

    for edge in path {
        let (in_node, out_node) = graph
            .edge_endpoints(edge)
            .ok_or_else(|| AlgorithmError::Other(format!("invalid edge: {:?}", edge)))?;

        let address_in = &graph[in_node];
        let address_out = &graph[out_node];

        let edge_data = &graph[edge];

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

        let is_component_vm = component_data
            .state
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
            .is_some();

        // If the component is a VM, use the VM state override shared across all VM components
        // Otherwise, use the per-component native state overrides
        let state_override = if is_component_vm {
            vm_state_override.as_ref()
        } else {
            native_state_overrides.get(component_id)
        };

        let state = state_override
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
            gas_estimate: result.gas,
        });

        // Store new state as override for next hops
        if is_component_vm {
            vm_state_override = Some(result.new_state);
        } else {
            native_state_overrides.insert(component_id, result.new_state);
        }
        current_amount = result.amount;
    }

    Ok(Route::new(swaps))
}
