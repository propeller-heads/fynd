//! Computes the `mid_price` of tokens relative to a gas token (e.g., ETH), selecting paths
//! by the lowest spread (the most reliable price) derived from full simulation of both buy and sell
//! directions.
//!
//! # Algorithm
//!
//! 1. **Path Discovery (DFS)**: Enumerate all paths from gas_token to each reachable token, scoring
//!    by spot-price spread: `|forward_spot - 1/reverse_spot|`. Lower spread = better score.
//!
//! 2. **Sort**: Order paths per token by spread score (lowest spread first).
//!
//! 3. **Round-Robin Simulation**: For each token, simulate paths in ranked order and compute their
//!    spread and mid_price by simulating both directions on the same path. Pick the path with the
//!    tightest spread for each token, as this indicates the most reliable/liquid route, and provide
//!    its mid_price as the token's price.
//!
//! # Price Formulas
//!
//! For a path P from gas_token to target:
//! - `buy_out` = simulate(P, probe_amount) → tokens received
//! - `sell_out` = simulate(reverse(P), buy_out) → gas_token received back
//! - `buy_price` = buy_out / (probe_amount + gas_cost)
//! - `sell_price` = buy_out / (sell_out - gas_cost)
//! - `mid_price` = (buy_price + sell_price) / 2
//! - `spread` = |sell_price - buy_price|
//!
//! # Dependencies
//!
//! This computation depends on [`SpotPrices`](crate::derived::types::SpotPrices) being
//! available in the [`DerivedDataStore`](crate::derived::store::DerivedDataStore).
//! Ensure `SpotPriceComputation` runs before this computation.

use std::collections::HashMap;

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, instrument, trace, warn, Span};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        computations::spot_price::SpotPriceComputation,
        error::ComputationError,
        manager::SharedDerivedDataRef,
        types::{SpotPriceKey, SpotPrices, TokenGasPrices},
    },
    feed::market_data::{SharedMarketData, SharedMarketDataRef},
    graph::{GraphManager, Path, PetgraphStableDiGraphManager},
    MostLiquidAlgorithm,
};

/// A path with its score
#[derive(Clone)]
struct CandidatePath<'a> {
    path: Path<'a, ()>,
    score: f64,
}

/// Computes token prices relative to the gas token. Returns the buy price for the path
/// with the lowest spread (most reliable) that we managed to find.
///
/// Uses DFS to discover paths, spot prices for ranking, and full simulation
/// for accurate output amounts and spread calculation.
#[derive(Debug, Clone)]
pub struct TokenGasPriceComputation {
    /// The gas token address (e.g., ETH).
    gas_token: Address,
    /// Maximum path length to explore.
    max_hops: usize,
    /// Amount of gas token to simulate with (affects slippage).
    simulation_amount: BigUint,
}

impl Default for TokenGasPriceComputation {
    fn default() -> Self {
        Self {
            gas_token: Address::zero(20), // ETH address
            max_hops: 2,
            simulation_amount: BigUint::from(10u64).pow(18), // 1 ETH
        }
    }
}

impl TokenGasPriceComputation {
    pub fn new(gas_token: Address, max_hops: usize, simulation_amount: BigUint) -> Self {
        Self { gas_token, max_hops, simulation_amount }
    }

    /// Sets the maximum number of hops to explore.
    pub fn with_max_hops(self, max_hops: usize) -> Self {
        Self { max_hops, ..self }
    }

    /// Sets the gas token address.
    pub fn with_gas_token(self, gas_token: Address) -> Self {
        Self { gas_token, ..self }
    }

    pub fn simulation_amount(&self) -> &BigUint {
        &self.simulation_amount
    }

