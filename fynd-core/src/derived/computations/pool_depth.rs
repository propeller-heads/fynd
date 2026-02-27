//! Pool depth computation.
//!
//! Computes liquidity depths for all pools using `query_pool_swap` when available,
//! falling back to binary search with `get_amount_out`.
//! Depth represents the maximum input amount before reaching the configured slippage
//! threshold from the spot price.
//!
//! # Dependencies
//!
//! This computation depends on [`SpotPrices`](crate::derived::types::SpotPrices) being
//! available in the [`DerivedDataStore`](crate::derived::store::DerivedDataStore).
//! Ensure `SpotPriceComputation` runs before this computation.

use std::collections::HashSet;

use async_trait::async_trait;
use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::{One, Zero};
use rayon::prelude::*;
use tracing::{Span, debug, instrument, warn};
use tycho_simulation::{
    tycho_common::{
        models::token::Token,
        simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
    },
    tycho_core::simulation::protocol_sim::{Price, QueryPoolSwapParams, SwapConstraint},
};

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        computations::spot_price::SpotPriceComputation,
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        types::{PoolDepthKey, PoolDepths, SpotPrices},
    },
    feed::market_data::{SharedMarketData, SharedMarketDataRef},
    types::ComponentId,
};

const PARALLEL_THRESHOLD: usize = 500;

/// Computes pool depths for all pools in all directions.
///
/// For each pool and token pair, uses binary search to find the maximum input
/// amount that results in at most the configured slippage from spot price.
#[derive(Debug)]
pub struct PoolDepthComputation {
    slippage_threshold: f64,
}

impl Default for PoolDepthComputation {
    fn default() -> Self {
        Self { slippage_threshold: 0.01 }
    }
}

impl PoolDepthComputation {
    /// Creates a new PoolDepthComputation with the given slippage threshold.
    ///
    /// # Arguments
    /// * `slippage_threshold` - Value between 0 and 1 exclusive (e.g., 0.01 for 1%)
    ///
    /// # Errors
    /// Returns `InvalidConfiguration` if slippage_threshold is not in (0, 1).
    pub fn new(slippage_threshold: f64) -> Result<Self, ComputationError> {
        if !(slippage_threshold > 0.0 && slippage_threshold < 1.0) {
            return Err(ComputationError::InvalidConfiguration(format!(
                "slippage_threshold must be between 0 and 1 exclusive, got {slippage_threshold}"
            )));
        }
        Ok(Self { slippage_threshold })
    }

    /// Binary search to find the maximum amount_in with acceptable slippage.
    ///
    /// Measures price impact by comparing the post-swap spot price against the initial spot price.
    /// Both prices are on the same basis (buy-side), avoiding fee mismatches that occur when
    /// comparing effective price (amount_out/amount_in) against spot price.
    ///
    /// Uses `get_limits()` for the upper bound and assumes that it returns a simulatable input
    /// amount.
    ///
    /// As we never exceed the upper bound, we assume that if the simulation errors, it's because
    /// we are below the lower bound of valid amounts, and thus should increase the lower bound.
    /// This assumes that the simulation should not have errors in the valid range.
    ///
    /// # Behavior
    /// - Simulation errors indicate we're outside valid range → adjust bounds accordingly
    /// - Spot price errors are propagated as `SimulationFailed`
    fn find_depth_binary_search(
        &self,
        sim_state: &dyn ProtocolSim,
        token_in: &Token,
        token_out: &Token,
        component_id: &ComponentId,
    ) -> Result<BigUint, ComputationError> {
        let (max_input, _) = sim_state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .map_err(|e| {
                ComputationError::SimulationFailed(format!(
                    "get_limits failed for pool {component_id} {}/{}: {e}",
                    token_in.address, token_out.address
                ))
            })?;

        if max_input.is_zero() {
            return Ok(BigUint::zero());
        }

        let initial_price = sim_state
            .spot_price(token_in, token_out)
            .map_err(|e| {
                ComputationError::SimulationFailed(format!(
                    "spot_price failed for pool {component_id} {}/{}: {e}",
                    token_in.address, token_out.address
                ))
            })?;

        // Check if the limit itself doesn't exceed slippage — if so, depth is the limit
        if let Ok(result) = sim_state.get_amount_out(max_input.clone(), token_in, token_out) {
            if let Ok(new_price) = result
                .new_state
                .spot_price(token_in, token_out)
            {
                let price_impact = ((new_price - initial_price) / initial_price).abs();
                if price_impact <= self.slippage_threshold {
                    return Ok(max_input);
                }
            }
        }

        let mut low = BigUint::one();
        let mut high = max_input;
        let mut best_valid = None;

        while low < high {
            let mid = (&low + &high) / 2u32;

            match sim_state.get_amount_out(mid.clone(), token_in, token_out) {
                Ok(result) => {
                    let new_price = result
                        .new_state
                        .spot_price(token_in, token_out)
                        .map_err(|e| {
                            ComputationError::SimulationFailed(format!(
                                "post-swap spot_price failed for pool {component_id} {}/{}: \
                                     {e}",
                                token_in.address, token_out.address
                            ))
                        })?;
                    let price_impact = ((new_price - initial_price) / initial_price).abs();

                    if price_impact <= self.slippage_threshold {
                        best_valid = Some(mid.clone());
                        low = mid + BigUint::one();
                    } else {
                        high = mid;
                    }
                }
                Err(_) => {
                    low = mid + BigUint::one();
                }
            }
        }

        best_valid.ok_or(ComputationError::NoValidResult {
            reason: format!(
                "could not find valid depth for pool {component_id} {}/{}",
                token_in.address, token_out.address
            ),
        })
    }
}

