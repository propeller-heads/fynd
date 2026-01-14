//! Solve statistics tracking for route-finding algorithms.
//!
//! This module provides the `SolveStats` struct which tracks statistics during
//! route finding and provides consistent logging across all algorithm implementations.

use std::{collections::HashSet, time::Instant};

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::info;

use crate::{
    feed::market_data::SharedMarketData, graph::petgraph::StableDiGraph, types::ComponentId, Route,
};

/// Tracks statistics during a solve operation.
///
/// Use this struct to track components visited, protocol systems seen, routes checked,
/// and timing information. Call `log_result` at the end to log the stats.
///
/// # Example
/// ```ignore
/// let mut stats = SolveStats::new(block_number, total_paths);
/// for path in paths {
///     // ... simulate path ...
///     stats.record_path(&path, graph, market);
/// }
/// stats.log_result("algorithm_name", best.as_ref(), market, &amount_in);
/// ```
pub struct SolveStats {
    start_time: Instant,
    total_paths_found: usize,
    routes_checked: usize,
    components_seen: HashSet<ComponentId>,
    protocol_systems_seen: HashSet<String>,
    block_number: u64,
}

impl SolveStats {
    /// Creates a new SolveStats tracker.
    pub fn new(block_number: u64, total_paths_found: usize) -> Self {
        Self {
            start_time: Instant::now(),
            total_paths_found,
            routes_checked: 0,
            components_seen: HashSet::new(),
            protocol_systems_seen: HashSet::new(),
            block_number,
        }
    }

    /// Records a path being checked, tracking its components and protocol systems.
    ///
    /// Also increments the routes_checked counter.
    pub fn record_path(
        &mut self,
        path: &[petgraph::stable_graph::EdgeIndex],
        graph: &StableDiGraph,
        market: &SharedMarketData,
    ) {
        for edge in path {
            let component_id = graph[*edge].component_id.clone();

            if let Some(component) = market.get_component(&component_id) {
                self.protocol_systems_seen.insert(
                    component
                        .component
                        .protocol_system
                        .clone(),
                );
            }
            self.components_seen
                .insert(component_id);
        }
        self.routes_checked += 1;
    }

    /// Returns the elapsed time since tracking started.
    pub fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// Returns the number of routes checked so far.
    pub fn routes_checked(&self) -> usize {
        self.routes_checked
    }

    /// Returns the number of unique components seen.
    pub fn components_seen(&self) -> usize {
        self.components_seen.len()
    }

    /// Returns the number of unique protocol systems seen.
    pub fn protocol_systems_seen(&self) -> usize {
        self.protocol_systems_seen.len()
    }

    /// Logs the solve result with optional best route information.
    pub fn log_result(
        &self,
        algorithm_name: &str,
        best: Option<&(Route, BigUint)>,
        market: &SharedMarketData,
        amount_in: &BigUint,
    ) {
        let solve_time_ms = self.elapsed_ms();

        if let Some((route, amount_out)) = best {
            let symbols = Self::build_path_symbols(route, market);
            let path_description = symbols.join(" -> ");
            let token_in_symbol = symbols
                .first()
                .cloned()
                .unwrap_or_default();
            let token_out_symbol = symbols
                .last()
                .cloned()
                .unwrap_or_default();

            let price = Self::calculate_price(amount_in, amount_out);

            let protocols: HashSet<String> = route
                .swaps
                .iter()
                .map(|swap| swap.protocol.to_string())
                .collect();

            info!(
                algorithm = algorithm_name,
                solve_time_ms,
                total_paths_found = self.total_paths_found,
                routes_checked = self.routes_checked,
                components_checked = self.components_seen.len(),
                protocol_systems_checked = self.protocol_systems_seen.len(),
                block_number = self.block_number,
                path = %path_description,
                amount_in = %amount_in,
                amount_out = %amount_out,
                price_out_per_in = price.unwrap_or(f64::NAN),
                token_in = %token_in_symbol,
                token_out = %token_out_symbol,
                hop_count = route.swaps.len(),
                protocols = ?protocols,
                "best route found"
            );
        } else {
            info!(
                algorithm = algorithm_name,
                solve_time_ms,
                total_paths_found = self.total_paths_found,
                routes_checked = self.routes_checked,
                components_checked = self.components_seen.len(),
                protocol_systems_checked = self.protocol_systems_seen.len(),
                block_number = self.block_number,
                "no valid route found"
            );
        }
    }

