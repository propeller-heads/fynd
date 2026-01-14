//! Most Liquid algorithm implementation.
//!
//! This algorithm finds routes by:
//! 1. Finding all edge paths up to max_hops using BFS (shorter paths first, all parallel edges)
//! 2. Scoring and sorting paths by spot price, fees, and liquidity depth
//! 3. Simulating paths with actual ProtocolSim to get accurate output (best paths first)
//! 4. Ranking by net output (output - gas cost in output token terms)
//! 5. Returning the best route with comprehensive stats reporting

use std::time::Duration;

use num_bigint::BigUint;
use num_traits::CheckedSub;

use super::{simulate_path, stats::SolveStats, Algorithm, AlgorithmError};
use crate::{
    feed::market_data::SharedMarketData,
    graph::{petgraph::StableDiGraph, GraphManager, Path, PetgraphStableDiGraphManager},
    types::{Order, Route},
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

    /// Scores a path based on spot prices, fees, and minimum liquidity depth (inertia).
    ///
    /// Formula: `weight = (product of all [spot_price × (1 - fee)]) × min(depths)`
    ///
    /// This accounts:
    /// - Spot price: the theoretical exchange rate along the path not accounting for slippage
    /// - Fees: deducted per hop as `price *= (1 - fee)`
    /// - Depth (inertia): minimum depth acts as a liquidity bottleneck indicator
    ///
    /// Returns `None` if edge weights are missing required data (spot_price, depth).
    /// Higher score = better path candidate. Paths through deeper, lower-fee pools rank higher.
    fn score_path(path: &Path, graph: &StableDiGraph) -> Option<f64> {
        let mut price = 1.0;
        // If path is empty, return max score
        let mut min_depth = f64::MAX;

        for edge in path {
            let weight = graph[*edge].weight.as_ref()?;
            let spot_price = weight.spot_price?;
            let depth = weight.depth?;

            price *= spot_price;
            price *= 1.0 - weight.fee.unwrap_or(0.0);
            min_depth = min_depth.min(depth);
        }

        Some(price * min_depth)
    }
}

impl Default for MostLiquidAlgorithm {
    fn default() -> Self {
        Self::new()
    }
}

impl Algorithm for MostLiquidAlgorithm {
    type GraphType = StableDiGraph;
    type GraphManager = PetgraphStableDiGraphManager;

    fn name(&self) -> &str {
        "most_liquid"
    }

