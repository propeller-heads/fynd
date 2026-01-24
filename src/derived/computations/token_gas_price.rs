//! Computes the `mid_price` of tokens relative to a gas token (e.g., ETH), selecting paths
//! by the lowest spread (the most reliable price) derived from full simulation of both buy and sell
//! directions.
//!
//! # Algorithm
//!
//! 1. **Path Discovery (DFS)**: Enumerate all paths from gas_token to each reachable token, scoring
//!    by composed spot prices (forward × reverse) as a heuristic for path quality.
//!
//! 2. **Sort**: Order paths per token by spot-based mid-price estimate.
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

use std::collections::HashMap;

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, instrument, trace, warn, Span};
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::Price};

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        computations::spot_price::{SpotPriceKey, SpotPrices},
        error::ComputationError,
        store::DerivedDataStore,
    },
    feed::market_data::SharedMarketData,
    graph::{GraphManager, Path, PetgraphStableDiGraphManager},
    MostLiquidAlgorithm,
};

/// Key for token price lookups.
pub type TokenGasPriceKey = Address;

/// Token prices map: token address → price ratio.
pub type TokenGasPrices = HashMap<TokenGasPriceKey, Price>;

/// A path with its score
#[derive(Clone)]
struct CandidatePath<'a> {
    edges: Path<'a, ()>,
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

    pub fn simulation_amount(&self) -> &BigUint {
        &self.simulation_amount
    }

    /// DFS to discover all paths from gas_token, scored by spot prices.
    fn discover_paths<'a>(
        &self,
        graph_manager: &'a PetgraphStableDiGraphManager<()>,
        spot_prices: &SpotPrices,
    ) -> Result<HashMap<Address, Vec<CandidatePath<'a>>>, ComputationError> {
        let graph = graph_manager.graph();

        let entry_node = graph_manager
            .find_node(&self.gas_token)
            .expect("gas token node must exist in graph");

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
            // Stop if max depth reached
            if frame.path.len() >= self.max_hops {
                continue;
            }

            // Token that we reached in this frame
            let token_reached = &graph[frame.token_node];

            // Explore neighbors
            for edge in graph.edges(frame.token_node) {
                // We can assume that the graph is directed and edges point to neighbors
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

                let Some(&fwd_spot) = spot_prices.get(&fwd_key) else {
                    continue;
                };
                let Some(&rev_spot) = spot_prices.get(&rev_key) else {
                    continue;
                };

                stack.push(DfsFrame {
                    token_node: next_node,
                    path: new_path,
                    forward_spot: frame.forward_spot * fwd_spot,
                    reverse_spot: frame.reverse_spot * rev_spot,
                });
            }

            // Record the path for the token we have reached
            let mid_spot_score = (frame.forward_spot + frame.reverse_spot) / 2.0;
            paths_by_token
                .entry(token_reached.clone())
                .or_default()
                .push(CandidatePath { edges: frame.path, score: mid_spot_score });
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

impl DerivedComputation for TokenGasPriceComputation {
    type Output = TokenGasPrices;

    const ID: ComputationId = "token_prices";

    #[instrument(level = "debug", skip(market, store), fields(computation_id = Self::ID, updated_token_prices))]
    fn compute(
        &self,
        market: &SharedMarketData,
        store: &DerivedDataStore,
    ) -> Result<Self::Output, ComputationError> {
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

        // Phase 2: Sort token's paths in ascending order (highest spot-price last)
        for paths in paths_by_token.values_mut() {
            paths.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap());
        }

        // Phase 2: Run round-robin and process paths in order of decreasing score (best paths first
        // via pop from ascending-sorted vec), and keep the best price (lowest spread) per token.
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