enum DepthRemoval {
    Component(ComponentId),
    Key(PoolDepthKey),
}

struct ComponentDepthResult {
    depths: Vec<(PoolDepthKey, BigUint)>,
    removals: Vec<DepthRemoval>,
    succeeded: usize,
    failed: usize,
}

#[async_trait]
impl DerivedComputation for PoolDepthComputation {
    type Output = PoolDepths;

    const ID: ComputationId = "pool_depths";

    #[instrument(level = "debug", skip(market, store, changed), fields(computation_id = Self::ID, updated_pool_depths))]
    async fn compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Self::Output, ComputationError> {
        // Fetch all data needed for the computation under short-lived locks, then drop guards.
        let (snapshot, spot_prices, mut pool_depths, components_to_compute) = {
            let market_guard = market.read().await;
            let store_guard = store.read().await;

            // Get precomputed spot prices (required dependency)
            let spot_prices = store_guard
                .spot_prices()
                .ok_or(ComputationError::MissingDependency(SpotPriceComputation::ID))?
                .clone();

            // Start with existing depths (or empty for full recompute)
            let mut pool_depths = if changed.is_full_recompute {
                PoolDepths::new()
            } else {
                store_guard
                    .pool_depths()
                    .cloned()
                    .unwrap_or_default()
            };

            // Remove pool depths for removed components
            for component_id in &changed.removed {
                pool_depths.retain(|key, _| &key.0 != component_id);
            }

            let topology = market_guard.component_topology();

            // Determine which components to compute
            let components_to_compute: Vec<ComponentId> = if changed.is_full_recompute {
                topology.keys().cloned().collect()
            } else {
                changed
                    .added
                    .keys()
                    .chain(changed.updated.iter())
                    .cloned()
                    .collect()
            };

            let component_ids: HashSet<ComponentId> = components_to_compute
                .iter()
                .cloned()
                .collect();
            let snapshot: SharedMarketData = market_guard.extract_subset(&component_ids);

            (snapshot, spot_prices, pool_depths, components_to_compute)
        };

        let results: Vec<ComponentDepthResult> =
            if components_to_compute.len() >= PARALLEL_THRESHOLD {
                components_to_compute
                    .par_iter()
                    .map(|id| {
                        compute_component_depths(
                            id,
                            changed,
                            &snapshot,
                            &spot_prices,
                            self.slippage_threshold,
                        )
                    })
                    .collect()
            } else {
                components_to_compute
                    .iter()
                    .map(|id| {
                        compute_component_depths(
                            id,
                            changed,
                            &snapshot,
                            &spot_prices,
                            self.slippage_threshold,
                        )
                    })
                    .collect()
            };

        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for component_result in results {
            for removal in &component_result.removals {
                match removal {
                    DepthRemoval::Component(id) => {
                        pool_depths.retain(|key, _| &key.0 != id);
                    }
                    DepthRemoval::Key(key) => {
                        pool_depths.remove(key);
                    }
                }
            }
            for (key, depth) in component_result.depths {
                pool_depths.insert(key, depth);
            }
            succeeded += component_result.succeeded;
            failed += component_result.failed;
        }

        debug!(succeeded, failed, total = pool_depths.len(), "pool depth computation complete");
        Span::current().record("updated_pool_depths", pool_depths.len());

        Ok(pool_depths)
    }
}

