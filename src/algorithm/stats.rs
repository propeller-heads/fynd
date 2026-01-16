//! Solve statistics tracking for route-finding algorithms.
//!
//! This module provides the `SolveStats` struct which tracks statistics during
//! route finding and provides consistent logging across all algorithm implementations.

use std::{collections::HashSet, time::Instant};

use num_bigint::{BigInt, BigUint};
use num_traits::ToPrimitive;
use tracing::info;

use crate::{feed::market_data::SharedMarketData, types::ComponentId, Path, Route};

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
    pub fn record_path<D>(&mut self, path: &Path<D>, market: &SharedMarketData) {
        for edge in path.edge_iter() {
            if let Some(component) = market.get_component(&edge.component_id) {
                self.protocol_systems_seen.insert(
                    component
                        .component
                        .protocol_system
                        .clone(),
                );
            }
            self.components_seen
                .insert(edge.component_id.clone());
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
        best: Option<&Route>,
        market: &SharedMarketData,
        amount_in: &BigUint,
    ) {
        let solve_time_ms = self.elapsed_ms();

        if let Some(route) = best {
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

            let price = Self::calculate_price(amount_in, &route.net_amount_out);

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
                net_amount_out = %route.net_amount_out,
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
    fn calculate_price(amount_in: &BigUint, amount_out: &BigInt) -> Option<f64> {
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

    use num_bigint::BigInt;
    use petgraph::visit::EdgeRef;
    use tycho_simulation::tycho_core::models::Address;

    use super::*;
    use crate::{
        algorithm::{
            most_liquid::DepthAndPrice,
            test_utils::{setup_market, token},
        },
        graph::{petgraph::PetgraphStableDiGraphManager, GraphManager, Path},
    };

    /// Helper to add a hop to a path by finding the edge with the given component_id.
    ///
    /// Panics if no matching edge is found.
    fn add_hop<'a, D>(
        path: &mut Path<'a, D>,
        graph: &'a crate::graph::petgraph::StableDiGraph<D>,
        from: &'a Address,
        to: &'a Address,
        component_id: &str,
    ) {
        let from_idx = graph
            .node_indices()
            .find(|&n| &graph[n] == from)
            .expect("from node not found");

        for edge in graph.edges(from_idx) {
            if graph[edge.id()].component_id == component_id && &graph[edge.target()] == to {
                path.add_hop(from, edge.weight(), to);
                return;
            }
        }
        panic!("edge not found: {from} -> {to} via {component_id}",);
    }

    // ==================== calculate_price Tests ====================

    #[test]
    fn calculate_price_valid() {
        let price = SolveStats::calculate_price(&BigUint::from(100u64), &BigInt::from(200));
        assert_eq!(price, Some(2.0));
    }

    #[test]
    fn calculate_price_zero_input_returns_none() {
        let price = SolveStats::calculate_price(&BigUint::ZERO, &BigInt::from(100));
        assert_eq!(price, None);
    }

    #[test]
    fn calculate_price_zero_output() {
        let price = SolveStats::calculate_price(&BigUint::from(100u64), &BigInt::ZERO);
        assert_eq!(price, Some(0.0));
    }

    #[test]
    fn calculate_price_negative_output() {
        // Negative net_amount_out when gas exceeds output
        let price = SolveStats::calculate_price(&BigUint::from(100u64), &BigInt::from(-50));
        assert_eq!(price, Some(-0.5));
    }

    // ==================== record_path Tests ====================

    #[test]
    fn record_path_tracks_and_deduplicates() {
        // Setup: Graph with 3 components (pools)
        // - pool1: A <-> B
        // - pool2: A <-> B (parallel pool)
        // - pool3: B <-> C
        let mut manager = PetgraphStableDiGraphManager::<DepthAndPrice>::default();

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

        // Build Path 1: A -> B via pool1, B -> C via pool3
        let mut path1 = Path::new();
        add_hop(&mut path1, graph, &token_a, &token_b, "pool1");
        add_hop(&mut path1, graph, &token_b, &token_c, "pool3");
        stats.record_path(&path1, &market);

        assert_eq!(stats.routes_checked(), 1);
        assert_eq!(stats.components_seen(), 2); // pool1, pool3

        // Build Path 2: A -> B via pool2, B -> C via pool3 (pool3 is duplicate)
        let mut path2 = Path::new();
        add_hop(&mut path2, graph, &token_a, &token_b, "pool2");
        add_hop(&mut path2, graph, &token_b, &token_c, "pool3");
        stats.record_path(&path2, &market);

        assert_eq!(stats.routes_checked(), 2);
        assert_eq!(stats.components_seen(), 3); // pool1, pool2, pool3 (pool3 deduplicated)

        // Protocol systems not tracked without market data
        assert_eq!(stats.protocol_systems_seen(), 0);
    }

    // ==================== build_path_symbols Tests ====================

    /// Creates a swap for testing with specific component_id and token addresses.
    fn swap(component_id: &str, token_in: &Address, token_out: &Address) -> crate::Swap {
        crate::Swap {
            component_id: component_id.to_string(),
            protocol: crate::ProtocolSystem::UniswapV2,
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            gas_estimate: BigUint::from(50000u64),
        }
    }

    #[test]
    fn build_path_symbols_single_hop() {
        use crate::algorithm::test_utils::MockProtocolSim;

        let token_a = token(0x01, "WETH");
        let token_b = token(0x02, "USDC");

        let (market, _) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2))]);

        // Create a route with one swap using actual token addresses
        let route =
            Route::new(vec![swap("pool1", &token_a.address, &token_b.address)], BigInt::from(200));

        let symbols = SolveStats::build_path_symbols(&route, &market);
        assert_eq!(symbols, vec!["WETH", "USDC"]);
    }

    #[test]
    fn build_path_symbols_multi_hop() {
        use crate::algorithm::test_utils::MockProtocolSim;

        let token_a = token(0x01, "WETH");
        let token_b = token(0x02, "USDC");
        let token_c = token(0x03, "DAI");

        let (market, _) = setup_market(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool2", &token_b, &token_c, MockProtocolSim::new(2)),
        ]);

        // Create a route with two swaps using actual token addresses
        let route = Route::new(
            vec![
                swap("pool1", &token_a.address, &token_b.address),
                swap("pool2", &token_b.address, &token_c.address),
            ],
            BigInt::from(200),
        );

        let symbols = SolveStats::build_path_symbols(&route, &market);
        assert_eq!(symbols, vec!["WETH", "USDC", "DAI"]);
    }

    #[test]
    fn build_path_symbols_empty_route() {
        let market = SharedMarketData::new();
        let route = Route::new(vec![], BigInt::from(0));

        let symbols = SolveStats::build_path_symbols(&route, &market);
        assert!(symbols.is_empty());
    }

    #[test]
    fn build_path_symbols_mixed_known_unknown_tokens() {
        use crate::algorithm::test_utils::MockProtocolSim;

        // token_a is known, token_unknown is not in market
        let token_a = token(0x01, "WETH");
        let token_b = token(0x02, "USDC");
        let token_unknown = token(0x99, "UNKNOWN");

        let (market, _) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2))]);

        // Route goes A -> unknown (unknown token not in market)
        let route = Route::new(
            vec![swap("pool1", &token_a.address, &token_unknown.address)],
            BigInt::from(200),
        );

        let symbols = SolveStats::build_path_symbols(&route, &market);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0], "WETH"); // Known token
        assert_eq!(symbols[1], format!("{:?}", token_unknown.address)); // Unknown token shows
                                                                        // address
    }

    #[test]
    fn build_path_symbols_cyclic_path() {
        use crate::algorithm::test_utils::MockProtocolSim;

        // Cyclic route: WETH -> USDC -> WETH (arbitrage-like path)
        let token_a = token(0x01, "WETH");
        let token_b = token(0x02, "USDC");

        let (market, _) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2))]);

        // Create a cyclic route A -> B -> A
        let route = Route::new(
            vec![
                swap("pool1", &token_a.address, &token_b.address),
                swap("pool1", &token_b.address, &token_a.address),
            ],
            BigInt::from(200),
        );

        let symbols = SolveStats::build_path_symbols(&route, &market);
        // Should show: WETH -> USDC -> WETH
        assert_eq!(symbols, vec!["WETH", "USDC", "WETH"]);
    }
}
