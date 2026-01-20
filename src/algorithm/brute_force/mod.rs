//! Brute-force algorithm implementation.
//!
//! This module provides a generic brute-force routing algorithm that:
//! 1. Maintains an internal graph of tokens connected by component edges
//! 2. Finds all paths using BFS (shorter paths first)
//! 3. Scores paths using a pluggable scorer
//! 4. Simulates paths in score order (best first)
//! 5. Returns the route with highest net output
//!
//! The algorithm is parameterized by a `PathScorer` which determines:
//! - What data is stored on graph edges
//! - How paths are scored and prioritized

pub mod graph;
pub mod scorer;
pub mod scorers;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use async_trait::async_trait;
pub use graph::{EdgeData, GraphManager, Path};
use metrics::{counter, histogram};
use num_bigint::{BigInt, BigUint};
use num_traits::ToPrimitive;
use petgraph::prelude::EdgeRef;
pub use scorer::PathScorer;
use tracing::{debug, instrument, trace};
use tycho_simulation::{
    evm::{engine_db::tycho_db::PreCachedDB, protocol::vm::state::EVMPoolState},
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::Address,
};

use super::{Algorithm, AlgorithmError, NoPathReason};
use crate::{
    feed::{
        events::MarketEvent,
        market_data::{SharedMarketData, SharedMarketDataRef},
    },
    types::{ComponentId, Order, Route},
    ProtocolSystem, Swap,
};

/// Brute-force algorithm that owns its graph internally.
///
/// This algorithm:
/// 1. Maintains a graph of tokens connected by component edges
/// 2. Finds all paths using BFS (shorter paths first)
/// 3. Scores paths using the provided scorer
/// 4. Simulates paths in score order (best first)
/// 5. Returns the route with highest net output
///
/// # Type Parameters
///
/// - `S`: The scorer type that determines edge data and scoring logic
pub struct BruteForceAlgorithm<S: PathScorer> {
    scorer: S,
    min_hops: usize,
    max_hops: usize,
    timeout: Duration,
    graph_manager: GraphManager<S::EdgeData>,
    initialized: bool,
}

impl<S: PathScorer> BruteForceAlgorithm<S> {
    /// Creates a new BruteForceAlgorithm with the given scorer and default settings.
    pub fn new(scorer: S) -> Self {
        Self {
            scorer,
            min_hops: 1,
            max_hops: 3,
            timeout: Duration::from_millis(50),
            graph_manager: GraphManager::new(),
            initialized: false,
        }
    }