                match self.compute_spread_and_mid_price(candidate.edges, market, &gas_price) {
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
    use tycho_simulation::tycho_ethereum::gas::{BlockGasPrice, GasPrice};

    use super::*;
    use crate::{
        algorithm::test_utils::{market_read, setup_market, token, MockProtocolSim},
        derived::computations::spot_price::SpotPriceComputation,
    };

    // ==================== Test Constants & Helpers ====================

    /// Standard simulation amount: 1 ETH = 10^18 wei.
    const SIM_AMOUNT: u128 = 1_000_000_000_000_000_000;

    /// Computes spot prices from market and stores them in the derived data store.
    fn with_spot_prices(market: &SharedMarketData, store: &mut DerivedDataStore) {
        let spot_comp = SpotPriceComputation::new();
        let spot_prices = spot_comp
            .compute(market, store)
            .expect("spot price computation should succeed");
        store.set_spot_prices(spot_prices, None);
    }

    /// Creates a computation configured for the given gas token with standard settings.
    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 2, BigUint::from(SIM_AMOUNT))
    }

    /// Creates an expected Price with exact numerator and denominator.
    fn expected_price(numerator: impl Into<BigUint>, denominator: impl Into<BigUint>) -> Price {
        Price { numerator: numerator.into(), denominator: denominator.into() }
    }

    /// Asserts that two Price values are exactly equal, with descriptive error messages.
    fn assert_price_eq(actual: &Price, expected: &Price, context: &str) {
        assert_eq!(
            actual.numerator, expected.numerator,
            "{context}: numerator mismatch - expected {}, got {}",
            expected.numerator, actual.numerator
        );
        assert_eq!(
            actual.denominator, expected.denominator,
            "{context}: denominator mismatch - expected {}, got {}",
            expected.denominator, actual.denominator
        );
    }

    /// Asserts that a price's numerator is one of the expected valid values.
    fn assert_price_numerator_in(actual: &Price, valid: &[BigUint], context: &str) {
        assert!(
            valid.contains(&actual.numerator),
            "{context}: numerator {} not in valid set {:?}",
            actual.numerator,
            valid
        );
    }

    // ==================== Category 1: Basic Functionality ====================

    #[test]
    fn computation_id() {
        assert_eq!(TokenGasPriceComputation::ID, "token_prices");
    }

    #[test]
    fn gas_token_has_identity_price() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, _) =
            setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0))]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // Gas token must have exact 1:1 price (numerator == denominator)
        let eth_price = prices
            .get(&eth.address)
            .expect("gas token should have price");
        let sim_amount = BigUint::from(SIM_AMOUNT);
        assert_price_eq(
            eth_price,
            &expected_price(sim_amount.clone(), sim_amount),
            "gas token identity price",
        );
    }

    #[test]
    fn single_hop_price_exact() {
        // Token ordering determines MockProtocolSim behavior:
        // When token_in.address < token_out.address: amount_out = amount_in * spot_price
        let eth = token(0, "ETH"); // address 0x00...00
        let usdc = token(1, "USDC"); // address 0x01...01
        assert!(eth.address < usdc.address, "test requires ETH < USDC for MockProtocolSim");

        let spot_price: u32 = 2000;
        let gas_units: u64 = 0;

        let (market, _) = setup_market(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price).with_gas(gas_units),
        )]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // Expected calculation:
        // buy_out = SIM_AMOUNT * spot_price = 1e18 * 2000 = 2e21
        // gas_cost = gas_units * gas_price = 0 * 1 = 0
        // Price = { numerator: 2e21, denominator: 1e18 }
        let expected_numerator = BigUint::from(SIM_AMOUNT) * spot_price;
        let expected_denominator = BigUint::from(SIM_AMOUNT) + gas_units;

        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        assert_price_eq(
            usdc_price,
            &expected_price(expected_numerator, expected_denominator),
            "USDC single-hop price",
        );
    }

    #[test]
    fn multi_hop_price_composition() {
        // Chain: ETH (0) → A (2) → B (3)
        // MockProtocolSim multiplies when token_in < token_out
        let eth = token(0, "ETH");
        let token_a = token(2, "A");
        let token_b = token(3, "B");

        assert!(eth.address < token_a.address);
        assert!(token_a.address < token_b.address);

        let rate_eth_a: u32 = 2;
        let rate_a_b: u32 = 3;

        let (market, _) = setup_market(vec![
            ("eth_a", &eth, &token_a, MockProtocolSim::new(rate_eth_a).with_gas(0)),
            ("a_b", &token_a, &token_b, MockProtocolSim::new(rate_a_b).with_gas(0)),
        ]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // Expected composed rate: 2 * 3 = 6
        // buy_out = SIM_AMOUNT * 2 * 3 = 6e18
        let expected_numerator = BigUint::from(SIM_AMOUNT) * rate_eth_a * rate_a_b;
        let expected_denominator = BigUint::from(SIM_AMOUNT);

        let price_b = prices
            .get(&token_b.address)
            .expect("token B should have price");
        assert_price_eq(
            price_b,
            &expected_price(expected_numerator, expected_denominator),
            "token B multi-hop price",
        );
    }

    // ==================== Category 2: Gas Impact ====================

    #[test]
    fn gas_cost_increases_denominator() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let spot_price: u32 = 2000;
        let gas_units: u64 = 50_000;
        // setup_market sets gas_price = 1 wei/gas

        let (market, _) = setup_market(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price).with_gas(gas_units),
        )]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // Expected:
        // buy_out = SIM_AMOUNT * spot_price
        // gas_cost = gas_units * 1 (gas_price) = 50_000 wei
        // denominator = SIM_AMOUNT + 50_000
        let expected_numerator = BigUint::from(SIM_AMOUNT) * spot_price;
        let expected_denominator = BigUint::from(SIM_AMOUNT) + gas_units;

        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        assert_price_eq(
            usdc_price,
            &expected_price(expected_numerator, expected_denominator),
            "USDC price with gas cost",
        );

        // Verify gas actually increased denominator beyond just SIM_AMOUNT
        assert!(
            usdc_price.denominator > BigUint::from(SIM_AMOUNT),
            "gas cost should increase denominator"
        );
    }

    #[test]
    fn higher_gas_price_increases_denominator_proportionally() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let spot_price: u32 = 2000;
        let gas_units: u64 = 50_000;
        let high_gas_price: u64 = 100; // 100 wei/gas instead of default 1

        let (market_lock, _) = setup_market(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price).with_gas(gas_units),
        )]);

        // Override gas price to higher value
        {
            let mut market_write = market_lock.try_write().unwrap();
            market_write.update_gas_price(BlockGasPrice {
                block_number: 1,
                block_hash: Default::default(),
                block_timestamp: 0,
                pricing: GasPrice::Legacy { gas_price: BigUint::from(high_gas_price) },
            });
        }

        let market_guard = market_read(&market_lock);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // Expected:
        // gas_cost = gas_units * high_gas_price = 50_000 * 100 = 5_000_000 wei
        // denominator = SIM_AMOUNT + 5_000_000
        let gas_cost_wei = gas_units * high_gas_price;
        let expected_numerator = BigUint::from(SIM_AMOUNT) * spot_price;
        let expected_denominator = BigUint::from(SIM_AMOUNT) + gas_cost_wei;

        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        assert_price_eq(
            usdc_price,
            &expected_price(expected_numerator, expected_denominator),
            "USDC price with high gas price",
        );
    }

    // ==================== Category 3: Spread-Based Selection ====================

    #[test]
    fn selects_lower_spread_path() {
        // Two pools for the same pair with different fees:
        // - Pool A: no fee → symmetric buy/sell → spread ≈ 0
        // - Pool B: 10% fee → asymmetric → spread > 0
        // Selection should prefer Pool A (lower spread)
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let spot_price: u32 = 2000;

        let (market, _) = setup_market(vec![
            // Pool A: no fee produces symmetric prices, minimal spread
            (
                "pool_a",
                &eth,
                &usdc,
                MockProtocolSim::new(spot_price)
                    .with_gas(0)
                    .with_fee(0.0),
            ),
            // Pool B: 10% fee creates bid/ask spread
            (
                "pool_b",
                &eth,
                &usdc,
                MockProtocolSim::new(spot_price)
                    .with_gas(0)
                    .with_fee(0.1),
            ),
        ]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // Pool A (no fee) should be selected due to lower spread
        // buy_out = SIM_AMOUNT * spot_price (no fee reduction)
        let expected_numerator = BigUint::from(SIM_AMOUNT) * spot_price;
        let expected_denominator = BigUint::from(SIM_AMOUNT);

        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        assert_price_eq(
            usdc_price,
            &expected_price(expected_numerator, expected_denominator),
            "USDC should use no-fee pool (lower spread)",
        );
    }

    #[test]
    fn symmetric_pools_returns_valid_price() {
        // Multiple symmetric pools (no fee) all have spread ≈ 0
        // Any valid path is acceptable
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, _) = setup_market(vec![
            ("pool1", &eth, &usdc, MockProtocolSim::new(1800).with_gas(0)),
            ("pool2", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0)),
            ("pool3", &eth, &usdc, MockProtocolSim::new(1900).with_gas(0)),
        ]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // TODO - make this pass
        let expected_numerator = BigUint::from(SIM_AMOUNT);
        let expected_denominator = BigUint::from(SIM_AMOUNT);

        // Should have a price for USDC from one of the pools
        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        assert_price_eq(
            usdc_price,
            &expected_price(expected_numerator, expected_denominator),
            "USDC should use no-fee pool (lower spread)",
        );
    }

    // ==================== Category 4: Path Discovery ====================

    #[test]
    fn discovers_two_hop_paths() {
        // No direct path from ETH to TARGET, only via MID
        let eth = token(0, "ETH");
        let mid = token(2, "MID");
        let target = token(3, "TARGET");

        let rate1: u32 = 2;
        let rate2: u32 = 4;

        let (market, _) = setup_market(vec![
            ("hop1", &eth, &mid, MockProtocolSim::new(rate1).with_gas(0)),
            ("hop2", &mid, &target, MockProtocolSim::new(rate2).with_gas(0)),
        ]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // TARGET should be reachable via 2-hop path with composed rate: 2 * 4 = 8
        let expected_numerator = BigUint::from(SIM_AMOUNT) * rate1 * rate2;
        let expected_denominator = BigUint::from(SIM_AMOUNT);

        let target_price = prices
            .get(&target.address)
            .expect("TARGET should be reachable");
        assert_price_eq(
            target_price,
            &expected_price(expected_numerator, expected_denominator),
            "TARGET 2-hop price",
        );
    }

    #[test]
    fn diamond_returns_valid_price() {
        // Diamond topology: two paths to token C
        // Path 1: ETH → A → C (rate 2 * 5 = 10)
        // Path 2: ETH → B → C (rate 3 * 2 = 6)
        // Both are symmetric, so either is valid
        let eth = token(0, "ETH");
        let token_a = token(2, "A");
        let token_b = token(3, "B");
        let token_c = token(4, "C");

        let (market, _) = setup_market(vec![
            ("eth_a", &eth, &token_a, MockProtocolSim::new(2).with_gas(0)),
            ("a_c", &token_a, &token_c, MockProtocolSim::new(5).with_gas(0)),
            ("eth_b", &eth, &token_b, MockProtocolSim::new(3).with_gas(0)),
            ("b_c", &token_b, &token_c, MockProtocolSim::new(2).with_gas(0)),
        ]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // C should have a price from one of the paths
        let price_c = prices
            .get(&token_c.address)
            .expect("token C should have price");

        // Valid rates: Path 1 = 2*5 = 10, Path 2 = 3*2 = 6
        let valid_numerators =
            [BigUint::from(SIM_AMOUNT) * 10u32, BigUint::from(SIM_AMOUNT) * 6u32];
        assert_price_numerator_in(price_c, &valid_numerators, "token C from diamond paths");
    }

    #[test]
    fn respects_max_hops_limit() {
        // Chain: ETH → A → B → C (3 hops total)
        // With max_hops=2, C should NOT be reachable
        let eth = token(0, "ETH");
        let token_a = token(2, "A");
        let token_b = token(3, "B");
        let token_c = token(4, "C");

        let (market, _) = setup_market(vec![
            ("eth_a", &eth, &token_a, MockProtocolSim::new(2).with_gas(0)),
            ("a_b", &token_a, &token_b, MockProtocolSim::new(3).with_gas(0)),
            ("b_c", &token_b, &token_c, MockProtocolSim::new(4).with_gas(0)),
        ]);

        let market_guard = market_read(&market);
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 2, BigUint::from(SIM_AMOUNT));
        let prices = computation
            .compute(&market_guard, &store)
            .unwrap();

        // A (1 hop) and B (2 hops) should be reachable
        assert!(prices.get(&token_a.address).is_some(), "A should be reachable (1 hop)");
        assert!(prices.get(&token_b.address).is_some(), "B should be reachable (2 hops)");

        // C requires 3 hops, exceeds max_hops=2
        assert!(prices.get(&token_c.address).is_none(), "C should NOT be reachable (3 hops)");
    }

    // ==================== Category 5: Error Handling ====================

    #[test]
    fn missing_spot_prices_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, _) =
            setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0))]);

        let market_guard = market_read(&market);
        let store = DerivedDataStore::new(); // Intentionally no spot prices

        let computation = computation_for(&eth.address);
        let result = computation.compute(&market_guard, &store);

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("spot_prices"))),
            "should return MissingDependency error for spot_prices"
        );
    }

    #[test]
    fn missing_gas_price_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Market without gas_price set
        let (market_lock, _) =
            setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0))]);

        let market_guard = market_lock.try_read().unwrap();
        let mut store = DerivedDataStore::new();
        with_spot_prices(&market_guard, &mut store);

        let computation = computation_for(&eth.address);
        let result = computation.compute(&market_guard, &store);

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("gas_price"))),
            "should return MissingDependency error for gas_price"
        );
    }
}
