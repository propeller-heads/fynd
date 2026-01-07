//! Most Liquid algorithm implementation.
//!
//! This algorithm finds routes by:
//! 1. Finding all paths up to max_hops using BFS
//! 2. Simulating each path to get expected output
//! 3. Ranking by net output (output - gas cost in output token terms)
//! 4. Returning the best route

use std::time::{Duration, Instant};

use alloy::primitives::U256;

use crate::market_data::SharedMarketData;
use crate::route_graph::{Path, RouteGraph};
use crate::types::{Order, Route, Swap};

use super::{Algorithm, AlgorithmError};

/// Algorithm that selects routes based on expected output after gas.
pub struct MostLiquidAlgorithm {
    max_hops: usize,
    timeout: Duration,
}

impl MostLiquidAlgorithm {
    /// Creates a new MostLiquidAlgorithm with default settings.
    pub fn new() -> Self {
        Self {
            max_hops: 3,
            timeout: Duration::from_millis(50),
        }
    }

    /// Creates a new MostLiquidAlgorithm with custom settings.
    pub fn with_config(max_hops: usize, timeout_ms: u64) -> Self {
        Self {
            max_hops,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Simulates a path and returns the expected output amount.
    ///
    /// TODO: Implement actual simulation using ProtocolSim
    fn simulate_path(
        &self,
        _path: &Path,
        _market: &SharedMarketData,
        amount_in: U256,
    ) -> Result<SimulationResult, AlgorithmError> {
        // TODO: Implement actual simulation
        // For now, return a placeholder that assumes 0.3% fee per hop
        let hops = _path.hop_count() as u32;
        let fee_multiplier = U256::from(997).pow(U256::from(hops));
        let divisor = U256::from(1000).pow(U256::from(hops));
        let amount_out = amount_in * fee_multiplier / divisor;

        // Estimate gas based on protocols in path
        let gas_estimate: u64 = _path
            .edges
            .iter()
            .map(|e| e.protocol_system.typical_gas_cost())
            .sum();

        Ok(SimulationResult {
            amount_out,
            gas_estimate: U256::from(gas_estimate),
        })
    }

    /// Converts a Path to a Route with simulated amounts.
    fn path_to_route(
        &self,
        path: &Path,
        market: &SharedMarketData,
        amount_in: U256,
    ) -> Result<Route, AlgorithmError> {
        // Simulate to get amounts
        let sim_result = self.simulate_path(path, market, amount_in)?;

        // Build swaps
        // TODO: Calculate intermediate amounts properly
        let mut swaps = Vec::with_capacity(path.edges.len());
        let mut current_amount = amount_in;

        for (i, edge) in path.edges.iter().enumerate() {
            let token_in = path.tokens[i];
            let token_out = edge.token_out;

            // Placeholder: distribute output evenly for now
            let amount_out = if i == path.edges.len() - 1 {
                sim_result.amount_out
            } else {
                // Estimate intermediate amount
                current_amount * U256::from(997) / U256::from(1000)
            };

            swaps.push(Swap {
                pool_id: edge.pool_id.clone(),
                protocol: edge.protocol_system,
                token_in,
                token_out,
                amount_in: current_amount,
                amount_out,
                gas_estimate: U256::from(edge.protocol_system.typical_gas_cost()),
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
    fn name(&self) -> &str {
        "most_liquid"
    }

    fn find_best_route(
        &self,
        graph: &RouteGraph,
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
            .ok_or_else(|| AlgorithmError::Other("missing amount_in".to_string()))?;

        // Find all paths
        let paths = graph.find_paths(&order.token_in, &order.token_out, self.max_hops);

        if paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: format!("{:?}", order.token_in),
                to: format!("{:?}", order.token_out),
            });
        }

        // Get gas price for ranking
        let gas_price = market.gas_price().effective_gas_price();

        // Simulate and rank paths
        let mut best_route: Option<(Route, U256)> = None;

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
            let sim_result = match self.simulate_path(path, market, amount_in) {
                Ok(r) => r,
                Err(_) => continue, // Skip paths that fail simulation
            };

            // Calculate net output (output - gas cost)
            // TODO: Convert gas cost to output token terms using price oracle
            let gas_cost_wei = sim_result.gas_estimate * gas_price;
            let net_output = sim_result.amount_out.saturating_sub(gas_cost_wei);

            // Update best if this is better
            let is_better = best_route
                .as_ref()
                .map(|(_, best_net)| net_output > *best_net)
                .unwrap_or(true);

            if is_better {
                if let Ok(route) = self.path_to_route(path, market, amount_in) {
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

/// Result of simulating a path.
struct SimulationResult {
    amount_out: U256,
    gas_estimate: U256,
}