    /// Creates a new BruteForceAlgorithm with custom settings.
    ///
    /// # Errors
    ///
    /// Returns `InvalidConfiguration` if:
    /// - `min_hops == 0` (at least one hop is required)
    /// - `min_hops > max_hops`
    pub fn with_config(
        scorer: S,
        min_hops: usize,
        max_hops: usize,
        timeout_ms: u64,
    ) -> Result<Self, AlgorithmError> {
        if min_hops == 0 {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: "min_hops must be at least 1".to_string(),
            });
        }
        if min_hops > max_hops {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: format!("min_hops ({min_hops}) cannot exceed max_hops ({max_hops})"),
            });
        }
        Ok(Self {
            scorer,
            min_hops,
            max_hops,
            timeout: Duration::from_millis(timeout_ms),
            graph_manager: GraphManager::new(),
            initialized: false,
        })
    }

    /// Adds a component to the graph with bidirectional edges.
    ///
    /// This is a convenience method that delegates to the graph manager.
    /// Used by test utilities to build graphs manually.
    #[cfg(test)]
    pub(crate) fn add_component(&mut self, component_id: &ComponentId, tokens: &[Address]) {
        self.graph_manager
            .add_component(component_id, tokens);
    }

    /// Updates edge weights for a component using the scorer (sync version).
    /// Takes a reference to market data directly - no locking.
    fn update_edge_weights(&mut self, component_id: &ComponentId, market: &SharedMarketData) {
        let Some(edge_indices) = self
            .graph_manager
            .get_component_edges(component_id)
        else {
            return;
        };
        let edge_indices: Vec<_> = edge_indices.to_vec();

        for edge_idx in edge_indices {
            let Some((source, target)) = self
                .graph_manager
                .edge_endpoints(edge_idx)
            else {
                continue;
            };
            let token_in = self
                .graph_manager
                .node_address(source)
                .clone();
            let token_out = self
                .graph_manager
                .node_address(target)
                .clone();

            let edge_data = self
                .scorer
                .create_edge_data(market, component_id, &token_in, &token_out)
                .ok();

            if let Some(edge) = self
                .graph_manager
                .edge_weight_mut(edge_idx)
            {
                edge.data = edge_data;
            }
        }
    }

    /// Updates edge weights for a component using the scorer (async version).
    /// Acquires the market lock internally and drops it ASAP.
    /// Used during initialization when we need to process many components.
    async fn update_edge_weights_async(
        &mut self,
        component_id: &ComponentId,
        market: SharedMarketDataRef,
    ) {
        let Some(edge_indices) = self
            .graph_manager
            .get_component_edges(component_id)
        else {
            return;
        };
        let edge_indices: Vec<_> = edge_indices.to_vec();

        // Collect token addresses first (no lock needed)
        let token_pairs: Vec<_> = edge_indices
            .iter()
            .filter_map(|&edge_idx| {
                let (source, target) = self
                    .graph_manager
                    .edge_endpoints(edge_idx)?;
                let token_in = self
                    .graph_manager
                    .node_address(source)
                    .clone();
                let token_out = self
                    .graph_manager
                    .node_address(target)
                    .clone();
                Some((edge_idx, token_in, token_out))
            })
            .collect();

        // Acquire lock once for all edges of this component
        let edge_data_results: Vec<_> = {
            let market_guard = market.read().await;
            token_pairs
                .iter()
                .map(|(_, token_in, token_out)| {
                    self.scorer
                        .create_edge_data(&market_guard, component_id, token_in, token_out)
                        .ok()
                })
                .collect()
        };
        // Lock is dropped here

        // Apply edge data (no lock needed)
        for ((edge_idx, _, _), edge_data) in token_pairs
            .iter()
            .zip(edge_data_results)
        {
            if let Some(edge) = self
                .graph_manager
                .edge_weight_mut(*edge_idx)
            {
                edge.data = edge_data;
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Path finding
    // ─────────────────────────────────────────────────────────────────────────

    /// Finds all paths between two tokens using BFS.
    #[instrument(level = "debug", skip(self))]
    fn find_paths(
        &self,
        from: &Address,
        to: &Address,
    ) -> Result<Vec<Path<'_, S::EdgeData>>, AlgorithmError> {
        let from_idx = self
            .graph_manager
            .get_node(from)
            .ok_or(AlgorithmError::NoPath {
                from: from.clone(),
                to: to.clone(),
                reason: NoPathReason::SourceTokenNotInGraph,
            })?;

        let to_idx = self
            .graph_manager
            .get_node(to)
            .ok_or(AlgorithmError::NoPath {
                from: from.clone(),
                to: to.clone(),
                reason: NoPathReason::DestinationTokenNotInGraph,
            })?;

        let mut paths = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back((from_idx, Path::new()));

        let graph = self.graph_manager.graph();

        while let Some((current_node, current_path)) = queue.pop_front() {
            if current_path.len() >= self.max_hops {
                continue;
            }

            for edge in graph.edges(current_node) {
                let next_node = edge.target();

                let mut new_path = current_path.clone();
                new_path.add_hop(&graph[current_node], edge.weight(), &graph[next_node]);

                if next_node == to_idx && new_path.len() >= self.min_hops {
                    paths.push(new_path.clone());
                }

                queue.push_back((next_node, new_path));
            }
        }

        Ok(paths)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Simulation
    // ─────────────────────────────────────────────────────────────────────────

    /// Simulates swaps along a path using each pool's `ProtocolSim::get_amount_out`.
    #[instrument(level = "trace", skip(path, market), fields(hop_count = path.len()))]
    fn simulate_path(
        path: Path<S::EdgeData>,
        market: &SharedMarketData,
        amount_in: BigUint,
    ) -> Result<Route, AlgorithmError> {
        let mut current_amount = amount_in.clone();
        let mut swaps = Vec::with_capacity(path.len());

        // Track state overrides for pools we've already swapped through.
        let mut native_state_overrides: HashMap<&ComponentId, Box<dyn ProtocolSim>> =
            HashMap::new();
        let mut vm_state_override: Option<Box<dyn ProtocolSim>> = None;

        for (address_in, edge_data, address_out) in path.iter() {
            let token_in = market
                .get_token(address_in)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: format!("{:?}", address_in),
                })?;
            let token_out = market
                .get_token(address_out)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: format!("{:?}", address_out),
                })?;

            let component_id = &edge_data.component_id;
            let component = market
                .get_component(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "component",
                    id: component_id.clone(),
                })?;
            let component_state = market
                .get_simulation_state(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: component_id.clone(),
                })?;

            let is_component_vm = component_state
                .as_any()
                .downcast_ref::<EVMPoolState<PreCachedDB>>()
                .is_some();

            let state_override = if is_component_vm {
                vm_state_override.as_ref()
            } else {
                native_state_overrides.get(component_id)
            };

            let state = state_override
                .map(Box::as_ref)
                .unwrap_or(component_state);

            let result = state
                .get_amount_out(current_amount.clone(), token_in, token_out)
                .map_err(|e| AlgorithmError::Other(format!("simulation error: {:?}", e)))?;

            let protocol: ProtocolSystem = component
                .protocol_system
                .as_str()
                .try_into()
                .map_err(|e| {
                    AlgorithmError::Other(format!(
                        "invalid protocol system: {} ({})",
                        component.protocol_system, e
                    ))
                })?;

            swaps.push(Swap {
                component_id: component_id.clone(),
                protocol,
                token_in: token_in.address.clone(),
                token_out: token_out.address.clone(),
                amount_in: current_amount.clone(),
                amount_out: result.amount.clone(),
                gas_estimate: result.gas,
            });

            if is_component_vm {
                vm_state_override = Some(result.new_state);
            } else {
                native_state_overrides.insert(component_id, result.new_state);
            }
            current_amount = result.amount;
        }

        // Calculate net amount out
        let output_amount = swaps
            .last()
            .map(|s| s.amount_out.clone())
            .unwrap_or_else(|| BigUint::ZERO);
        let total_gas: BigUint = swaps
            .iter()
            .map(|s| &s.gas_estimate)
            .sum();
        let gas_price = market.gas_price().effective_gas_price();
        let gas_cost_wei = total_gas * gas_price;
        let gas_cost_out = gas_cost_wei * 1u32; // Placeholder until conversion is implemented

        let net_amount_out = BigInt::from(output_amount) - BigInt::from(gas_cost_out);

        Ok(Route::new(swaps, net_amount_out))
    }
}