    /// Builds token symbol path from route swaps.
    fn build_path_symbols(route: &Route, market: &SharedMarketData) -> Vec<String> {
        let mut symbols = Vec::with_capacity(route.swaps.len() + 1);

        for (i, swap) in route.swaps.iter().enumerate() {
            if i == 0 {
                let symbol = market
                    .get_token(&swap.token_in)
                    .map(|t| t.symbol.clone())
                    .unwrap_or_else(|| format!("{:?}", swap.token_in));
                symbols.push(symbol);
            }
            let symbol = market
                .get_token(&swap.token_out)
                .map(|t| t.symbol.clone())
                .unwrap_or_else(|| format!("{:?}", swap.token_out));
            symbols.push(symbol);
        }

        symbols
    }

    /// Calculates price (amount_out / amount_in) if both values are valid.
    /// Returns None if conversion fails or would result in division by zero.
    fn calculate_price(amount_in: &BigUint, amount_out: &BigUint) -> Option<f64> {
        let amount_in_f64 = amount_in.to_f64()?;
        let amount_out_f64 = amount_out.to_f64()?;

        if amount_in_f64 > 0.0 {
            Some(amount_out_f64 / amount_in_f64)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_simulation::tycho_core::models::Address;

    use super::*;
    use crate::graph::{petgraph::PetgraphStableDiGraphManager, GraphManager};

    #[test]
    fn calculate_price_valid() {
        let price = SolveStats::calculate_price(&BigUint::from(100u64), &BigUint::from(200u64));
        assert_eq!(price, Some(2.0));
    }

    #[test]
    fn calculate_price_zero_input_returns_none() {
        let price = SolveStats::calculate_price(&BigUint::ZERO, &BigUint::from(100u64));
        assert_eq!(price, None);
    }

    #[test]
    fn calculate_price_zero_output() {
        let price = SolveStats::calculate_price(&BigUint::from(100u64), &BigUint::ZERO);
        assert_eq!(price, Some(0.0));
    }

    fn find_edge(
        graph: &StableDiGraph,
        component_id: &str,
        from: &Address,
        to: &Address,
    ) -> petgraph::stable_graph::EdgeIndex {
        graph
            .edge_indices()
            .find(|&e| {
                let (src, dst) = graph.edge_endpoints(e).unwrap();
                graph[e].component_id == component_id && &graph[src] == from && &graph[dst] == to
            })
            .expect("edge not found")
    }

    #[test]
    fn record_path_tracks_and_deduplicates() {
        // Setup: Graph with 3 components (pools)
        // - pool1: A <-> B
        // - pool2: A <-> B (parallel pool)
        // - pool3: B <-> C
        let mut manager = PetgraphStableDiGraphManager::default();

        let token_a = Address::default();
        let token_b = Address::from([1u8; 20]);
        let token_c = Address::from([2u8; 20]);

        let components = HashMap::from([
            ("pool1".to_string(), vec![token_a.clone(), token_b.clone()]),
            ("pool2".to_string(), vec![token_a.clone(), token_b.clone()]),
            ("pool3".to_string(), vec![token_b.clone(), token_c.clone()]),
        ]);
        manager.initialize_graph(&components);

        let graph = manager.graph();
        let market = SharedMarketData::default();
        let mut stats = SolveStats::new(123, 5);

        // Path 1: A -> B via pool1, B -> C via pool3
        let path1 = vec![
            find_edge(graph, "pool1", &token_a, &token_b),
            find_edge(graph, "pool3", &token_b, &token_c),
        ];
        stats.record_path(&path1, graph, &market);

        assert_eq!(stats.routes_checked(), 1);
        assert_eq!(stats.components_seen(), 2); // pool1, pool3

        // Path 2: A -> B via pool2, B -> C via pool3 (pool3 is duplicate)
        let path2 = vec![
            find_edge(graph, "pool2", &token_a, &token_b),
            find_edge(graph, "pool3", &token_b, &token_c),
        ];
        stats.record_path(&path2, graph, &market);

        assert_eq!(stats.routes_checked(), 2);
        assert_eq!(stats.components_seen(), 3); // pool1, pool2, pool3 (pool3 deduplicated)

        // TODO: test protocol_systems deduplication once we can add mock ComponentData to
        // SharedMarketData assert_eq!(stats.protocol_systems_seen(), 2); // e.g.,
        // "uniswap_v2", "uniswap_v3"
        assert_eq!(stats.protocol_systems_seen(), 0);
    }
}
