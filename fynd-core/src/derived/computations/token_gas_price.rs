//! Computes the `mid_price` of tokens relative to a gas token (e.g., ETH), using
//! Bellman-Ford SPFA to find the optimal path per token and full simulation of both
//! buy and sell directions to derive spread and mid-price.
//!
//! # Algorithm
//!
//! 1. **BF Forward Pass (one-to-all)**: Run SPFA from gas_token with a probe amount. Each token's
//!    distance = the best amount reachable via simulation during relaxation. This replaces DFS path
//!    enumeration AND spot-price scoring in a single pass.
//!
//! 2. **Reverse Simulation**: For each priced token, reverse the winning path and simulate the sell
//!    direction. Compute spread and mid_price from forward + reverse amounts.
//!
//! # Price Formulas
//!
//! For a path P from gas_token to target:
//! - `buy_out` = simulate(P, probe_amount) -> tokens received
//! - `sell_out` = simulate(reverse(P), buy_out) -> gas_token received back
//! - `buy_price` = buy_out / (probe_amount + gas_cost)
//! - `sell_price` = buy_out / (sell_out - gas_cost)
//! - `mid_price` = (buy_price + sell_price) / 2
//! - `spread` = |sell_price - buy_price|

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use petgraph::graph::NodeIndex;
use tracing::{debug, instrument, trace, Span};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    algorithm::bellman_ford_pricing::{resimulate_path, solve_one_to_all, SpfaAllResult},
    derived::{
        computation::{ComputationId, DerivedComputation},
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        types::{TokenGasPrices, TokenPriceEntry, TokenPricesWithDeps},
    },
    feed::market_data::{SharedMarketData, SharedMarketDataRef},
    graph::{GraphManager, PetgraphStableDiGraphManager},
    types::ComponentId,
};