impl<S: PathScorer> Default for BruteForceAlgorithm<S>
where
    S: Default,
{
    fn default() -> Self {
        Self::new(S::default())
    }
}

#[async_trait]
impl<S: PathScorer> Algorithm for BruteForceAlgorithm<S> {
    fn name(&self) -> &str {
        self.scorer.name()
    }

    async fn initialize(&mut self, market: SharedMarketDataRef) {
        // Acquire read lock and extract what we need
        let (topology, component_ids) = {
            let market_guard = market.read().await;
            let topology = market_guard.component_topology();
            let component_ids: Vec<_> = topology.keys().cloned().collect();
            (topology, component_ids)
        };
        // Lock is dropped here

        // Reset graph manager with new topology
        self.graph_manager = GraphManager::new();
        self.graph_manager.initialize(&topology);

        // Populate edge weights using scorer (needs market lock per component)
        for component_id in component_ids {
            self.update_edge_weights_async(&component_id, market.clone())
                .await;
        }

        self.initialized = true;
    }

    async fn handle_events(
        &mut self,
        events: &[MarketEvent],
        market: SharedMarketDataRef,
    ) -> Result<(), AlgorithmError> {
        if events.is_empty() {
            return Ok(());
        }

        // Step 1: Collect all component IDs that need weight updates and graph changes
        let mut components_to_add: Vec<(ComponentId, Vec<Address>)> = Vec::new();
        let mut components_to_remove: Vec<ComponentId> = Vec::new();
        let mut components_to_update: HashSet<ComponentId> = HashSet::new();

        for event in events {
            match event {
                MarketEvent::MarketUpdated {
                    added_components,
                    removed_components,
                    updated_components,
                } => {
                    for component_id in removed_components {
                        components_to_remove.push(component_id.clone());
                    }
                    for (component_id, tokens) in added_components {
                        components_to_add.push((component_id.clone(), tokens.clone()));
                        components_to_update.insert(component_id.clone());
                    }
                    for component_id in updated_components {
                        components_to_update.insert(component_id.clone());
                    }
                }
                MarketEvent::GasPriceUpdated { .. } => {
                    // No graph changes needed
                }
            }
        }

        // Step 2: Apply graph topology changes (no lock needed)
        for component_id in &components_to_remove {
            self.graph_manager
                .remove_component(component_id);
            // Don't try to update weights for removed components
            components_to_update.remove(component_id);
        }
        for (component_id, tokens) in &components_to_add {
            self.graph_manager
                .add_component(component_id, tokens);
        }

        // Step 3: Acquire lock once, extract minimal subset, drop lock
        if !components_to_update.is_empty() {
            let local_market = {
                let market_guard = market.read().await;
                market_guard.extract_subset(&components_to_update)
            };
            // Lock is dropped here

            // Step 4: Update edge weights using local copy (no lock needed)
            for component_id in &components_to_update {
                self.update_edge_weights(component_id, &local_market);
            }
        }

        Ok(())
    }