    fn find_best_route(
        &self,
        graph_manager: &Self::GraphManager,
        market: &SharedMarketData,
        order: &Order,
    ) -> Result<Route, AlgorithmError> {
        // Exact-out not supported yet
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let amount_in = order.amount.clone();
        let graph = graph_manager.graph();

        // Step 1: Find all edge paths using BFS (shorter paths first)
        let all_paths = graph_manager
            .find_paths(&order.token_in, &order.token_out, 0, self.max_hops)
            .map_err(|e| AlgorithmError::Other(format!("Graph error: {}", e)))?;

        if all_paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: format!("{:?}", order.token_in),
                to: format!("{:?}", order.token_out),
            });
        }

        // Initialize stats tracking
        let mut stats = SolveStats::new(market.last_updated().number, all_paths.len());

        // Step 2: Score and sort all paths by estimated output (higher score = better)
        let mut scored_paths: Vec<(Path, f64)> = all_paths
            .into_iter()
            .filter_map(|path| {
                let score = Self::score_path(&path, graph)?;
                Some((path, score))
            })
            .collect();

        scored_paths.sort_by(|(_, a_score), (_, b_score)| {
            // Invert the comparison to get descending order
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if scored_paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: format!("{:?}", order.token_in),
                to: format!("{:?}", order.token_out),
            });
        }

        // Step 3: Simulate all paths in score order
        let mut best: Option<(Route, BigUint)> = None;

        for (edge_path, _) in scored_paths {
            // Check timeout
            if stats.elapsed_ms() > self.timeout.as_millis() as u64 {
                break;
            }

            // Track pools and protocols for this path
            stats.record_path(&edge_path, graph, market);

            let route = match simulate_path(edge_path, graph, market, amount_in.clone()) {
                Ok(r) => r,
                Err(_) => continue, // Skip paths that fail simulation
            };

            let output_amount = if let Some(swap) = route.swaps.last() {
                swap.amount_out.clone()
            } else {
                Err(AlgorithmError::Other("route has no swaps".to_string()))?
            };

            // Calculate net output (output - gas cost in wei)
            // TODO: Convert gas cost to output token terms for proper ranking.
            // Currently subtracting raw wei from output amount, which is incorrect when
            // token_out != ETH. Need to:
            // 1. Store ETH price per token (token/ETH rate) - likely in SharedMarketData
            // 2. Look up ETH price for the output token of this path
            // 3. Convert: gas_cost_in_token_out = gas_cost_wei * eth_price_in_token_out
            let gas_price = market.gas_price().effective_gas_price();
            let gas_cost_wei = route.total_gas() * gas_price;
            let gas_cost_out = gas_cost_wei * 1u32; // Placeholder until conversion is implemented
            let net_output = output_amount
                .checked_sub(&gas_cost_out)
                .ok_or(AlgorithmError::GasExceedsOutput)?;

            // Check if this is the best result so far
            let is_better = best
                .as_ref()
                .map(|(_, best_net)| net_output > *best_net)
                .unwrap_or(true);

            if is_better {
                best = Some((route, net_output));
            }
        }

        // Log solve result
        stats.log_result(self.name(), best.as_ref(), market, &amount_in);

        let elapsed = stats.elapsed_ms();

        best.map(|(route, _)| route).ok_or({
            if elapsed > self.timeout.as_millis() as u64 {
                AlgorithmError::Timeout { elapsed_ms: elapsed }
            } else {
                AlgorithmError::InsufficientLiquidity
            }
        })
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_simulation::tycho_core::models::Address;

    use super::*;
    use crate::graph::petgraph::EdgeWeight;

    /// Creates a single-edge graph (A -> B) with optional edge weight.
    fn single_edge_setup(weight: Option<EdgeWeight>) -> (StableDiGraph, Path) {
        let mut manager = PetgraphStableDiGraphManager::default();
        let token_a = Address::default();
        let token_b = Address::from([1u8; 20]);

        manager.initialize_graph(&HashMap::from([(
            "pool1".to_string(),
            vec![token_a.clone(), token_b.clone()],
        )]));

        if let Some(w) = weight {
            manager
                .set_edge_weight(&"pool1".to_string(), &token_a, &token_b, w, false)
                .unwrap();
        }

        let graph = manager.graph().clone();

        let path = vec![graph
            .edge_indices()
            .find(|&e| {
                let (src, _) = graph.edge_endpoints(e).unwrap();
                graph[src] == token_a
            })
            .unwrap()];

        (graph, path)
    }

    #[test]
    fn score_path_calculates_correctly() {
        let mut manager = PetgraphStableDiGraphManager::default();
        let token_a = Address::default();
        let token_b = Address::from([1u8; 20]);
        let token_c = Address::from([2u8; 20]);

        manager.initialize_graph(&HashMap::from([
            ("pool1".to_string(), vec![token_a.clone(), token_b.clone()]),
            ("pool2".to_string(), vec![token_b.clone(), token_c.clone()]),
        ]));

        // pool1 A->B: spot=2.0, depth=1000, fee=0.3%; pool2 B->C: spot=0.5, depth=500, fee=0.1%
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a,
                &token_b,
                EdgeWeight::new(2.0, 1000.0, 0.003),
                false,
            )
            .unwrap();
        manager
            .set_edge_weight(
                &"pool2".to_string(),
                &token_b,
                &token_c,
                EdgeWeight::new(0.5, 500.0, 0.001),
                false,
            )
            .unwrap();

        let graph = manager.graph();
        let path: Path = graph
            .edge_indices()
            .filter(|&e| {
                let (src, dst) = graph.edge_endpoints(e).unwrap();
                (graph[src] == token_a && graph[dst] == token_b) ||
                    (graph[src] == token_b && graph[dst] == token_c)
            })
            .collect();

        // price = 2.0 * 0.997 * 0.5 * 0.999, min_depth = 500.0
        let expected = 2.0 * 0.997 * 0.5 * 0.999 * 500.0;
        let score = MostLiquidAlgorithm::score_path(&path, graph).unwrap();
        assert!((score - expected).abs() < 0.0001, "expected {expected}, got {score}");
    }

    #[test]
    fn score_path_empty_returns_max() {
        let graph = StableDiGraph::default();
        assert_eq!(MostLiquidAlgorithm::score_path(&vec![], &graph), Some(f64::MAX));
    }

    #[test]
    fn score_path_missing_weight_returns_none() {
        let (graph, path) = single_edge_setup(None);
        assert!(MostLiquidAlgorithm::score_path(&path, &graph).is_none());
    }

    #[test]
    fn score_path_missing_spot_price_returns_none() {
        let (graph, path) = single_edge_setup(Some(EdgeWeight::default().with_depth(1000.0)));
        assert!(MostLiquidAlgorithm::score_path(&path, &graph).is_none());
    }

    #[test]
    fn score_path_missing_depth_returns_none() {
        let (graph, path) = single_edge_setup(Some(EdgeWeight::default().with_spot_price(2.0)));
        assert!(MostLiquidAlgorithm::score_path(&path, &graph).is_none());
    }

    #[test]
    fn score_path_missing_fee_uses_zero() {
        let (graph, path) = single_edge_setup(Some(
            EdgeWeight::default()
                .with_spot_price(2.0)
                .with_depth(1000.0),
        ));
        // price = 2.0 * (1 - 0.0), depth = 1000.0 -> score = 2000.0
        let score = MostLiquidAlgorithm::score_path(&path, &graph).unwrap();
        assert_eq!(score, 2000.0);
    }

    // ==================== find_best_route Tests ====================

    use num_bigint::BigUint;

    use crate::algorithm::test_utils::{
        add_component_to_market, sell_order, setup_market_with_weights, token, ONE_ETH,
    };

    #[test]
    fn find_best_route_single_path() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_with_weights(vec![(
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,      // multiplier (also used as spot_price)
            1000.0, // depth
        )]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = sell_order(&token_a, &token_b, ONE_ETH);
        let route = algorithm
            .find_best_route(&manager, &market, &order)
            .unwrap();

        assert_eq!(route.swaps.len(), 1);
        assert_eq!(route.swaps[0].amount_in, BigUint::from(ONE_ETH));
        assert_eq!(route.swaps[0].amount_out, BigUint::from(ONE_ETH * 2));
    }

    #[test]
    fn find_best_route_selects_higher_output() {
        // Two parallel pools A->B: pool1 (multiplier=2) and pool2 (multiplier=3)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (mut market, mut manager) = setup_market_with_weights(vec![(
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,
            1000.0,
        )]);

        // Add second pool with higher multiplier
        add_component_to_market(&mut market, "pool2", vec![token_a.clone(), token_b.clone()], 3);
        manager.initialize_graph(&market.component_topology());

        // Set edge weights for both pools
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a.address,
                &token_b.address,
                EdgeWeight::new(2.0, 1000.0, 0.003),
                false,
            )
            .unwrap();
        manager
            .set_edge_weight(
                &"pool2".to_string(),
                &token_a.address,
                &token_b.address,
                EdgeWeight::new(3.0, 1000.0, 0.003),
                false,
            )
            .unwrap();

        let algorithm = MostLiquidAlgorithm::new();
        let order = sell_order(&token_a, &token_b, ONE_ETH);
        let route = algorithm
            .find_best_route(&manager, &market, &order)
            .unwrap();

        // Should select pool2 for higher output (3x vs 2x)
        assert_eq!(route.swaps.len(), 1);
        assert_eq!(route.swaps[0].amount_out, BigUint::from(ONE_ETH * 3));
        assert_eq!(route.swaps[0].component_id, "pool2");
    }

    #[test]
    fn find_best_route_no_path_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C"); // Disconnected

        let (market, manager) = setup_market_with_weights(vec![(
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,
            1000.0,
        )]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = sell_order(&token_a, &token_c, ONE_ETH);

        let result = algorithm.find_best_route(&manager, &market, &order);
        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[test]
    fn find_best_route_multi_hop() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_with_weights(vec![
            ("pool1", vec![token_a.clone(), token_b.clone()], 2, 1000.0),
            ("pool2", vec![token_b.clone(), token_c.clone()], 3, 1000.0),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(2, 100);
        let order = sell_order(&token_a, &token_c, ONE_ETH);
        let route = algorithm
            .find_best_route(&manager, &market, &order)
            .unwrap();

        // A->B: ONE_ETH*2, B->C: (ONE_ETH*2)*3
        assert_eq!(route.swaps.len(), 2);
        assert_eq!(route.swaps[0].amount_out, BigUint::from(ONE_ETH * 2));
        assert_eq!(route.swaps[1].amount_out, BigUint::from(ONE_ETH * 2 * 3));
    }

    #[test]
    fn find_best_route_skips_paths_without_edge_weights() {
        // Pool1 has edge weights (scoreable), Pool2 doesn't (filtered out)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (mut market, mut manager) = setup_market_with_weights(vec![(
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,
            1000.0,
        )]);

        // Add pool2 with higher multiplier but NO edge weight
        add_component_to_market(&mut market, "pool2", vec![token_a.clone(), token_b.clone()], 10);
        manager.initialize_graph(&market.component_topology());

        // Re-set pool1 weight after reinitializing graph
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a.address,
                &token_b.address,
                EdgeWeight::new(2.0, 1000.0, 0.003),
                false,
            )
            .unwrap();
        // pool2 intentionally has no edge weight

        let algorithm = MostLiquidAlgorithm::new();
        let order = sell_order(&token_a, &token_b, ONE_ETH);
        let route = algorithm
            .find_best_route(&manager, &market, &order)
            .unwrap();

        // Should use pool1 (only scoreable path), despite pool2 having better multiplier
        assert_eq!(route.swaps[0].component_id, "pool1");
        assert_eq!(route.swaps[0].amount_out, BigUint::from(ONE_ETH * 2));
    }

    // ==================== Error Handling Tests ====================

    #[test]
    fn find_best_route_exact_out_not_supported() {
        use crate::algorithm::test_utils::buy_order;

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_with_weights(vec![(
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,
            1000.0,
        )]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = buy_order(&token_a, &token_b, ONE_ETH);

        let result = algorithm.find_best_route(&manager, &market, &order);
        assert!(matches!(result, Err(AlgorithmError::ExactOutNotSupported)));
    }

    #[test]
    fn find_best_route_gas_exceeds_output() {
        use crate::types::GasPrice;

        // Use a tiny amount (1 wei) so output (2 wei) is less than gas cost
        // MockProtocolSim uses gas=100_000, so gas_cost = 100_000 * gas_price
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (mut market, manager) = setup_market_with_weights(vec![(
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,
            1000.0,
        )]);

        // Set a non-zero gas price so gas cost exceeds tiny output
        // gas_cost = 100_000 * (1_000_000 + 1_000_000) = 200_000_000_000 >> 2 wei output
        market.update_gas_price(GasPrice::new(
            BigUint::from(1_000_000u64),
            BigUint::from(1_000_000u64),
        ));

        let algorithm = MostLiquidAlgorithm::new();
        let order = sell_order(&token_a, &token_b, 1); // 1 wei input -> 2 wei output

        let result = algorithm.find_best_route(&manager, &market, &order);
        assert!(matches!(result, Err(AlgorithmError::GasExceedsOutput)));
    }

    #[test]
    fn find_best_route_insufficient_liquidity() {
        use crate::algorithm::test_utils::add_component_with_liquidity;

        // Pool has limited liquidity (1000 wei) but we try to swap ONE_ETH
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let mut market = SharedMarketData::new();
        add_component_with_liquidity(
            &mut market,
            "pool1",
            vec![token_a.clone(), token_b.clone()],
            2,
            1000, // Only 1000 wei liquidity
        );

        let mut manager = PetgraphStableDiGraphManager::default();
        manager.initialize_graph(&market.component_topology());

        // Set edge weights so path is scoreable
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a.address,
                &token_b.address,
                EdgeWeight::new(2.0, 1000.0, 0.003),
                false,
            )
            .unwrap();

        let algorithm = MostLiquidAlgorithm::new();
        let order = sell_order(&token_a, &token_b, ONE_ETH); // Way more than 1000 wei liquidity

        let result = algorithm.find_best_route(&manager, &market, &order);
        assert!(matches!(result, Err(AlgorithmError::InsufficientLiquidity)));
    }
}