    /// DFS to discover all paths from gas_token, scored by spot-price spread.
    fn discover_paths<'a>(
        &self,
        graph_manager: &'a PetgraphStableDiGraphManager<()>,
        spot_prices: &SpotPrices,
    ) -> Result<HashMap<Address, Vec<CandidatePath<'a>>>, ComputationError> {
        let graph = graph_manager.graph();

        // If gas token has no pools, it won't be in the graph → no paths to discover
        let Ok(entry_node) = graph_manager.find_node(&self.gas_token) else {
            return Ok(HashMap::new());
        };

        let mut paths_by_token: HashMap<Address, Vec<CandidatePath>> = HashMap::new();

        // DFS state
        struct DfsFrame<'a> {
            token_node: NodeIndex,
            path: Path<'a, ()>,
            forward_spot: f64,
            reverse_spot: f64,
        }

        let mut stack = vec![DfsFrame {
            token_node: entry_node,
            path: Path::new(),
            forward_spot: 1.0,
            reverse_spot: 1.0,
        }];

        while let Some(frame) = stack.pop() {
            // Token that we reached in this frame
            let token_reached = &graph[frame.token_node];

            // Record non-empty paths (skip the starting node's empty path)
            if !frame.path.is_empty() {
                // Compute spread from spot prices:
                // buy_price = forward_spot (target per gas when buying)
                // sell_price = 1/reverse_spot (target per gas when selling)
                // spread = |buy_price - sell_price|
                // Score = spread directly (lower = better, 0 for symmetric pools)
                let buy_price = frame.forward_spot;
                let sell_price = 1.0 / frame.reverse_spot;
                let spot_spread = (buy_price - sell_price).abs();

                paths_by_token
                    .entry(token_reached.clone())
                    .or_default()
                    .push(CandidatePath { path: frame.path.clone(), score: spot_spread });
            }

            // Stop exploring further if max depth reached
            if frame.path.len() >= self.max_hops {
                continue;
            }

            // Explore neighbors
            for edge in graph.edges(frame.token_node) {
                let next_node = edge.target();
                let next_token = &graph[next_node];

                let mut new_path = frame.path.clone();
                new_path.add_hop(token_reached, edge.weight(), next_token);

                let component_id = edge.weight().component_id.clone();

                // Look up spot prices for this edge
                let fwd_key: SpotPriceKey =
                    (component_id.clone(), token_reached.clone(), next_token.clone());
                let rev_key: SpotPriceKey =
                    (component_id.clone(), next_token.clone(), token_reached.clone());

                let &fwd_spot =
                    spot_prices
                        .get(&fwd_key)
                        .ok_or(ComputationError::InvalidDependencyData {
                            dependency: SpotPriceComputation::ID,
                            reason: format!(
                                "missing forward spot price for pool {} {}/{}",
                                component_id, token_reached, next_token
                            ),
                        })?;
                let &rev_spot =
                    spot_prices
                        .get(&rev_key)
                        .ok_or(ComputationError::InvalidDependencyData {
                            dependency: SpotPriceComputation::ID,
                            reason: format!(
                                "missing reverse spot price for pool {} {}/{}",
                                component_id, next_token, token_reached
                            ),
                        })?;

                stack.push(DfsFrame {
                    token_node: next_node,
                    path: new_path,
                    forward_spot: frame.forward_spot * fwd_spot,
                    reverse_spot: frame.reverse_spot * rev_spot,
                });
            }
        }

        Ok(paths_by_token)
    }

    /// Compute the spread and mid_price for a given path by simulating both directions.
    ///
    /// Returns (spread_ratio, mid_price) where:
    /// - spread_ratio: |sell - buy|, lower = more reliable
    /// - mid_price: precise Price struct
    fn compute_spread_and_mid_price(
        &self,
        path: Path<()>,
        market: &SharedMarketData,
        gas_price: &BigUint,
    ) -> Result<(f64, Price), ComputationError> {
        // Forward: gas_token → target_token
        let buy_route =
            MostLiquidAlgorithm::simulate_path(&path, market, self.simulation_amount.clone())
                .map_err(|e| {
                    ComputationError::SimulationFailed(format!("buy simulation failed: {}", e))
                })?;
        let buy_gas_units = buy_route.total_gas();
        let buy_gas_cost = &buy_gas_units * gas_price; // Convert gas units to actual cost
        let buy_out = buy_route
            .swaps
            .into_iter()
            .last()
            .ok_or(ComputationError::Internal("no output from buy simulation".into()))?
            .amount_out;

        // Reverse: target_token → gas_token
        let reversed_path = path.reversed();

        let sell_route =
            MostLiquidAlgorithm::simulate_path(&reversed_path, market, buy_out.clone()).map_err(
                |e| ComputationError::SimulationFailed(format!("sell simulation failed: {}", e)),
            )?;
        let sell_gas_units = sell_route.total_gas();
        let sell_gas_cost = &sell_gas_units * gas_price; // Convert gas units to actual cost
        let sell_out = sell_route
            .swaps
            .into_iter()
            .last()
            .ok_or(ComputationError::Internal("no output from sell simulation".into()))?
            .amount_out;

        // Convert to f64 for mid_price calculation
        let buy_out_f = buy_out
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing buy_out".into()))?;
        let sell_out_f = sell_out
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing sell_out".into()))?;
        let buy_gas_cost_f = buy_gas_cost
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing buy_gas_cost".into()))?;
        let sell_gas_cost_f = sell_gas_cost
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing sell_gas_cost".into()))?;
        let sim_amount_f = self
            .simulation_amount
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing simulation_amount".into()))?;

        // buy_price: tokens received per (gas_token spent + gas cost)
        let buy_price = buy_out_f / (sim_amount_f + buy_gas_cost_f);

        // sell_price: tokens we had / (gas_token received - gas cost)
        let sell_price = buy_out_f / (sell_out_f - sell_gas_cost_f);

        let spread = (sell_price - buy_price).abs();

        // Compute mid_price in numerator/denominator form (precise BigUint arithmetic)
        // numerator = buy_out * (sell_out - sell_gas_cost) + buy_out * (sim_amount + buy_gas_cost)
        // denominator = 2 * (sim_amount + buy_gas_cost) * (sell_out - sell_gas_cost)
        let buy_price_precise = Price {
            numerator: &buy_out * (&sell_out - &sell_gas_cost) +
                &buy_out * (&self.simulation_amount + &buy_gas_cost),
            denominator: BigUint::from(2u8) *
                (&self.simulation_amount + &buy_gas_cost) *
                (&sell_out - &sell_gas_cost),
        };

        Ok((spread, buy_price_precise))
    }
}