    #[instrument(level = "debug", skip_all, fields(order_id = %order.id))]
    async fn find_best_route(
        &self,
        market: SharedMarketDataRef,
        order: &Order,
    ) -> Result<Route, AlgorithmError> {
        let start = Instant::now();

        // Exact-out isn't supported yet
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let amount_in = order.amount.clone();

        // Step 1: Find all paths using BFS (no lock needed - uses internal graph)
        let all_paths = self.find_paths(&order.token_in, &order.token_out)?;

        let paths_candidates = all_paths.len();
        if paths_candidates == 0 {
            return Err(AlgorithmError::NoPath {
                from: order.token_in.clone(),
                to: order.token_out.clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        // Step 2: Score and sort all paths (no lock needed - uses edge data)
        let mut scored_paths: Vec<(Path<S::EdgeData>, f64)> = all_paths
            .into_iter()
            .filter_map(|path| {
                let score = self.scorer.score_path(&path)?;
                Some((path, score))
            })
            .collect();

        scored_paths.sort_by(|(_, a_score), (_, b_score)| {
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let paths_to_simulate = scored_paths.len();
        let scoring_failures = paths_candidates - paths_to_simulate;
        if paths_to_simulate == 0 {
            return Err(AlgorithmError::NoPath {
                from: order.token_in.clone(),
                to: order.token_out.clone(),
                reason: NoPathReason::NoScorablePaths,
            });
        }

        let mut paths_simulated = 0usize;
        let mut simulation_failures = 0usize;
        let mut best: Option<Route> = None;
        let timeout_ms = self.timeout.as_millis() as u64;

        // Step 3: Simulate all paths - acquire lock for market data access
        // We hold the lock during simulation since each path needs continuous market access
        let (block_number, path_desc) = {
            let market_guard = market.read().await;

            for (edge_path, _) in scored_paths {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                if elapsed_ms > timeout_ms {
                    break;
                }

                let route = match Self::simulate_path(edge_path, &market_guard, amount_in.clone()) {
                    Ok(r) => r,
                    Err(e) => {
                        trace!(error = %e, "simulation failed for path");
                        simulation_failures += 1;
                        continue;
                    }
                };

                if best
                    .as_ref()
                    .map(|b| route.net_amount_out > b.net_amount_out)
                    .unwrap_or(true)
                {
                    best = Some(route);
                }

                paths_simulated += 1;
            }

            // Extract data for logging before releasing lock
            // TODO: Make BlockInfo access atomic so we don't need the lock for this
            let block_number = market_guard
                .last_updated()
                .map(|b| b.number);
            let path_desc = best
                .as_ref()
                .map(|route| route.path_description(market_guard.token_registry_ref()));

            (block_number, path_desc)
        };
        // Lock is dropped here

        // Log solve result (no lock needed)
        let solve_time_ms = start.elapsed().as_millis() as u64;
        let coverage_pct = if paths_to_simulate == 0 {
            100.0
        } else {
            (paths_simulated as f64 / paths_to_simulate as f64) * 100.0
        };

        // Record metrics
        counter!("algorithm.scoring_failures").increment(scoring_failures as u64);
        counter!("algorithm.simulation_failures").increment(simulation_failures as u64);
        histogram!("algorithm.simulation_coverage_pct").record(coverage_pct);

        match &best {
            Some(route) => {
                let protocols = route
                    .swaps
                    .as_slice()
                    .iter()
                    .map(|s| s.protocol)
                    .collect::<Vec<_>>();

                let price = amount_in
                    .to_f64()
                    .filter(|&v| v > 0.0)
                    .and_then(|amt_in| {
                        route
                            .net_amount_out
                            .to_f64()
                            .map(|amt_out| amt_out / amt_in)
                    })
                    .unwrap_or(f64::NAN);

                debug!(
                    solve_time_ms,
                    block_number,
                    paths_candidates,
                    paths_to_simulate,
                    paths_simulated,
                    simulation_failures,
                    simulation_coverage_pct = coverage_pct,
                    path = ?path_desc,
                    amount_in = %amount_in,
                    net_amount_out = %route.net_amount_out,
                    price_out_per_in = price,
                    hop_count = route.swaps.len(),
                    protocols = ?protocols,
                    "route found"
                );
            }
            None => {
                debug!(
                    solve_time_ms,
                    block_number,
                    paths_candidates,
                    paths_to_simulate,
                    paths_simulated,
                    simulation_failures,
                    simulation_coverage_pct = coverage_pct,
                    "no viable route"
                );
            }
        }

        best.ok_or({
            if solve_time_ms > timeout_ms {
                AlgorithmError::Timeout { elapsed_ms: solve_time_ms }
            } else {
                AlgorithmError::InsufficientLiquidity
            }
        })
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn max_hops(&self) -> usize {
        self.max_hops
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        algorithm::test_utils::{
            component,
            fixtures::{addrs, make_algo_graph},
            order, setup_market, token, MockProtocolSim, ONE_ETH,
        },
        types::OrderSide,
    };

    // ==================== find_paths Tests ====================

    #[test]
    fn find_paths_linear_one_hop() {
        let (a, b, _, _) = addrs();
        // Use max_hops=1 to avoid cyclic paths
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 1, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab", &[&a, &b])]);

        let paths = algo.find_paths(&a, &b).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].len(), 1);
    }

    #[test]
    fn find_paths_linear_two_hops() {
        let (a, b, c, _) = addrs();
        // Use max_hops=2 to find exactly the 2-hop path
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 2, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab", &[&a, &b]), ("bc", &[&b, &c])]);

        let paths = algo.find_paths(&a, &c).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].len(), 2);
    }

    #[test]
    fn find_paths_parallel_edges() {
        let (a, b, _, _) = addrs();
        // Use max_hops=1 to find only direct paths
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 1, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab1", &[&a, &b]), ("ab2", &[&a, &b]), ("ab3", &[&a, &b])]);

        let paths = algo.find_paths(&a, &b).unwrap();
        assert_eq!(paths.len(), 3); // 3 parallel edges
    }

    #[test]
    fn find_paths_diamond() {
        let (a, b, c, d) = addrs();
        // Use max_hops=2 to find the two 2-hop paths
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 2, 50).unwrap();
        make_algo_graph(
            &mut algo,
            &[("ab", &[&a, &b]), ("ac", &[&a, &c]), ("bd", &[&b, &d]), ("cd", &[&c, &d])],
        );

        let paths = algo.find_paths(&a, &d).unwrap();
        assert_eq!(paths.len(), 2); // A->B->D and A->C->D
    }

    #[test]
    fn find_paths_source_not_in_graph() {
        let (a, b, c, _) = addrs();
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 1, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab", &[&a, &b])]);

        let result = algo.find_paths(&c, &b);
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::SourceTokenNotInGraph, .. })
        ));
    }

    #[test]
    fn find_paths_dest_not_in_graph() {
        let (a, b, c, _) = addrs();
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 1, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab", &[&a, &b])]);

        let result = algo.find_paths(&a, &c);
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::DestinationTokenNotInGraph, .. })
        ));
    }

    #[test]
    fn find_paths_respects_max_hops() {
        let (a, b, c, d) = addrs();
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 2, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab", &[&a, &b]), ("bc", &[&b, &c]), ("cd", &[&c, &d])]);

        // A->D requires 3 hops, but max_hops=2
        let paths = algo.find_paths(&a, &d).unwrap();
        assert!(paths.is_empty());
    }

    #[test]
    fn find_paths_respects_min_hops() {
        let (a, b, c, _) = addrs();
        // min_hops=2, max_hops=2 to get exactly the 2-hop path
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 2, 2, 50).unwrap();
        make_algo_graph(&mut algo, &[("ab", &[&a, &b]), ("bc", &[&b, &c]), ("ac", &[&a, &c])]);

        // A->C has a 1-hop path (direct) and 2-hop path (via B)
        // With min_hops=2, only 2-hop should be found
        let paths = algo.find_paths(&a, &c).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].len(), 2);
    }

    // ==================== Configuration Tests ====================

    #[test]
    fn config_rejects_zero_min_hops() {
        let result = BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 0, 3, 50);
        assert!(matches!(result, Err(AlgorithmError::InvalidConfiguration { .. })));
    }

    #[test]
    fn config_rejects_min_greater_than_max() {
        let result = BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 4, 3, 50);
        assert!(matches!(result, Err(AlgorithmError::InvalidConfiguration { .. })));
    }

    // ==================== Integration Tests ====================

    #[tokio::test]
    async fn find_best_route_simple() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let market = setup_market(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2))]);

        // Use max_hops=1 to prevent cyclic paths
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 1, 50).unwrap();
        algo.initialize(market.clone()).await;

        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let route = algo
            .find_best_route(market, &order)
            .await
            .unwrap();

        assert_eq!(route.swaps.len(), 1);
        assert_eq!(route.swaps[0].component_id, "pool_ab");
    }

    #[tokio::test]
    async fn find_best_route_two_hops() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        let market = setup_market(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(2)),
        ]);

        // Use max_hops=2 to find exactly the 2-hop path
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 2, 50).unwrap();
        algo.initialize(market.clone()).await;

        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);
        let route = algo
            .find_best_route(market, &order)
            .await
            .unwrap();

        assert_eq!(route.swaps.len(), 2);
    }

    #[tokio::test]
    async fn find_best_route_selects_best_parallel() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        // Pool with spot_price=3 should win over spot_price=2
        let market = setup_market(vec![
            ("pool_good", &token_a, &token_b, MockProtocolSim::new(3)),
            ("pool_bad", &token_a, &token_b, MockProtocolSim::new(2)),
        ]);

        // Use max_hops=1 to prevent cyclic paths
        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 1, 50).unwrap();
        algo.initialize(market.clone()).await;

        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let route = algo
            .find_best_route(market, &order)
            .await
            .unwrap();

        assert_eq!(route.swaps.len(), 1);
        assert_eq!(route.swaps[0].component_id, "pool_good");
    }

    #[tokio::test]
    async fn find_best_route_no_path() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        // A-B pool only, no connection to C
        let market = setup_market(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2))]);

        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 2, 50).unwrap();
        algo.initialize(market.clone()).await;

        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);
        let result = algo
            .find_best_route(market, &order)
            .await;

        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[tokio::test]
    async fn handle_event_adds_components() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        // Start with just A-B
        let market = setup_market(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2))]);

        let mut algo =
            BruteForceAlgorithm::with_config(scorers::MostLiquidScorer::new(), 1, 2, 50).unwrap();
        algo.initialize(market.clone()).await;

        // A->C should fail initially
        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);
        assert!(algo
            .find_best_route(market.clone(), &order)
            .await
            .is_err());

        // Add B-C pool via event
        {
            let market_clone = market.clone();
            let mut market_write = market_clone.write().await;

            let comp = component("pool_bc", &[token_b.clone(), token_c.clone()]);
            market_write.upsert_components(std::iter::once(comp));
            market_write.update_states([(
                "pool_bc".to_string(),
                Box::new(MockProtocolSim::new(2)) as Box<dyn ProtocolSim>,
            )]);
            market_write.upsert_tokens([token_b.clone(), token_c.clone()]);
        }

        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([(
                "pool_bc".to_string(),
                vec![token_b.address.clone(), token_c.address.clone()],
            )]),
            removed_components: vec![],
            updated_components: vec![],
        };

        algo.handle_events(&[event], market.clone())
            .await
            .unwrap();

        // Now A->C should work (A->B->C)
        let route = algo
            .find_best_route(market, &order)
            .await
            .unwrap();
        assert_eq!(route.swaps.len(), 2);
    }

    #[tokio::test]
    async fn handle_event_removes_components() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        let market = setup_market(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(2)),
        ]);

        let mut algo = BruteForceAlgorithm::new(scorers::MostLiquidScorer::new());
        algo.initialize(market.clone()).await;

        // A->C works initially
        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);
        assert!(algo
            .find_best_route(market.clone(), &order)
            .await
            .is_ok());

        // Remove B-C pool
        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::new(),
            removed_components: vec!["pool_bc".to_string()],
            updated_components: vec![],
        };

        algo.handle_events(&[event], market.clone())
            .await
            .unwrap();

        // Now A->C should fail
        assert!(algo
            .find_best_route(market, &order)
            .await
            .is_err());
    }
}