/// Computes token prices relative to the gas token using Bellman-Ford SPFA.
///
/// Runs a single BF forward pass to find the optimal path per token, then
/// simulates the reverse direction on each winning path to compute spread
/// and mid-price.
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
    #[cfg(test)]
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

    /// Computes spread and mid_price for a given BF path by simulating both directions.
    ///
    /// Returns (spread_ratio, mid_price, path_components) where:
    /// - spread_ratio: |sell - buy|, lower = more reliable
    /// - mid_price: precise Price struct
    /// - path_components: component IDs used in this path (for incremental invalidation)
    fn compute_spread_and_mid_price(
        &self,
        forward_path: &[(NodeIndex, NodeIndex, ComponentId)],
        market: &SharedMarketData,
        gas_price: &BigUint,
        spfa_result: &SpfaAllResult,
    ) -> Result<(f64, Price, HashSet<ComponentId>), ComputationError> {
        let path_components: HashSet<ComponentId> = forward_path
            .iter()
            .map(|(_, _, cid)| cid.clone())
            .collect();

        let token_map = spfa_result.token_map();

        // Forward: gas_token -> target_token
        let (forward_route, buy_out) =
            resimulate_path(forward_path, &self.simulation_amount, market, token_map).map_err(
                |e| ComputationError::SimulationFailed(format!("buy simulation failed: {}", e)),
            )?;
        let buy_gas_cost = forward_route.total_gas() * gas_price;

        // Reverse: target_token -> gas_token
        let reversed_path: Vec<_> = forward_path
            .iter()
            .rev()
            .map(|(from, to, cid)| (*to, *from, cid.clone()))
            .collect();

        let (reverse_route, sell_out) =
            resimulate_path(&reversed_path, &buy_out, market, token_map).map_err(|e| {
                ComputationError::SimulationFailed(format!("sell simulation failed: {}", e))
            })?;
        let sell_gas_cost = reverse_route.total_gas() * gas_price;

        // Convert to f64 for spread calculation
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

        // Guard: if gas cost exceeds sell output, this path is not viable
        if sell_gas_cost >= sell_out {
            return Err(ComputationError::SimulationFailed(
                "gas cost exceeds sell output - path not viable".into(),
            ));
        }

        // buy_price: tokens received per (gas_token spent + gas cost)
        let buy_price = buy_out_f / (sim_amount_f + buy_gas_cost_f);

        // sell_price: tokens we had / (gas_token received - gas cost)
        let sell_price = buy_out_f / (sell_out_f - sell_gas_cost_f);

        let spread = (sell_price - buy_price).abs();

        // Compute mid_price in numerator/denominator form (precise BigUint arithmetic)
        let sell_out_net = &sell_out - &sell_gas_cost; // Safe: checked above
        let buy_price_precise = Price {
            numerator: &buy_out * &sell_out_net +
                &buy_out * (&self.simulation_amount + &buy_gas_cost),
            denominator: BigUint::from(2u8) *
                (&self.simulation_amount + &buy_gas_cost) *
                sell_out_net,
        };

        Ok((spread, buy_price_precise, path_components))
    }

    /// Core pricing logic: runs BF forward pass, then reverse-simulates each winning path.
    ///
    /// If `filter_tokens` is `Some`, only prices those tokens (incremental mode).
    /// If `None`, prices all reachable tokens (full mode).
    #[allow(clippy::type_complexity)]
    fn simulate_token_prices(
        &self,
        market: &SharedMarketData,
        gas_price: &BigUint,
        filter_tokens: Option<&HashSet<Address>>,
    ) -> Result<HashMap<Address, (f64, Price, HashSet<ComponentId>)>, ComputationError> {
        let mut graph_manager = PetgraphStableDiGraphManager::new();
        graph_manager.initialize_graph(&market.component_topology());
        let graph = graph_manager.graph();

        // If gas token has no pools, it won't be in the graph
        let Ok(source_node) = graph_manager.find_node(&self.gas_token) else {
            return Ok(HashMap::new());
        };

        // BF forward pass: prices all tokens in one traversal
        let spfa_result = solve_one_to_all(
            source_node,
            self.simulation_amount.clone(),
            self.max_hops,
            graph,
            market,
        );

        let token_map = spfa_result.token_map();
        let mut best_prices: HashMap<Address, (f64, Price, HashSet<ComponentId>)> = HashMap::new();

        for (&node, token) in token_map {
            // Skip source token and unreachable tokens
            if token.address == self.gas_token || !spfa_result.is_reachable(node) {
                continue;
            }

            // Optionally filter to only requested tokens
            if let Some(filter) = filter_tokens {
                if !filter.contains(&token.address) {
                    continue;
                }
            }

            // Reconstruct the winning forward path
            let forward_path = match spfa_result.reconstruct_path(node) {
                Ok(p) => p,
                Err(e) => {
                    trace!(
                        token = ?token.address,
                        error = %e,
                        "failed to reconstruct path, skipping"
                    );
                    continue;
                }
            };

            // Compute spread and mid-price via forward + reverse simulation
            match self.compute_spread_and_mid_price(&forward_path, market, gas_price, &spfa_result)
            {
                Ok((spread, price, components)) => {
                    trace!(
                        token = ?token.address,
                        spread_ratio = spread,
                        "computed token price"
                    );
                    best_prices.insert(token.address.clone(), (spread, price, components));
                }
                Err(e) => {
                    trace!(
                        token = ?token.address,
                        error = %e,
                        "price computation failed, skipping"
                    );
                }
            }
        }

        Ok(best_prices)
    }

    /// Attempts incremental recomputation for state-only changes.
    ///
    /// Only recomputes token prices whose dependency paths intersect with changed components.
    /// Returns `Ok(Some(prices))` if incremental recomputation succeeded,
    /// `Ok(None)` if full recomputation is needed (e.g., no dependencies stored yet),
    /// or `Err` if computation failed.
    async fn try_incremental_compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Option<TokenGasPrices>, ComputationError> {
        let store_guard = store.read().await;

        // Need existing deps to do incremental computation
        let Some(existing_deps) = store_guard.token_prices_deps() else {
            return Ok(None); // No deps stored yet, need full compute
        };
        let Some(existing_prices) = store_guard.token_prices() else {
            return Ok(None);
        };

        let changed_components = changed.all_changed_ids();

        // Find tokens whose paths intersect with changed components
        let tokens_to_recompute: HashSet<Address> = existing_deps
            .iter()
            .filter(|(_, entry)| {
                !entry
                    .path_components
                    .is_disjoint(&changed_components)
            })
            .map(|(addr, _)| addr.clone())
            .collect();

        if tokens_to_recompute.is_empty() {
            return Ok(Some(existing_prices.clone()));
        }

        let existing_prices = existing_prices.clone();
        let existing_deps = existing_deps.clone();
        drop(store_guard);

        debug!(
            affected_tokens = tokens_to_recompute.len(),
            total_tokens = existing_prices.len(),
            "incremental token price recomputation"
        );

        let market = market.read().await;
        let block = market
            .last_updated()
            .map(|b| b.number())
            .unwrap_or(0);

        let gas_price = market
            .gas_price()
            .ok_or(ComputationError::MissingDependency("gas_price"))?
            .effective_gas_price();

        let best_prices =
            self.simulate_token_prices(&market, &gas_price, Some(&tokens_to_recompute))?;

        // Merge results into existing prices and deps
        let mut result = existing_prices;
        let mut new_deps = existing_deps;

        for token in &tokens_to_recompute {
            if let Some((_, price, components)) = best_prices.get(token) {
                new_deps.insert(
                    token.clone(),
                    TokenPriceEntry { price: price.clone(), path_components: components.clone() },
                );
                result.insert(token.clone(), price.clone());
            } else {
                result.remove(token);
                new_deps.remove(token);
            }
        }

        store
            .write()
            .await
            .set_token_prices_deps(new_deps, block);
        Span::current().record("updated_token_prices", result.len());

        Ok(Some(result))
    }
}