#[async_trait]
impl DerivedComputation for TokenGasPriceComputation {
    type Output = TokenGasPrices;

    const ID: ComputationId = "token_prices";

    #[instrument(level = "debug", skip(market, store), fields(computation_id = Self::ID, updated_token_prices))]
    async fn compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
    ) -> Result<Self::Output, ComputationError> {
        let market = market.read().await;
        let store = store.read().await;
        // Get spot prices (required dependency)
        let spot_prices = store
            .spot_prices()
            .ok_or(ComputationError::MissingDependency("spot_prices"))?;

        // Get gas price for converting gas units to actual cost
        let gas_price = market
            .gas_price()
            .ok_or(ComputationError::MissingDependency("gas_price"))?
            .effective_gas_price();

        let mut graph_manager = PetgraphStableDiGraphManager::new();
        graph_manager.initialize_graph(&market.component_topology());

        // Phase 1: Discover all paths using DFS
        let mut paths_by_token = self.discover_paths(&graph_manager, spot_prices)?;

        // Phase 2: Sort token's paths such that the lowest spread is last (for popping later)
        for paths in paths_by_token.values_mut() {
            paths.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        }

        // Phase 3: Run round-robin and process paths in order of increasing spot-spread score,
        // keeping the best mid-price (based on the lowest spread) per token.
        let mut best_prices = HashMap::new();

        // Flag to indicate there are no more candidates to evaluate for any token.
        let mut candidates_exhausted = false;

        while !candidates_exhausted {
            // Unset at the start of each round
            candidates_exhausted = true;

            for (token, candidate_paths) in paths_by_token.iter_mut() {
                // Skip if no more candidate paths left to evaluate for this token
                let Some(candidate) = candidate_paths.pop() else {
                    continue;
                };
                candidates_exhausted = false; // Found at least one candidate

                match self.compute_spread_and_mid_price(candidate.path, &market, &gas_price) {
                    Ok((spread_ratio, buy_price)) => {
                        // Update if better (lower spread = tighter bid/ask = more reliable price)
                        let is_better = best_prices
                            .get(token)
                            .map(|&(existing_spread, _)| spread_ratio < existing_spread)
                            .unwrap_or(true);

                        if is_better {
                            trace!(
                                token = ?token,
                                spread_ratio = spread_ratio,
                                "found better price (lower spread)"
                            );
                            best_prices.insert(token.clone(), (spread_ratio, buy_price));
                        }
                    }
                    Err(e) => {
                        // Failures are expected as we are simulating with a fixed amount that may
                        // not be suitable for all paths (low liquidity, min/max trade sizes, etc)
                        warn!(token = ?token, error = ?e, "simulation failed");
                    }
                }
            }
        }

        // Remove the spread_ratio from the output
        let mut best_prices: TokenGasPrices = best_prices
            .into_iter()
            .map(|(k, (_, price))| (k, price))
            .collect();

        // Add the gas token itself with price 1:1
        best_prices.insert(
            self.gas_token.clone(),
            Price {
                numerator: self.simulation_amount.clone(),
                denominator: self.simulation_amount.clone(),
            },
        );

        // Report success rate
        let reachable = paths_by_token.len();
        let priced = best_prices.len() - 1; // Exclude gas token itself
        debug!(priced = priced, reachable = reachable, "token price computation complete");

        Span::current().record("updated_token_prices", best_prices.len());

        Ok(best_prices)
    }
}