fn compute_component_depths(
    component_id: &ComponentId,
    changed: &ChangedComponents,
    snapshot: &SharedMarketData,
    spot_prices: &SpotPrices,
    slippage_threshold: f64,
) -> ComponentDepthResult {
    let topology = snapshot.component_topology();
    let tokens = snapshot.token_registry_ref();
    let comp = PoolDepthComputation { slippage_threshold };

    let mut result =
        ComponentDepthResult { depths: Vec::new(), removals: Vec::new(), succeeded: 0, failed: 0 };

    let token_addresses = changed
        .added
        .get(component_id)
        .or_else(|| topology.get(component_id));

    let Some(token_addresses) = token_addresses else {
        return result;
    };

    let Some(sim_state) = snapshot.get_simulation_state(component_id) else {
        warn!(component_id, "missing simulation state, skipping pool");
        result
            .removals
            .push(DepthRemoval::Component(component_id.clone()));
        return result;
    };

    let pool_tokens: Result<Vec<_>, _> = token_addresses
        .iter()
        .map(|addr| tokens.get(addr).ok_or(addr))
        .collect();
    let Ok(pool_tokens) = pool_tokens else {
        warn!(component_id, "missing token metadata, skipping pool");
        result
            .removals
            .push(DepthRemoval::Component(component_id.clone()));
        return result;
    };

    for perm in pool_tokens.iter().permutations(2) {
        let (token_in, token_out) = (*perm[0], *perm[1]);
        let key = (component_id.clone(), token_in.address.clone(), token_out.address.clone());

        let Some(spot_price) = spot_prices.get(&key) else {
            warn!(
                component_id,
                token_in = %token_in.address,
                token_out = %token_out.address,
                "missing spot price, skipping pair"
            );
            result
                .removals
                .push(DepthRemoval::Key(key));
            result.failed += 1;
            continue;
        };

        let min_price = spot_price * (1.0 - slippage_threshold);

        const SCALE: u128 = 10u128.pow(18);
        let min_price_scaled = (min_price * SCALE as f64) as u128;

        if min_price_scaled == 0 {
            warn!(
                component_id,
                token_in = %token_in.address,
                token_out = %token_out.address,
                spot_price,
                "spot price too small to compute depth, skipping pair"
            );
            result
                .removals
                .push(DepthRemoval::Key(key));
            result.failed += 1;
            continue;
        }

        let limit_price = Price::new(BigUint::from(min_price_scaled), BigUint::from(SCALE));

        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: limit_price,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let depth_result = match sim_state.query_pool_swap(&params) {
            Ok(swap) => Ok(swap.amount_in().clone()),
            Err(SimulationError::FatalError(msg)) if msg == "query_pool_swap not implemented" => {
                comp.find_depth_binary_search(sim_state, token_in, token_out, component_id)
            }
            Err(SimulationError::InvalidInput(msg, _))
                if msg.contains("does not support TradeLimitPrice") =>
            {
                comp.find_depth_binary_search(sim_state, token_in, token_out, component_id)
            }
            Err(e) => Err(ComputationError::SimulationFailed(format!(
                "query_pool_swap failed for {}/{}: {e}",
                token_in.address, token_out.address
            ))),
        };

        match depth_result {
            Ok(depth) => {
                result.depths.push((key, depth));
                result.succeeded += 1;
            }
            Err(e) => {
                let probe_info = sim_state
                    .get_amount_out(BigUint::one(), token_in, token_out)
                    .map(|r| format!("amount_out={}", r.amount))
                    .unwrap_or_else(|e| format!("sim_error={e}"));
                let limits_info = sim_state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .map(|(max_in, max_out)| format!("max_in={max_in}, max_out={max_out}"))
                    .unwrap_or_else(|e| format!("limits_error={e}"));
                debug!(
                    component_id,
                    token_in = %token_in.address,
                    token_out = %token_out.address,
                    spot_price,
                    min_price,
                    probe_info,
                    limits_info,
                    error = %e,
                    "pool depth failed, skipping pair"
                );
                result
                    .removals
                    .push(DepthRemoval::Key(key));
                result.failed += 1;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{
        algorithm::test_utils::{MockProtocolSim, setup_market, token},
        derived::{
            store::DerivedData,
            types::{PoolDepthKey, SpotPrices},
        },
        feed::market_data::SharedMarketData,
    };

    #[test]
    fn computation_id() {
        assert_eq!(PoolDepthComputation::ID, "pool_depths");
    }

    #[test]
    fn default_slippage_is_one_percent() {
        let comp = PoolDepthComputation::default();
        assert!((comp.slippage_threshold - 0.01).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case(0.001)]
    #[case(0.01)]
    #[case(0.5)]
    #[case(0.99)]
    fn new_with_valid_slippage(#[case] threshold: f64) {
        let comp = PoolDepthComputation::new(threshold).unwrap();
        assert!((comp.slippage_threshold - threshold).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case(0.0, "zero")]
    #[case(1.0, "one")]
    #[case(-0.1, "negative")]
    #[case(1.5, "greater than one")]
    #[case(f64::NAN, "NaN")]
    #[case(f64::INFINITY, "infinity")]
    fn new_with_invalid_slippage(#[case] threshold: f64, #[case] _desc: &str) {
        let result = PoolDepthComputation::new(threshold);
        assert!(
            matches!(result, Err(ComputationError::InvalidConfiguration(_))),
            "expected InvalidConfiguration for {_desc}, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_compute_handles_empty_market() {
        let market = SharedMarketData::new_shared();
        let derived = DerivedData::new_shared();
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(SpotPrices::new(), 0);
        let changed = ChangedComponents::default();

        let output = PoolDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn test_compute_missing_spot_prices_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, _) = setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000))]);
        let derived = DerivedData::new_shared(); // No spot prices
        let changed = ChangedComponents::default();

        let result = PoolDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("spot_prices"))),
            "should return MissingDependency for spot_prices, got {result:?}"
        );
    }

    /// MockProtocolSim increments spot_price by 1 on each swap.
    /// With spot_price=100 (no fee), price impact = 1/100 = 1% which equals the default
    /// threshold, so the limit itself passes → depth = sell_limit.
    /// With zero liquidity, depth is zero.
    #[rstest]
    #[case::within_threshold(100, 1_000_000, 10_000)]
    #[case::zero_for_zero_liquidity(100, 0, 0)]
    fn test_binary_search_finds_depth_within_threshold(
        #[case] spot_price: u32,
        #[case] liquidity: u128,
        #[case] expected_depth: u64,
    ) {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let sim = MockProtocolSim::new(spot_price).with_liquidity(liquidity);
        let comp = PoolDepthComputation::default();

        let depth = comp
            .find_depth_binary_search(&sim, &token_a, &token_b, &"mock_pool".into())
            .unwrap();

        assert_eq!(
            depth,
            BigUint::from(expected_depth),
            "expected depth {expected_depth} for spot_price={spot_price}, liquidity={liquidity}"
        );
    }

    /// When price impact always exceeds the threshold (spot_price=1, impact=100%),
    /// binary search returns NoValidResult.
    #[test]
    fn test_binary_search_returns_error_when_all_amounts_exceed_threshold() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let sim = MockProtocolSim::new(1).with_liquidity(1_000_000);
        let comp = PoolDepthComputation::default();

        let result = comp.find_depth_binary_search(&sim, &token_a, &token_b, &"mock_pool".into());

        assert!(
            matches!(result, Err(ComputationError::NoValidResult { .. })),
            "expected NoValidResult when price impact always exceeds threshold, got {result:?}"
        );
    }

    /// With a higher slippage threshold (50%), spot_price=1 (impact=100%) still fails,
    /// but spot_price=2 (impact=50%) passes.
    #[test]
    fn test_binary_search_respects_custom_slippage_threshold() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let comp = PoolDepthComputation::new(0.5).unwrap();

        // spot_price=2: new_state has spot_price=3, impact = |1/3 - 1/2| / (1/2) = 1/3 ≈ 33% <= 50%
        let sim = MockProtocolSim::new(2).with_liquidity(1_000_000);
        let depth = comp
            .find_depth_binary_search(&sim, &token_a, &token_b, &"mock_pool".into())
            .unwrap();

        // sell_limit = liquidity / spot_price = 1_000_000 / 2 = 500_000
        assert_eq!(depth, BigUint::from(500_000u64));
    }

    /// Verify that the binary search uses spot price impact (not effective price).
    /// With a fee, effective price differs from spot price, but the binary search
    /// should only consider spot price changes.
    #[test]
    fn test_binary_search_uses_spot_price_not_effective_price() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // spot_price=100, fee=1%. The mock's spot_price() includes fee markup: raw/(1-fee).
        // After swap: new spot_price=101. Price impact based on spot prices:
        // initial = 1/(100/0.99), new = 1/(101/0.99) → impact = |new-initial|/initial = 1/100 = 1%
        // With default threshold 1%, this should pass (impact <= threshold).
        let sim = MockProtocolSim::new(100)
            .with_liquidity(1_000_000)
            .with_fee(0.01);
        let comp = PoolDepthComputation::default();

        let depth = comp
            .find_depth_binary_search(&sim, &token_a, &token_b, &"mock_pool".into())
            .unwrap();

        // Should find a valid depth (the limit itself passes)
        assert!(depth > BigUint::zero(), "should find valid depth for high-fee pool");
    }

    #[tokio::test]
    async fn test_compute_integration() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Use spot_price=100 so price impact (1/100 = 1%) equals the default threshold.
        // The mock increments spot_price by 1 on each swap, so new_state.spot_price=101.
        let (market, _) = setup_market(vec![(
            "pool",
            &eth,
            &usdc,
            MockProtocolSim::new(100).with_liquidity(1_000_000),
        )]);
        let derived = DerivedData::new_shared();
        let spot_comp = SpotPriceComputation::new();
        let changed = ChangedComponents {
            added: std::collections::HashMap::from([(
                "pool".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };
        let spot_prices = spot_comp
            .compute(&market, &derived, &changed)
            .await
            .expect("spot price computation should succeed");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_prices, 0);

        let pool_depths = PoolDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("computation should succeed");

        // Should have depths for both directions: ETH→USDC and USDC→ETH
        assert_eq!(pool_depths.len(), 2, "should have depths for both directions");

        let key_eth_usdc: PoolDepthKey = ("pool".into(), eth.address.clone(), usdc.address.clone());
        let key_usdc_eth: PoolDepthKey = ("pool".into(), usdc.address.clone(), eth.address.clone());

        assert!(pool_depths.contains_key(&key_eth_usdc), "should have depth for ETH→USDC");
        assert!(pool_depths.contains_key(&key_usdc_eth), "should have depth for USDC→ETH");

        // With spot_price=100, price impact = 1% which equals threshold → limit passes.
        // sell_limit = liquidity / spot_price = 1_000_000 / 100 = 10_000
        let expected_depth = BigUint::from(10_000u64);
        assert_eq!(pool_depths.get(&key_eth_usdc).unwrap(), &expected_depth, "ETH→USDC depth");
        // For USDC→ETH (addr 0x01 > addr 0x00): sell_limit = liquidity * spot_price (inverted)
        // Actually get_limits is direction-agnostic in the mock, so same sell_limit
        assert_eq!(pool_depths.get(&key_usdc_eth).unwrap(), &expected_depth, "USDC→ETH depth");
    }

    #[tokio::test]
    async fn test_compute_parallel_path() {
        let num_pools = PARALLEL_THRESHOLD + 10;
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let pool_names: Vec<String> = (0..num_pools)
            .map(|i| format!("pool_{i}"))
            .collect();
        let pool_tuples: Vec<(&str, &Token, &Token, MockProtocolSim)> = pool_names
            .iter()
            .map(|name| {
                (name.as_str(), &eth, &usdc, MockProtocolSim::new(100).with_liquidity(1_000_000))
            })
            .collect();

        let (market, _) = setup_market(pool_tuples);
        let derived = DerivedData::new_shared();
        let spot_comp = SpotPriceComputation::new();

        let added: std::collections::HashMap<String, Vec<_>> = pool_names
            .iter()
            .map(|name| (name.clone(), vec![eth.address.clone(), usdc.address.clone()]))
            .collect();
        let changed =
            ChangedComponents { added, removed: vec![], updated: vec![], is_full_recompute: true };

        let spot_prices = spot_comp
            .compute(&market, &derived, &changed)
            .await
            .expect("spot price computation should succeed");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_prices, 0);

        let pool_depths = PoolDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("parallel computation should succeed");

        // Each pool has 2 directions (ETH→USDC and USDC→ETH)
        assert_eq!(
            pool_depths.len(),
            num_pools * 2,
            "should have depths for all pools in both directions"
        );
    }
}