#[async_trait]
impl DerivedComputation for TokenGasPriceComputation {
    type Output = TokenGasPrices;

    const ID: ComputationId = "token_prices";

    #[instrument(level = "debug", skip(market, store, changed), fields(computation_id = Self::ID, updated_token_prices))]
    async fn compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Self::Output, ComputationError> {
        // For topology changes or full recompute, do a full computation
        // For state-only changes, use incremental computation
        if !changed.is_full_recompute && !changed.is_topology_change() {
            if let Some(result) = self
                .try_incremental_compute(market, store, changed)
                .await?
            {
                return Ok(result);
            }
        }

        let market = market.read().await;

        let block = market
            .last_updated()
            .map(|b| b.number())
            .unwrap_or(0);

        let gas_price = market
            .gas_price()
            .ok_or(ComputationError::MissingDependency("gas_price"))?
            .effective_gas_price();

        let best_prices = self.simulate_token_prices(&market, &gas_price, None)?;

        // Build token prices with dependencies for incremental computation
        let mut token_prices_with_deps = TokenPricesWithDeps::new();
        let mut token_prices = TokenGasPrices::new();

        for (token, (_, price, path_components)) in best_prices {
            token_prices_with_deps
                .insert(token.clone(), TokenPriceEntry { price: price.clone(), path_components });
            token_prices.insert(token, price);
        }

        // Add the gas token itself with price 1:1 (no path dependencies since it's the root)
        let gas_token_price = Price {
            numerator: self.simulation_amount.clone(),
            denominator: self.simulation_amount.clone(),
        };
        token_prices_with_deps.insert(
            self.gas_token.clone(),
            TokenPriceEntry { price: gas_token_price.clone(), path_components: HashSet::new() },
        );
        token_prices.insert(self.gas_token.clone(), gas_token_price);

        store
            .write()
            .await
            .set_token_prices_deps(token_prices_with_deps, block);

        debug!(priced = token_prices.len() - 1, "token price computation complete");

        Span::current().record("updated_token_prices", token_prices.len());

        Ok(token_prices)
    }
}

