//! Most Liquid algorithm implementation.
//!
//! This algorithm finds routes by:
//! 1. Finding all paths up to max_hops using BFS (via petgraph)
//! 2. Simulating each path to get expected output
//! 3. Ranking by net output (output - gas cost in output token terms)
//! 4. Returning the best route

use std::time::{Duration, Instant};

use num_bigint::BigUint;
use petgraph::graph::UnGraph;
use tycho_simulation::tycho_core::models::Address;

use super::{Algorithm, AlgorithmError};
use crate::{
    feed::market_data::SharedMarketData,
    graph::Path,
    types::{Order, Route, Swap},
    ComponentId,
};

/// Algorithm that selects routes based on expected output after gas.
pub struct MostLiquidAlgorithm {
    max_hops: usize,
    timeout: Duration,
}

impl MostLiquidAlgorithm {
    /// Creates a new MostLiquidAlgorithm with default settings.
    pub fn new() -> Self {
        Self { max_hops: 3, timeout: Duration::from_millis(50) }
    }

    /// Creates a new MostLiquidAlgorithm with custom settings.
    pub fn with_config(max_hops: usize, timeout_ms: u64) -> Self {
        Self { max_hops, timeout: Duration::from_millis(timeout_ms) }
    }

    /// Simulates a path and returns the expected output amount.
    ///
    /// TODO: Implement actual simulation using ProtocolSim
    fn simulate_path(
        &self,
        _path: &Path,
        _market: &SharedMarketData,
        amount_in: BigUint,
    ) -> Result<SimulationResult, AlgorithmError> {
        // TODO: Implement actual simulation
        // For now, return a placeholder that assumes 0.3% fee per hop
        let hops = _path.hop_count() as u32;
        let fee_multiplier = BigUint::from(997u32).pow(hops);
        let divisor = BigUint::from(1000u32).pow(hops);
        let amount_out = &amount_in * &fee_multiplier / &divisor;

        // TODO: Estimate gas based on protocols in path
        // let gas_estimate: u64 = _path
        //     .hops
        //     .iter()
        //     .map(|e| e.protocol_system.typical_gas_cost())
        //     .sum();

        Ok(SimulationResult { amount_out, gas_estimate: BigUint::ZERO })
    }

    /// Converts a Path to a Route with simulated amounts.
    fn path_to_route(
        &self,
        path: &Path,
        market: &SharedMarketData,
        amount_in: BigUint,
    ) -> Result<Route, AlgorithmError> {
        // Simulate to get amounts
        let sim_result = self.simulate_path(path, market, amount_in.clone())?;

        // Build swaps
        // TODO: Calculate intermediate amounts properly
        let mut swaps = Vec::with_capacity(path.hops.len());
        let mut current_amount = amount_in;

        for (i, edge) in path.hops.iter().enumerate() {
            let token_in = path.tokens[i].clone();
            let token_out = edge.token_out.clone();

            // Placeholder: distribute output evenly for now
            let amount_out = if i == path.hops.len() - 1 {
                sim_result.amount_out.clone()
            } else {
                // Estimate intermediate amount
                &current_amount * BigUint::from(997u32) / BigUint::from(1000u32)
            };

            let protocol_system = market
                .get_component(&edge.component_id)
                .unwrap()
                .protocol_system;

            swaps.push(Swap {
                component_id: edge.component_id.clone(),
                protocol: protocol_system,
                token_in,
                token_out,
                amount_in: current_amount.clone(),
                amount_out: amount_out.clone(),
                gas_estimate: BigUint::from(protocol_system.typical_gas_cost()),
            });

            current_amount = amount_out;
        }

        Ok(Route::new(swaps))
    }
}

impl Default for MostLiquidAlgorithm {
    fn default() -> Self {
        Self::new()
    }
}

impl Algorithm for MostLiquidAlgorithm {
    type GraphType = UnGraph<Address, ComponentId>;
    type GraphManager = crate::graph::PetgraphUnGraphManager;
    fn name(&self) -> &str {
        "most_liquid"
    }