#[cfg(test)]
mod tests {
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, market_read, setup_market, token, MockProtocolSim},
        derived::{computations::spot_price::SpotPriceComputation, manager::wrap_derived},
        feed::market_data::wrap_market,
        DerivedData,
    };
    // ==================== Constants ====================

    /// Standard simulation amount: 1 ETH = 10^18 wei.
    const SIM_AMOUNT: u128 = 1_000_000_000_000_000_000;

    /// Gas price set by setup_market: 100 wei/gas.
    const GAS_PRICE: u64 = 100;

    // ==================== Test Helpers ====================

    /// Sets up a complete test environment: market with pools + precomputed spot prices.
    /// Returns (market_guard, store) ready for computation.
    async fn setup_test_env(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (SharedMarketDataRef, SharedDerivedDataRef) {
        let (wrapped_market, _) = setup_market(pools);

        let wrapped_store = wrap_derived(DerivedData::new());
        let spot_comp = SpotPriceComputation::new();
        let spot_prices = spot_comp
            .compute(&wrapped_market, &wrapped_store)
            .await
            .expect("spot price computation should succeed");
        wrapped_store
            .try_write()
            .unwrap()
            .set_spot_prices(spot_prices, None);

        (wrapped_market, wrapped_store)
    }

    async fn setup_graph_and_spot_prices(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (PetgraphStableDiGraphManager<()>, SpotPrices) {
        let (market, derived) = setup_test_env(pools).await;
        let market = market_read(&market);

        let mut graph = PetgraphStableDiGraphManager::new();
        graph.initialize_graph(&market.component_topology());

        let spot_prices = derived
            .try_write()
            .unwrap()
            .spot_prices()
            .unwrap()
            .clone();
        (graph, spot_prices)
    }

    /// Creates a computation configured for the given gas token with standard settings.
    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 2, BigUint::from(SIM_AMOUNT))
    }

    // ==================== discover_paths tests ====================

    #[tokio::test]
    async fn test_discover_paths_single_hop() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (graph_manager, spot_prices) =
            setup_graph_and_spot_prices(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000))])
                .await;

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph_manager, &spot_prices)
            .unwrap();

        // Exactly 1 path to USDC (single hop via "pool")
        let usdc_paths = &paths[&usdc.address];
        assert_eq!(usdc_paths.len(), 1, "should have exactly 1 path to USDC");

        let path = &usdc_paths[0];
        assert_eq!(path.path.len(), 1, "path should be single hop");
        assert_eq!(path.path.edge_data[0].component_id, "pool");

        // For a symmetric pool, spread = 0
        assert_eq!(path.score, 0.0);
    }

    #[tokio::test]
    async fn test_discover_paths_multi_hop() {
        let eth = token(0, "ETH");
        let mid = token(2, "MID");
        let target = token(3, "TARGET");

        let (graph, spot_prices) = setup_graph_and_spot_prices(vec![
            ("hop1", &eth, &mid, MockProtocolSim::new(2)),
            ("hop2", &mid, &target, MockProtocolSim::new(3)),
        ])
        .await;

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices)
            .unwrap();

        // MID: exactly 1 path (1-hop via hop1)
        let mid_paths = &paths[&mid.address];
        assert_eq!(mid_paths.len(), 1, "should have exactly 1 path to MID");
        assert_eq!(mid_paths[0].path.len(), 1, "MID path should be 1 hop");
        assert_eq!(mid_paths[0].path.edge_data[0].component_id, "hop1");
        assert_eq!(mid_paths[0].score, 0.0);

        // TARGET: exactly 1 path (2-hop via hop1 → hop2)
        let target_paths = &paths[&target.address];
        assert_eq!(target_paths.len(), 1, "should have exactly 1 path to TARGET");
        assert_eq!(target_paths[0].path.len(), 2, "TARGET path should be 2 hops");
        assert_eq!(target_paths[0].path.edge_data[0].component_id, "hop1");
        assert_eq!(target_paths[0].path.edge_data[1].component_id, "hop2");
        assert_eq!(target_paths[0].score, 0.0);
    }

    #[tokio::test]
    async fn test_discover_paths_respects_max_hops() {
        let eth = token(0, "ETH");
        let a = token(2, "A");
        let b = token(3, "B");
        let c = token(4, "C");

        let (graph, spot_prices) = setup_graph_and_spot_prices(vec![
            ("eth_a", &eth, &a, MockProtocolSim::new(2)),
            ("a_b", &a, &b, MockProtocolSim::new(2)),
            ("b_c", &b, &c, MockProtocolSim::new(2)),
        ])
        .await;

        // max_hops = 2
        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices)
            .unwrap();

        // A: exactly 1 path (1 hop via eth_a)
        let a_paths = &paths[&a.address];
        assert_eq!(a_paths.len(), 1, "should have exactly 1 path to A");
        assert_eq!(a_paths[0].path.len(), 1, "A path should be 1 hop");
        assert_eq!(a_paths[0].path.edge_data[0].component_id, "eth_a");
        assert_eq!(a_paths[0].score, 0.0);

        // B: exactly 1 path (2 hops via eth_a → a_b)
        let b_paths = &paths[&b.address];
        assert_eq!(b_paths.len(), 1, "should have exactly 1 path to B");
        assert_eq!(b_paths[0].path.len(), 2, "B path should be 2 hops");
        assert_eq!(b_paths[0].path.edge_data[0].component_id, "eth_a");
        assert_eq!(b_paths[0].path.edge_data[1].component_id, "a_b");
        assert_eq!(b_paths[0].score, 0.0);

        // C: not reachable (would require 3 hops, exceeds max_hops=2)
        assert!(!paths.contains_key(&c.address), "C should NOT be reachable (3 hops)");
    }

    #[tokio::test]
    async fn test_discover_paths_returns_multiple_candidates() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Two pools with different spot prices
        let (graph, spot_prices) = setup_graph_and_spot_prices(vec![
            ("pool_low", &eth, &usdc, MockProtocolSim::new(1000)),
            ("pool_high", &eth, &usdc, MockProtocolSim::new(2000)),
        ])
        .await;

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices)
            .unwrap();

        // Exactly 2 paths to USDC (one via each pool)
        let usdc_paths = &paths[&usdc.address];
        assert_eq!(usdc_paths.len(), 2, "should have exactly 2 paths to USDC");

        // MockProtocolSim's spot_price is symmetric: forward_spot = 1/reverse_spot,
        // so spread = |forward - 1/reverse| = 0 for all pools.
        // TODO: Test with asymmetric simulation component to verify non-zero spread ranking.
        for path in usdc_paths {
            assert_eq!(path.path.len(), 1, "path should be single hop");
            assert_eq!(path.score, 0.0, "symmetric mock produces zero spread");
        }

        // Verify both pools are discovered (order is arbitrary when scores are equal)
        let component_ids: Vec<_> = usdc_paths
            .iter()
            .map(|p| {
                p.path.edge_data[0]
                    .component_id
                    .as_str()
            })
            .collect();
        assert!(component_ids.contains(&"pool_low"));
        assert!(component_ids.contains(&"pool_high"));
    }

    // ==================== compute_spread_and_mid_price tests ====================

    #[tokio::test]
    async fn test_compute_spread_and_mid_price_with_gas_and_fee() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Non-trivial setup: 10% fee + significant gas (10% of sim_amount)
        // gas_units = 1e15, gas_cost = 1e15 * 100 = 1e17 (10% of 1e18)
        //
        // Forward (ETH→USDC):
        //   buy_out = 1e18 * 2000 * 0.9 = 1.8e21
        //   buy_gas_cost = 1e17
        //
        // Reverse (USDC→ETH):
        //   sell_out = 1.8e21 / 2000 * 0.9 = 8.1e17
        //   sell_gas_cost = 1e17
        //
        // buy_price = buy_out / (sim_amount + buy_gas_cost)
        //           = 1.8e21 / (1e18 + 1e17) = 1.8e21 / 1.1e18 = 18000/11 ≈ 1636.36
        //
        // sell_price = buy_out / (sell_out - sell_gas_cost)
        //            = 1.8e21 / (8.1e17 - 1e17) = 1.8e21 / 7.1e17 = 180000/71 ≈ 2535.21
        //
        // spread = |sell_price - buy_price| = 180000/71 - 18000/11 = 702000/781 ≈ 898.85
        // mid_price = (buy_price + sell_price) / 2 ≈ 2085.79
        let gas_units: u64 = 1_000_000_000_000_000; // 1e15
        let (market, _) = setup_test_env(vec![(
            "pool",
            &eth,
            &usdc,
            MockProtocolSim::new(2000)
                .with_gas(gas_units)
                .with_fee(0.1),
        )])
        .await;
        let market = market_read(&market);

        // Build path manually using graph
        let mut graph = PetgraphStableDiGraphManager::new();
        graph.initialize_graph(&market.component_topology());

        let eth_node = graph.find_node(&eth.address).unwrap();
        let path_edges: Vec<_> = graph.graph().edges(eth_node).collect();
        assert_eq!(path_edges.len(), 1);

        let edge = path_edges[0].weight();
        let mut path = Path::new();
        path.add_hop(&eth.address, edge, &usdc.address);

        let gas_price = BigUint::from(GAS_PRICE);
        let computation = computation_for(&eth.address);
        let (spread, mid_price) = computation
            .compute_spread_and_mid_price(path, &market, &gas_price)
            .unwrap();

        // Expected values from exact fractions
        let buy_price = 18000.0 / 11.0; // 1636.363636...
        let sell_price = 180000.0 / 71.0; // 2535.211267...
        let expected_spread = sell_price - buy_price; // ~898.85
        let expected_mid = (buy_price + sell_price) / 2.0; // ~2085.79

        assert!(
            (spread - expected_spread).abs() < 1e-5,
            "spread should be {expected_spread}, got {spread}"
        );

        let ratio = mid_price.numerator.to_f64().unwrap() / mid_price.denominator.to_f64().unwrap();
        assert!(
            (ratio - expected_mid).abs() < 1e-5,
            "mid_price should be {expected_mid}, got {ratio}"
        );
    }

    // ==================== compute tests ====================

    #[tokio::test]
    async fn test_compute_single_hop_mid_price() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let spot_price: u32 = 2000;
        let gas_units: u64 = 50_000;

        let (market, derived) = setup_test_env(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price).with_gas(gas_units),
        )])
        .await;

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived)
            .await
            .unwrap();

        // Exactly 2 prices: ETH (gas token) and USDC
        assert_eq!(prices.len(), 2, "should have exactly 2 token prices");

        // Gas token (ETH) should have exact 1:1 price
        let eth_price = prices
            .get(&eth.address)
            .expect("ETH should have price");
        assert_eq!(
            eth_price.numerator, eth_price.denominator,
            "gas token must have exact 1:1 price"
        );
        assert_eq!(
            eth_price.numerator,
            BigUint::from(SIM_AMOUNT),
            "gas token numerator should equal simulation amount"
        );

        // USDC mid-price should be 2000 (symmetric pool, no fee)
        // Small deviation due to gas cost adjustment in buy_price/sell_price
        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        let ratio =
            usdc_price.numerator.to_f64().unwrap() / usdc_price.denominator.to_f64().unwrap();
        assert!((ratio - 2000.0).abs() < 1e-6, "mid-price should be ~2000, got {ratio}");
    }

    #[tokio::test]
    async fn test_compute_selects_best_path_by_spread() {
        // Diamond topology: two paths to C
        //
        //     A (10% fee on eth_a)
        //    / \
        // ETH   C
        //    \ /
        //     B (5% fee on eth_b)
        //
        // Only first hops have fees; second hops (a_c, b_c) are fee-free.
        // Gas = 0 to simplify calculations.
        //
        // Path via A (eth_a=10% fee, a_c=0% fee):
        //   Forward: 1e18 * 2 * 0.9 * 5 = 9e18
        //   Reverse: 9e18 / 5 / 2 * 0.9 = 0.81e18
        //   buy_price = 9, sell_price = 9/0.81 = 100/9
        //   spread_A = |100/9 - 9| = 19/9 ≈ 2.11
        //
        // Path via B (eth_b=5% fee, b_c=0% fee):
        //   Forward: 1e18 * 3 * 0.95 * 2 = 5.7e18 = (57/10)e18
        //   Reverse: 5.7e18 / 2 / 3 * 0.95 = 0.9025e18 = (361/400)e18
        //   buy_price = 57/10, sell_price = (57/10)/(361/400) = 2280/361
        //   spread_B = |2280/361 - 57/10| = 2223/3610 ≈ 0.62
        //
        // spread_B < spread_A → Path via B selected.
        let eth = token(0, "ETH");
        let a = token(2, "A");
        let b = token(3, "B");
        let c = token(4, "C");

        let (market, derived) = setup_test_env(vec![
            (
                "eth_a",
                &eth,
                &a,
                MockProtocolSim::new(2)
                    .with_fee(0.1)
                    .with_gas(0),
            ),
            ("a_c", &a, &c, MockProtocolSim::new(5).with_gas(0)),
            (
                "eth_b",
                &eth,
                &b,
                MockProtocolSim::new(3)
                    .with_fee(0.05)
                    .with_gas(0),
            ),
            ("b_c", &b, &c, MockProtocolSim::new(2).with_gas(0)),
        ])
        .await;

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived)
            .await
            .unwrap();

        assert_eq!(prices.len(), 4, "should have prices for ETH, A, B, C");

        // A: 1-hop from ETH with 10% fee
        // buy_out = 1e18 * 2 * 0.9 = 1.8e18 = (9/5)e18
        // sell_out = 1.8e18 / 2 * 0.9 = 0.81e18 = (81/100)e18
        // buy_price = 9/5, sell_price = (9/5)/(81/100) = 9*100/(5*81) = 20/9
        // mid_price = (9/5 + 20/9) / 2 = (81 + 100) / 90 = 181/90
        let a_price = prices
            .get(&a.address)
            .expect("A should have price");
        let a_ratio = a_price.numerator.to_f64().unwrap() / a_price.denominator.to_f64().unwrap();
        let expected_a = 181.0 / 90.0;
        assert!(
            (a_ratio - expected_a).abs() < 1e-10,
            "A mid_price should be 181/90 = {expected_a}, got {a_ratio}"
        );

        // B: 1-hop from ETH with 5% fee
        // buy_out = 1e18 * 3 * 0.95 = 2.85e18 = (57/20)e18
        // sell_out = 2.85e18 / 3 * 0.95 = 0.9025e18 = (361/400)e18
        // buy_price = 57/20, sell_price = (57/20)/(361/400) = 57*400/(20*361) = 1140/361
        // mid_price = (57/20 + 1140/361) / 2 = (57*361 + 1140*20) / (2*20*361)
        //           = (20577 + 22800) / 14440 = 43377/14440
        let b_price = prices
            .get(&b.address)
            .expect("B should have price");
        let b_ratio = b_price.numerator.to_f64().unwrap() / b_price.denominator.to_f64().unwrap();
        let expected_b = 43377.0 / 14440.0;
        assert!(
            (b_ratio - expected_b).abs() < 1e-10,
            "B mid_price should be 43377/14440 = {expected_b}, got {b_ratio}"
        );

        // C: Path via B selected (lower spread)
        // buy_out = 1e18 * 3 * 0.95 * 2 = 5.7e18 = (57/10)e18
        // sell_out = 5.7e18 / 2 / 3 * 0.95 = 0.9025e18 = (361/400)e18
        // buy_price = 57/10, sell_price = (57/10)/(361/400) = 2280/361
        // mid_price = (57/10 + 2280/361) / 2 = (20577 + 22800) / 7220 = 43377/7220
        let c_price = prices
            .get(&c.address)
            .expect("C should have price");
        let c_ratio = c_price.numerator.to_f64().unwrap() / c_price.denominator.to_f64().unwrap();
        let expected_c = 43377.0 / 7220.0;
        assert!(
            (c_ratio - expected_c).abs() < 1e-10,
            "C mid_price should be 43377/7220 = {expected_c} (via B), got {c_ratio}"
        );
    }

    #[tokio::test]
    async fn test_compute_missing_spot_prices_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Create market without spot prices set
        let (market, _) = setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000))]);
        let derived = wrap_derived(DerivedData::new()); // No spot prices

        let computation = computation_for(&eth.address);
        let result = computation
            .compute(&market, &derived)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("spot_prices"))),
            "should return MissingDependency for spot_prices"
        );
    }

    #[tokio::test]
    async fn test_compute_gas_token_with_no_pools_returns_only_self() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let dai = token(2, "DAI");

        // Create a pool that doesn't include ETH (gas token)
        let (market, derived) =
            setup_test_env(vec![("usdc_dai", &usdc, &dai, MockProtocolSim::new(1))]).await;

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived)
            .await
            .unwrap();

        // Only the gas token itself should have a price (1:1)
        assert_eq!(prices.len(), 1, "should only have gas token price");
        let eth_price = prices
            .get(&eth.address)
            .expect("ETH should have price");
        assert_eq!(
            eth_price.numerator, eth_price.denominator,
            "gas token must have exact 1:1 price"
        );
    }

    #[tokio::test]
    async fn test_compute_missing_gas_price_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Create market without gas price set
        let mut market = SharedMarketData::new();
        let comp = component("pool", &[eth.clone(), usdc.clone()]);
        market.upsert_components(std::iter::once(comp));
        market.update_states([("pool".to_string(), Box::new(MockProtocolSim::new(2000)) as _)]);
        market.upsert_tokens([eth.clone(), usdc.clone()]);
        let market = wrap_market(market);

        // Compute spot prices
        let derived = wrap_derived(DerivedData::new());

        let spot_comp = SpotPriceComputation::new();
        let spot_prices = spot_comp
            .compute(&market, &derived)
            .await
            .unwrap();
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_prices, None);

        let computation = computation_for(&eth.address);
        let result = computation
            .compute(&market, &derived)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("gas_price"))),
            "should return MissingDependency for gas_price"
        );
    }
}