#[cfg(test)]
mod tests {
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, market_read, setup_market, token, MockProtocolSim},
        derived::store::DerivedData,
    };
    // ==================== Constants ====================

    /// Standard simulation amount: 1 ETH = 10^18 wei.
    const SIM_AMOUNT: u128 = 1_000_000_000_000_000_000;

    /// Gas price set by setup_market: 100 wei/gas.
    const GAS_PRICE: u64 = 100;

    // ==================== Test Helpers ====================

    /// Sets up a complete test environment: market with pools.
    /// Returns (market_guard, store) ready for computation.
    async fn setup_test_env(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (SharedMarketDataRef, SharedDerivedDataRef) {
        let (wrapped_market, _) = setup_market(pools);
        let wrapped_store = DerivedData::new_shared();
        (wrapped_market, wrapped_store)
    }

    /// Creates a computation configured for the given gas token with standard settings.
    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 2, BigUint::from(SIM_AMOUNT))
    }

    // ==================== compute_spread_and_mid_price tests ====================

    #[tokio::test]
    async fn test_compute_spread_and_mid_price_with_gas_and_fee() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Non-trivial setup: 10% fee + significant gas (10% of sim_amount)
        // gas_units = 1e15, gas_cost = 1e15 * 100 = 1e17 (10% of 1e18)
        //
        // Forward (ETH->USDC):
        //   buy_out = 1e18 * 2000 * 0.9 = 1.8e21
        //   buy_gas_cost = 1e17
        //
        // Reverse (USDC->ETH):
        //   sell_out = 1.8e21 / 2000 * 0.9 = 8.1e17
        //   sell_gas_cost = 1e17
        //
        // buy_price = buy_out / (sim_amount + buy_gas_cost)
        //           = 1.8e21 / (1e18 + 1e17) = 1.8e21 / 1.1e18 = 18000/11
        //
        // sell_price = buy_out / (sell_out - sell_gas_cost)
        //            = 1.8e21 / (8.1e17 - 1e17) = 1.8e21 / 7.1e17 = 180000/71
        //
        // spread = |sell_price - buy_price| = 180000/71 - 18000/11
        // mid_price = (buy_price + sell_price) / 2
        let gas_units: u64 = 1_000_000_000_000_000; // 1e15
        let (market, _) = setup_test_env(vec![(
            "pool",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0)
                .with_gas(gas_units)
                .with_fee(0.1),
        )])
        .await;
        let market = market_read(&market);

        // Build graph and run BF forward pass
        let mut graph_manager = PetgraphStableDiGraphManager::new();
        graph_manager.initialize_graph(&market.component_topology());
        let graph = graph_manager.graph();
        let source = graph_manager
            .find_node(&eth.address)
            .unwrap();

        let spfa_result = solve_one_to_all(source, BigUint::from(SIM_AMOUNT), 2, graph, &market);

        let dest = graph_manager
            .find_node(&usdc.address)
            .unwrap();
        let forward_path = spfa_result
            .reconstruct_path(dest)
            .unwrap();

        let gas_price = BigUint::from(GAS_PRICE);
        let computation = computation_for(&eth.address);
        let (spread, mid_price, _path_components) = computation
            .compute_spread_and_mid_price(&forward_path, &market, &gas_price, &spfa_result)
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

        let spot_price: f64 = 2000.0;
        let gas_units: u64 = 50_000;

        let (market, derived) = setup_test_env(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price).with_gas(gas_units),
        )])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
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
    async fn test_compute_selects_best_path_by_output() {
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
        // BF picks by highest forward output:
        //
        // Path via A: 1e18 * 2 * 0.9 * 5 = 9e18  (higher output)
        // Path via B: 1e18 * 3 * 0.95 * 2 = 5.7e18
        //
        // BF selects path via A for C.
        //
        // C via A:
        //   buy_out = 9e18
        //   sell: C->A (9e18/5 = 1.8e18) -> A->ETH (1.8e18*0.9/2 = 0.81e18)
        //   buy_price = 9, sell_price = 9/0.81 = 100/9
        //   mid_price = (81+100)/18 = 181/18
        let eth = token(0, "ETH");
        let a = token(2, "A");
        let b = token(3, "B");
        let c = token(4, "C");

        let (market, derived) = setup_test_env(vec![
            (
                "eth_a",
                &eth,
                &a,
                MockProtocolSim::new(2.0)
                    .with_fee(0.1)
                    .with_gas(0),
            ),
            ("a_c", &a, &c, MockProtocolSim::new(5.0).with_gas(0)),
            (
                "eth_b",
                &eth,
                &b,
                MockProtocolSim::new(3.0)
                    .with_fee(0.05)
                    .with_gas(0),
            ),
            ("b_c", &b, &c, MockProtocolSim::new(2.0).with_gas(0)),
        ])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        assert_eq!(prices.len(), 4, "should have prices for ETH, A, B, C");

        // A: 1-hop from ETH with 10% fee
        // buy_out = 1e18 * 2 * 0.9 = 1.8e18
        // sell_out = 1.8e18 / 2 * 0.9 = 0.81e18
        // buy_price = 9/5, sell_price = (9/5)/(81/100) = 20/9
        // mid_price = (9/5 + 20/9) / 2 = 181/90
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
        // buy_out = 1e18 * 3 * 0.95 = 2.85e18
        // sell_out = 2.85e18 / 3 * 0.95 = 0.9025e18
        // buy_price = 57/20, sell_price = 1140/361
        // mid_price = 43377/14440
        let b_price = prices
            .get(&b.address)
            .expect("B should have price");
        let b_ratio = b_price.numerator.to_f64().unwrap() / b_price.denominator.to_f64().unwrap();
        let expected_b = 43377.0 / 14440.0;
        assert!(
            (b_ratio - expected_b).abs() < 1e-10,
            "B mid_price should be 43377/14440 = {expected_b}, got {b_ratio}"
        );

        // C: BF selects path via A (higher output: 9e18 > 5.7e18)
        // buy_out = 9e18
        // sell_out = 0.81e18
        // buy_price = 9, sell_price = 100/9
        // mid_price = 181/18
        let c_price = prices
            .get(&c.address)
            .expect("C should have price");
        let c_ratio = c_price.numerator.to_f64().unwrap() / c_price.denominator.to_f64().unwrap();
        let expected_c = 181.0 / 18.0;
        assert!(
            (c_ratio - expected_c).abs() < 1e-10,
            "C mid_price should be 181/18 = {expected_c} (via A, highest output), got {c_ratio}"
        );
    }

    #[tokio::test]
    async fn test_compute_gas_token_with_no_pools_returns_only_self() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let dai = token(2, "DAI");

        // Create a pool that doesn't include ETH (gas token)
        let (market, derived) =
            setup_test_env(vec![("usdc_dai", &usdc, &dai, MockProtocolSim::new(1.0))]).await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
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
        market.update_states([("pool".to_string(), Box::new(MockProtocolSim::new(2000.0)) as _)]);
        market.upsert_tokens([eth.clone(), usdc.clone()]);
        let market = SharedMarketData::new_shared();

        let derived = DerivedData::new_shared();
        let changed = ChangedComponents {
            added: std::collections::HashMap::from([(
                "pool".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };

        let computation = computation_for(&eth.address);
        let result = computation
            .compute(&market, &derived, &changed)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("gas_price"))),
            "should return MissingDependency for gas_price"
        );
    }

    #[tokio::test]
    async fn test_compute_respects_max_hops() {
        let eth = token(0, "ETH");
        let a = token(2, "A");
        let b = token(3, "B");
        let c = token(4, "C");

        let (market, derived) = setup_test_env(vec![
            ("eth_a", &eth, &a, MockProtocolSim::new(2.0)),
            ("a_b", &a, &b, MockProtocolSim::new(2.0)),
            ("b_c", &b, &c, MockProtocolSim::new(2.0)),
        ])
        .await;
        let changed = ChangedComponents::default();

        // max_hops = 2
        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        // A (1 hop) and B (2 hops) should be priced, C (3 hops) should not
        assert!(prices.contains_key(&a.address), "A should be priced (1 hop)");
        assert!(prices.contains_key(&b.address), "B should be priced (2 hops)");
        assert!(!prices.contains_key(&c.address), "C should NOT be priced (3 hops)");
    }

    #[tokio::test]
    async fn test_compute_multiple_pools_same_pair() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Two pools with different spot prices; BF picks higher output
        let (market, derived) = setup_test_env(vec![
            ("pool_low", &eth, &usdc, MockProtocolSim::new(1000.0)),
            ("pool_high", &eth, &usdc, MockProtocolSim::new(2000.0)),
        ])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        // BF picks pool_high (higher output: 2000 > 1000)
        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        let ratio =
            usdc_price.numerator.to_f64().unwrap() / usdc_price.denominator.to_f64().unwrap();
        assert!(
            (ratio - 2000.0).abs() < 1e-6,
            "mid-price should be ~2000 (via pool_high), got {ratio}"
        );
    }
}
