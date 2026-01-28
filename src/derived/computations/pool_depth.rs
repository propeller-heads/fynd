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

use async_trait::async_trait;
use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::{One, ToPrimitive, Zero};
use tracing::{instrument, Span};
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
        manager::SharedDerivedDataRef,
        types::{PoolDepths, SpotPriceKey},
    },
    feed::market_data::SharedMarketDataRef,
    types::ComponentId,
};

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
    /// Finds the largest amount where `effective_price >= spot_price * (1 - threshold)`.
    /// Uses `get_limits()` for the upper bound and assumes that it returns a simulatable input
    /// amount.
    ///
    /// As we never exceed the upper bound, we assume that if the simulation errors, it's because
    /// we are below the lower bound of valid amounts, and thus should increase the lower bound.
    /// This assumes that the simulation should not have errors in the valid range.
    ///
    /// # Behavior
    /// - Simulation errors indicate we're outside valid range → adjust bounds accordingly
    /// - Conversion errors (to_f64) are unexpected → terminate with error
    fn find_depth_binary_search(
        &self,
        sim_state: &dyn ProtocolSim,
        token_in: &Token,
        token_out: &Token,
        min_price: f64,
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

        let mut low = BigUint::one();
        let mut high = max_input.clone();
        let mut best_valid = None;

        while low < high {
            let mid = (&low + &high) / 2u32;

            match sim_state.get_amount_out(mid.clone(), token_in, token_out) {
                Ok(result) => {
                    let amount_out = result.amount.to_f64().ok_or_else(|| {
                        ComputationError::Internal("amount_out to_f64 overflow".into())
                    })?;
                    let amount_in = mid.to_f64().ok_or_else(|| {
                        ComputationError::Internal("amount_in to_f64 overflow".into())
                    })?;

                    let effective_price = amount_out / amount_in;

                    if effective_price >= min_price {
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

#[async_trait]
impl DerivedComputation for PoolDepthComputation {
    type Output = PoolDepths;

    const ID: ComputationId = "pool_depths";

    #[instrument(level = "debug", skip(market, store), fields(computation_id = Self::ID, updated_pool_depths))]
    async fn compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
    ) -> Result<Self::Output, ComputationError> {
        let market = market.read().await;
        let store = store.read().await;

        // Get precomputed spot prices (required dependency)
        let spot_prices = store
            .spot_prices()
            .ok_or(ComputationError::MissingDependency(SpotPriceComputation::ID))?;

        let mut pool_depths = PoolDepths::new();

        let topology = market.component_topology();
        let tokens = market.token_registry_ref();

        for (component_id, token_addresses) in topology.iter() {
            let sim_state = market
                .get_simulation_state(component_id)
                .ok_or(ComputationError::InvalidDependencyData {
                    dependency: "market_data::simulation_states",
                    reason: format!("missing simulation state for {component_id}"),
                })?;

            let pool_tokens: Vec<_> = token_addresses
                .iter()
                .map(|addr| {
                    tokens
                        .get(addr)
                        .ok_or_else(|| ComputationError::InvalidDependencyData {
                            dependency: "market_data::tokens",
                            reason: format!(
                                "missing token metadata for {addr} in pool {component_id}"
                            ),
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;

            for perm in pool_tokens.iter().permutations(2) {
                let (token_in, token_out) = (*perm[0], *perm[1]);

                // Look up precomputed spot price
                let spot_price_key: SpotPriceKey =
                    (component_id.clone(), token_in.address.clone(), token_out.address.clone());
                let spot_price = spot_prices
                    .get(&spot_price_key)
                    .ok_or_else(|| ComputationError::InvalidDependencyData {
                        dependency: SpotPriceComputation::ID,
                        reason: format!(
                            "missing spot price for pool {} {}/{}",
                            component_id, token_in.address, token_out.address
                        ),
                    })?;

                // Calculate minimum acceptable price at slippage threshold
                let min_price = spot_price * (1.0 - self.slippage_threshold);

                // Convert the f64 price to a BigUint / BigUint price representation by scaling
                const SCALE: u128 = 10u128.pow(18);
                let min_price_scaled = (min_price * SCALE as f64) as u128;

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

                // Try query_pool_swap first, fall back to binary search if not supported
                let pool_depth = match sim_state.query_pool_swap(&params) {
                    Ok(swap) => swap.amount_in().clone(),
                    Err(SimulationError::FatalError(msg))
                        if msg == "query_pool_swap not implemented" =>
                    {
                        self.find_depth_binary_search(
                            sim_state,
                            token_in,
                            token_out,
                            min_price,
                            component_id,
                        )?
                    }
                    Err(SimulationError::InvalidInput(msg, _))
                        if msg.contains("does not support TradeLimitPrice") =>
                    {
                        // Pool doesn't support TradeLimitPrice constraint, use binary search
                        self.find_depth_binary_search(
                            sim_state,
                            token_in,
                            token_out,
                            min_price,
                            component_id,
                        )?
                    }
                    Err(e) => {
                        return Err(ComputationError::SimulationFailed(format!(
                            "query_pool_swap failed for {}/{}: {e}",
                            token_in.address, token_out.address
                        )));
                    }
                };

                let key =
                    (component_id.clone(), token_in.address.clone(), token_out.address.clone());
                pool_depths.insert(key, pool_depth);
            }
        }

        Span::current().record("updated_pool_depths", pool_depths.len());

        Ok(pool_depths)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::{
        algorithm::test_utils::{setup_market, token, MockProtocolSim},
        derived::manager::wrap_derived,
        feed::market_data::{wrap_market, SharedMarketData},
        DerivedData, PoolDepthKey, SpotPrices,
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
        let market = wrap_market(SharedMarketData::new());
        let derived = wrap_derived(DerivedData::new());
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(SpotPrices::new(), 0);

        let output = PoolDepthComputation::default()
            .compute(&market, &derived)
            .await
            .unwrap();

        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn test_compute_missing_spot_prices_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, _) = setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000))]);
        let derived = wrap_derived(DerivedData::new()); // No spot prices

        let result = PoolDepthComputation::default()
            .compute(&market, &derived)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("spot_prices"))),
            "should return MissingDependency for spot_prices, got {result:?}"
        );
    }

    /// MockProtocolSim has constant price (no slippage), so depth equals sell_limit - 1.
    #[rstest]
    #[case::normal(2, 1_000_000, 499_999)]
    #[case::zero_for_zero_liquidity(2, 0, 0)]
    fn test_binary_search_finds_exact_depth_for_constant_price_pool(
        #[case] spot_price: u32,
        #[case] liquidity: u128,
        #[case] expected_depth: u64,
    ) {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let sim = MockProtocolSim::new(spot_price).with_liquidity(liquidity);
        let comp = PoolDepthComputation::default();
        let spot = sim
            .spot_price(&token_a, &token_b)
            .unwrap();
        let min_price = spot * (1.0 - comp.slippage_threshold);

        let depth = comp
            .find_depth_binary_search(&sim, &token_a, &token_b, min_price, &"mock_pool".into())
            .unwrap();

        assert_eq!(
            depth,
            BigUint::from(expected_depth),
            "expected depth {expected_depth} for spot_price={spot_price}, liquidity={liquidity}"
        );
    }

    #[tokio::test]
    async fn test_compute_integration() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Use spot_price=1 for symmetric behavior in both directions
        let (market, _) = setup_market(vec![(
            "pool",
            &eth,
            &usdc,
            MockProtocolSim::new(1).with_liquidity(1_000_000),
        )]);
        let derived = wrap_derived(DerivedData::new());
        let spot_comp = SpotPriceComputation::new();
        let spot_prices = spot_comp
            .compute(&market, &derived)
            .await
            .expect("spot price computation should succeed");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_prices, 0);

        let pool_depths = PoolDepthComputation::default()
            .compute(&market, &derived)
            .await
            .expect("computation should succeed");

        // Should have depths for both directions: ETH→USDC and USDC→ETH
        assert_eq!(pool_depths.len(), 2, "should have depths for both directions");

        let key_eth_usdc: PoolDepthKey = ("pool".into(), eth.address.clone(), usdc.address.clone());
        let key_usdc_eth: PoolDepthKey = ("pool".into(), usdc.address.clone(), eth.address.clone());

        assert!(pool_depths.contains_key(&key_eth_usdc), "should have depth for ETH→USDC");
        assert!(pool_depths.contains_key(&key_usdc_eth), "should have depth for USDC→ETH");

        // With spot_price=1 and no fee, effective_price = 1.0 for all amounts.
        // min_price = 1.0 * 0.99 = 0.99, so all amounts satisfy effective_price >= min_price.
        // Binary search finds depth = sell_limit - 1 = liquidity - 1 = 999_999.
        let expected_depth = BigUint::from(999_999u64);
        assert_eq!(pool_depths.get(&key_eth_usdc).unwrap(), &expected_depth, "ETH→USDC depth");
        assert_eq!(pool_depths.get(&key_usdc_eth).unwrap(), &expected_depth, "USDC→ETH depth");
    }
}
