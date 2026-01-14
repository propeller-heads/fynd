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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use chrono::NaiveDateTime;
    use num_bigint::BigUint;
    use tycho_simulation::tycho_core::{
        dto::ProtocolStateDelta,
        models::{protocol::ProtocolComponent, token::Token, Address, Chain},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        },
        Bytes,
    };

    use super::*;
    use crate::{
        feed::market_data::ComponentData,
        graph::{petgraph::PetgraphStableDiGraphManager, GraphManager},
    };

    // ==================== Mock ProtocolSim ====================

    /// Mock that multiplies input by a factor and tracks call count.
    /// Each call increments the multiplier, simulating state changes.
    #[derive(Debug, Clone)]
    struct MockProtocolSim {
        /// Output = input * multiplier
        multiplier: u32,
        /// Shared counter to track calls across clones
        call_count: std::sync::Arc<AtomicU32>,
    }

    impl MockProtocolSim {
        fn new(multiplier: u32) -> Self {
            Self { multiplier, call_count: std::sync::Arc::new(AtomicU32::new(0)) }
        }

        fn with_shared_counter(multiplier: u32, counter: std::sync::Arc<AtomicU32>) -> Self {
            Self { multiplier, call_count: counter }
        }
    }

    impl ProtocolSim for MockProtocolSim {
        fn fee(&self) -> f64 {
            0.003 // 0.3%
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            Ok(self.multiplier as f64)
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            _token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            self.call_count
                .fetch_add(1, Ordering::SeqCst);

            // Return amount * multiplier, and a new state with incremented multiplier
            let amount_out = &amount_in * self.multiplier;
            let new_state = Box::new(MockProtocolSim::with_shared_counter(
                self.multiplier + 1, // State changes: next call would use multiplier+1
                self.call_count.clone(),
            ));

            Ok(GetAmountOutResult::new(amount_out, BigUint::from(100_000u64), new_state))
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::from(u64::MAX), BigUint::from(u64::MAX)))
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError<String>> {
            Ok(())
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .map(|o| o.multiplier == self.multiplier)
                .unwrap_or(false)
        }
    }

    // ==================== Test Fixtures ====================

    fn token(addr: u8, symbol: &str) -> Token {
        Token {
            address: Address::from([addr; 20]),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn component(id: &str, tokens: &[Token]) -> ProtocolComponent {
        ProtocolComponent::new(
            id,
            "uniswap_v2",
            "swap",
            Chain::Ethereum,
            tokens
                .iter()
                .map(|t| t.address.clone())
                .collect(),
            vec![],
            HashMap::new(),
            Default::default(),
            Default::default(),
            NaiveDateTime::default(),
        )
    }

    /// Sets up market with components and a graph. Returns (market, graph_manager).
    fn setup_market(
        pools: Vec<(&str, Vec<Token>, u32)>, // (pool_id, tokens, multiplier)
    ) -> (SharedMarketData, PetgraphStableDiGraphManager) {
        let mut market = SharedMarketData::new();
        let mut topology = HashMap::new();

        for (pool_id, tokens, multiplier) in pools {
            let comp = component(pool_id, &tokens);
            let state = Box::new(MockProtocolSim::new(multiplier));
            let data = ComponentData { component: comp, state, tokens: tokens.clone() };

            topology.insert(
                pool_id.to_string(),
                tokens
                    .iter()
                    .map(|t| t.address.clone())
                    .collect(),
            );
            market.insert_component(data);
        }

        let mut graph_manager = PetgraphStableDiGraphManager::default();
        graph_manager.initialize_graph(&topology);

        (market, graph_manager)
    }

    // ==================== simulate_path Tests ====================

    #[test]
    fn simulate_path_single_hop() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, manager) =
            setup_market(vec![("pool1", vec![token_a.clone(), token_b.clone()], 2)]);

        let graph = manager.graph();
        let path = manager
            .find_paths(&token_a.address, &token_b.address, 0, 1)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let route = simulate_path(path, graph, &market, BigUint::from(100u64)).unwrap();

        assert_eq!(route.swaps.len(), 1);
        assert_eq!(route.swaps[0].amount_in, BigUint::from(100u64));
        assert_eq!(route.swaps[0].amount_out, BigUint::from(200u64)); // 100 * 2
        assert_eq!(route.swaps[0].component_id, "pool1");
    }

    #[test]
    fn simulate_path_multi_hop_chains_amounts() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market(vec![
            ("pool1", vec![token_a.clone(), token_b.clone()], 2), // A->B: *2
            ("pool2", vec![token_b.clone(), token_c.clone()], 3), // B->C: *3
        ]);

        let graph = manager.graph();
        let paths = manager
            .find_paths(&token_a.address, &token_c.address, 0, 2)
            .unwrap();
        let path = paths.into_iter().next().unwrap();

        let route = simulate_path(path, graph, &market, BigUint::from(10u64)).unwrap();

        assert_eq!(route.swaps.len(), 2);
        // First hop: 10 * 2 = 20
        assert_eq!(route.swaps[0].amount_out, BigUint::from(20u64));
        // Second hop: 20 * 3 = 60
        assert_eq!(route.swaps[1].amount_in, BigUint::from(20u64));
        assert_eq!(route.swaps[1].amount_out, BigUint::from(60u64));
    }

    #[test]
    fn simulate_path_same_pool_twice_uses_updated_state() {
        // Route: A -> B -> A through the same pool
        // First swap uses multiplier=2, second should use multiplier=3 (updated state)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market(vec![("pool1", vec![token_a.clone(), token_b.clone()], 2)]);

        let graph = manager.graph();

        // Manually construct path: A->B, B->A (same pool, both directions)
        let edge_ab = graph
            .edge_indices()
            .find(|&e| {
                let (src, dst) = graph.edge_endpoints(e).unwrap();
                graph[src] == token_a.address && graph[dst] == token_b.address
            })
            .unwrap();

        let edge_ba = graph
            .edge_indices()
            .find(|&e| {
                let (src, dst) = graph.edge_endpoints(e).unwrap();
                graph[src] == token_b.address && graph[dst] == token_a.address
            })
            .unwrap();

        let path = vec![edge_ab, edge_ba];
        let route = simulate_path(path, graph, &market, BigUint::from(10u64)).unwrap();

        assert_eq!(route.swaps.len(), 2);
        // First: 10 * 2 = 20
        assert_eq!(route.swaps[0].amount_out, BigUint::from(20u64));
        // Second: 20 * 3 = 60 (state updated, multiplier incremented)
        assert_eq!(route.swaps[1].amount_out, BigUint::from(60u64));
    }

    #[test]
    fn simulate_path_missing_token_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let (market, _) = setup_market(vec![("pool1", vec![token_a.clone(), token_b.clone()], 2)]);

        // Add token C to graph but not to market (A->B->C)
        let mut topology = market.component_topology();
        topology
            .insert("pool2".to_string(), vec![token_b.address.clone(), token_c.address.clone()]);
        let mut manager = PetgraphStableDiGraphManager::default();
        manager.initialize_graph(&topology);

        let graph = manager.graph();
        let path = manager
            .find_paths(&token_a.address, &token_c.address, 0, 2)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let result = simulate_path(path, graph, &market, BigUint::from(100u64));
        assert!(
            matches!(result, Err(AlgorithmError::Other(msg)) if msg.contains("token not found"))
        );
    }

    #[test]
    fn simulate_path_missing_component_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (mut market, manager) =
            setup_market(vec![("pool1", vec![token_a.clone(), token_b.clone()], 2)]);

        // Remove the component but keep tokens and graph
        market.remove_component(&"pool1".to_string());

        let graph = manager.graph();
        let path = manager
            .find_paths(&token_a.address, &token_b.address, 0, 1)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let result = simulate_path(path, graph, &market, BigUint::from(100u64));
        assert!(
            matches!(result, Err(AlgorithmError::Other(msg)) if msg.contains("component not found"))
        );
    }
}