    fn find_best_route(
        &self,
        graph: &UnGraph<Address, ComponentId>,
        market: &SharedMarketData,
        order: &Order,
    ) -> Result<Route, AlgorithmError> {
        let start_time = Instant::now();

        // Check for exact-out (not supported yet)
        if order.is_exact_out() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let amount_in = order
            .amount_in
            .as_ref()
            .ok_or_else(|| AlgorithmError::Other("missing amount_in".to_string()))?
            .clone();

        // Find all paths using BFS
        let paths = self.find_paths(graph, &order.token_in, &order.token_out);

        if paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: format!("{:?}", order.token_in),
                to: format!("{:?}", order.token_out),
            });
        }

        // Get gas price for ranking
        let gas_price = market.gas_price().effective_gas_price();

        // Simulate and rank paths
        let mut best_route: Option<(Route, BigUint)> = None;

        for path in &paths {
            // Check timeout
            if start_time.elapsed() > self.timeout {
                // Return best found so far, or timeout error
                return best_route
                    .map(|(route, _)| route)
                    .ok_or(AlgorithmError::Timeout {
                        elapsed_ms: start_time.elapsed().as_millis() as u64,
                    });
            }

            // Simulate path
            let sim_result = match self.simulate_path(path, market, amount_in.clone()) {
                Ok(r) => r,
                Err(_) => continue, // Skip paths that fail simulation
            };

            // Calculate net output (output - gas cost)
            // TODO: Convert gas cost to output token terms using price oracle
            let gas_cost_wei = &sim_result.gas_estimate * &gas_price;
            let net_output = if sim_result.amount_out > gas_cost_wei {
                &sim_result.amount_out - &gas_cost_wei
            } else {
                BigUint::ZERO
            };

            // Update best if this is better
            let is_better = best_route
                .as_ref()
                .map(|(_, best_net)| net_output > *best_net)
                .unwrap_or(true);

            if is_better {
                if let Ok(route) = self.path_to_route(path, market, amount_in.clone()) {
                    best_route = Some((route, net_output));
                }
            }
        }

        best_route
            .map(|(route, _)| route)
            .ok_or(AlgorithmError::InsufficientLiquidity)
    }

    fn supports_exact_out(&self) -> bool {
        false // TODO: Implement exact-out support
    }

    fn max_hops(&self) -> usize {
        self.max_hops
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl MostLiquidAlgorithm {
    /// Finds all paths between two tokens using BFS on a petgraph.
    fn find_paths(
        &self,
        graph: &UnGraph<Address, ComponentId>,
        from: &Address,
        to: &Address,
    ) -> Vec<Path> {
        // Find node indices for from and to tokens
        let from_idx = graph
            .node_indices()
            .find(|&idx| &graph[idx] == from);
        let to_idx = graph
            .node_indices()
            .find(|&idx| &graph[idx] == to);

        let (Some(from_idx), Some(to_idx)) = (from_idx, to_idx) else {
            return vec![];
        };

        if from_idx == to_idx {
            return vec![];
        }

        vec![]

        // TODO: Use petgraph's all_simple_paths to find all paths

        // let mut paths = Vec::new();
        // let mut queue: VecDeque<(petgraph::graph::NodeIndex, Vec<ComponentId>, Vec<Address>)> =
        //     VecDeque::new();

        // // Start BFS from the source token
        // queue.push_back((from_idx, vec![], vec![from.clone()]));

        // while let Some((current_idx, edges, tokens)) = queue.pop_front() {
        //     // Check hop limit
        //     if edges.len() >= self.max_hops {
        //         continue;
        //     }

        //     // Explore neighbors
        //     for neighbor_idx in graph.neighbors(current_idx) {
        //         let neighbor_token = graph[neighbor_idx].clone();

        //         // Avoid cycles (don't revisit tokens)
        //         if tokens.contains(&neighbor_token) {
        //             continue;
        //         }

        //         // Get the edge data
        //         let edge = graph
        //             .edges_connecting(current_idx, neighbor_idx)
        //             .next()
        //             .map(|e| e.weight().clone());

        //         let Some(edge) = edge else {
        //             continue;
        //         };

        //         let mut new_edges = edges.clone();
        //         new_edges.push(edge.clone());

        //         let mut new_tokens = tokens.clone();
        //         new_tokens.push(neighbor_token.clone());

        //         // Found a path to destination
        //         if neighbor_token == to.clone() {
        //             paths.push(Path { hops: new_edges, tokens: new_tokens });
        //         } else {
        //             // Continue searching
        //             queue.push_back((neighbor_idx, new_edges, new_tokens));
        //         }
        //     }
        // }

        // paths
    }
}

/// Result of simulating a path.
struct SimulationResult {
    amount_out: BigUint,
    gas_estimate: BigUint,
}
