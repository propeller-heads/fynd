//! Pool depth computation.
//!
//! Computes liquidity depths for all pools using `query_pool_swap`.
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
use tracing::{debug, instrument, warn, Span};
use tycho_simulation::{
    evm::query_pool_swap::query_pool_swap,
    tycho_common::simulation::errors::SimulationError,
    tycho_core::simulation::protocol_sim::{Price, QueryPoolSwapParams, SwapConstraint},
};

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        computations::spot_price::SpotPriceComputation,
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        types::PoolDepths,
    },
    feed::market_data::{SharedMarketData, SharedMarketDataRef},
    types::ComponentId,
};

/// Computes pool depths for all pools in all directions.
///
/// For each pool and token pair, finds the maximum input amount before reaching
/// the configured slippage threshold using `query_pool_swap`.
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

        let topology = snapshot.component_topology();
        let tokens = snapshot.token_registry_ref();

        let mut succeeded = 0usize;
        let mut failed = 0usize;

        for component_id in &components_to_compute {
            // Get token addresses: changed.added for new components, topology for existing
            let token_addresses = changed
                .added
                .get(component_id)
                .or_else(|| topology.get(component_id));

            let Some(token_addresses) = token_addresses else {
                continue; // Component might have been removed in the meantime
            };

            let Some(sim_state) = snapshot.get_simulation_state(component_id) else {
                warn!(component_id, "missing simulation state, skipping pool");
                pool_depths.retain(|key, _| &key.0 != component_id);
                continue;
            };

            let pool_tokens: Result<Vec<_>, _> = token_addresses
                .iter()
                .map(|addr| tokens.get(addr).ok_or(addr))
                .collect();
            let Ok(pool_tokens) = pool_tokens else {
                warn!(component_id, "missing token metadata, skipping pool");
                pool_depths.retain(|key, _| &key.0 != component_id);
                continue;
            };

            for perm in pool_tokens.iter().permutations(2) {
                let (token_in, token_out) = (*perm[0], *perm[1]);
                let key =
                    (component_id.clone(), token_in.address.clone(), token_out.address.clone());

                // Look up precomputed spot price
                let Some(spot_price) = spot_prices.get(&key) else {
                    warn!(
                        component_id,
                        token_in = %token_in.address,
                        token_out = %token_out.address,
                        "missing spot price, skipping pair"
                    );
                    pool_depths.remove(&key);
                    failed += 1;
                    continue;
                };

                // Calculate minimum acceptable price at slippage threshold
                let min_price = spot_price * (1.0 - self.slippage_threshold);

                // Convert the f64 price to a BigUint / BigUint price representation by scaling
                const SCALE: u128 = 10u128.pow(18);
                let min_price_scaled = (min_price * SCALE as f64) as u128;

                // Skip pairs where the scaled price rounds to zero (extremely small spot price)
                if min_price_scaled == 0 {
                    warn!(
                        component_id,
                        token_in = %token_in.address,
                        token_out = %token_out.address,
                        spot_price,
                        "spot price too small to compute depth, skipping pair"
                    );
                    pool_depths.remove(&key);
                    failed += 1;
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

                // Try query_pool_swap first, fall back to generic Brent solver
                // if the protocol doesn't support it.
                let depth_result = match sim_state.query_pool_swap(&params) {
                    Ok(swap) => Ok(swap),
                    Err(SimulationError::FatalError(msg))
                        if msg == "query_pool_swap not implemented" =>
                    {
                        query_pool_swap(sim_state, &params)
                    }
                    Err(SimulationError::InvalidInput(msg, _))
                        if msg.contains("does not support TradeLimitPrice") =>
                    {
                        query_pool_swap(sim_state, &params)
                    }
                    Err(e) => Err(e),
                }
                .map(|swap| swap.amount_in().clone())
                .map_err(|e| {
                    ComputationError::SimulationFailed(format!(
                        "query_pool_swap failed for {}/{}: {e}",
                        token_in.address, token_out.address
                    ))
                });

                match depth_result {
                    Ok(depth) => {
                        pool_depths.insert(key, depth);
                        succeeded += 1;
                    }
                    Err(e) => {
                        // Diagnostic: probe with 1 unit to understand why depth search failed
                        let probe_info = sim_state
                            .get_amount_out(BigUint::from(1u32), token_in, token_out)
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
                        pool_depths.remove(&key);
                        failed += 1;
                    }
                }
            }
        }

        debug!(succeeded, failed, total = pool_depths.len(), "pool depth computation complete");
        Span::current().record("updated_pool_depths", pool_depths.len());

        Ok(pool_depths)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use num_traits::{One, ToPrimitive, Zero};
    use rstest::rstest;
    use tycho_simulation::{
        tycho_common::simulation::protocol_sim::ProtocolSim,
        tycho_core::{
            dto::ProtocolStateDelta,
            models::{token::Token, Chain},
            simulation::{
                errors::TransitionError,
                protocol_sim::{Balances, GetAmountOutResult},
            },
            Bytes,
        },
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{setup_market, token, MockProtocolSim},
        feed::market_data::SharedMarketData,
        DerivedData, PoolDepthKey, SpotPrices,
    };

    // ==================== ConstantProductSim ====================

    /// Minimal constant-product (x*y=k) AMM for testing the Brent fallback path.
    ///
    /// Uses address ordering to map tokens to reserves:
    /// - Token with smaller address → reserve0
    /// - Token with larger address → reserve1
    #[derive(Debug, Clone)]
    struct ConstantProductSim {
        reserve0: u128,
        reserve1: u128,
    }

    impl ConstantProductSim {
        fn new(reserve0: u128, reserve1: u128) -> Self {
            Self { reserve0, reserve1 }
        }

        fn reserves_for_direction(&self, token_in: &Bytes, token_out: &Bytes) -> (u128, u128) {
            if token_in < token_out {
                (self.reserve0, self.reserve1)
            } else {
                (self.reserve1, self.reserve0)
            }
        }
    }

    impl ProtocolSim for ConstantProductSim {
        fn fee(&self) -> f64 {
            0.0
        }

        fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
            let (reserve_base, reserve_quote) =
                self.reserves_for_direction(&base.address, &quote.address);
            if reserve_base == 0 {
                return Err(SimulationError::FatalError("zero reserve".into()));
            }
            Ok(reserve_quote as f64 / reserve_base as f64)
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            let (reserve_in, reserve_out) =
                self.reserves_for_direction(&token_in.address, &token_out.address);

            let amount_in_u128: u128 = amount_in
                .try_into()
                .map_err(|_| SimulationError::FatalError("amount_in overflow".into()))?;

            if amount_in_u128 == 0 {
                return Err(SimulationError::InvalidInput("zero amount_in".into(), None));
            }

            let amount_out = reserve_out
                .checked_mul(amount_in_u128)
                .and_then(|n| n.checked_div(reserve_in.checked_add(amount_in_u128)?))
                .ok_or_else(|| SimulationError::FatalError("arithmetic overflow".into()))?;

            let new_reserve_in = reserve_in + amount_in_u128;
            let new_reserve_out = reserve_out - amount_out;

            let new_state = if token_in.address < token_out.address {
                ConstantProductSim::new(new_reserve_in, new_reserve_out)
            } else {
                ConstantProductSim::new(new_reserve_out, new_reserve_in)
            };

            Ok(GetAmountOutResult::new(
                BigUint::from(amount_out),
                BigUint::from(0u32),
                Box::new(new_state),
            ))
        }

        fn get_limits(
            &self,
            sell_token: Bytes,
            buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            let (reserve_in, reserve_out) = self.reserves_for_direction(&sell_token, &buy_token);
            Ok((BigUint::from(reserve_in), BigUint::from(reserve_out)))
        }

        fn query_pool_swap(
            &self,
            _params: &QueryPoolSwapParams,
        ) -> Result<tycho_simulation::tycho_core::simulation::protocol_sim::PoolSwap, SimulationError>
        {
            Err(SimulationError::FatalError("query_pool_swap not implemented".into()))
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError<String>> {
            unimplemented!()
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .map(|o| o.reserve0 == self.reserve0 && o.reserve1 == self.reserve1)
                .unwrap_or(false)
        }
    }

    // ==================== Legacy binary search (from HEAD~1) ====================

    /// Spot-price-based binary search for pool depth.
    ///
    /// Recovered from the pre-Brent implementation for comparison testing.
    /// Measures price impact via post-swap spot price change, as opposed to the
    /// Brent solver which uses execution price (amount_out / amount_in).
    fn find_depth_binary_search_legacy(
        slippage_threshold: f64,
        sim_state: &dyn ProtocolSim,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<BigUint, ComputationError> {
        let (max_input, _) = sim_state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .map_err(|e| ComputationError::SimulationFailed(format!("get_limits failed: {e}")))?;

        if max_input.is_zero() {
            return Ok(BigUint::zero());
        }

        let initial_price = sim_state
            .spot_price(token_in, token_out)
            .map_err(|e| ComputationError::SimulationFailed(format!("spot_price failed: {e}")))?;

        if let Ok(result) = sim_state.get_amount_out(max_input.clone(), token_in, token_out) {
            if let Ok(new_price) = result
                .new_state
                .spot_price(token_in, token_out)
            {
                let price_impact = ((new_price - initial_price) / initial_price).abs();
                if price_impact <= slippage_threshold {
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
                                "post-swap spot_price failed: {e}"
                            ))
                        })?;
                    let price_impact = ((new_price - initial_price) / initial_price).abs();

                    if price_impact <= slippage_threshold {
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
            reason: "could not find valid depth".to_string(),
        })
    }

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

    // ==================== Brent fallback & comparison tests ====================

    fn cp_token(addr_b: u8, symbol: &str) -> Token {
        Token {
            address: Bytes::from([addr_b; 20].to_vec()),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    #[rstest]
    #[case::balanced(1_000_000, 1_000_000, 0.01)]
    #[case::unbalanced_2000x(1_000_000, 2_000_000_000, 0.01)]
    #[case::unbalanced_inverse(2_000_000_000, 1_000_000, 0.01)]
    #[case::tight_slippage(1_000_000_000, 1_000_000_000, 0.001)]
    #[case::wide_slippage(1_000_000, 1_000_000, 0.05)]
    fn test_brent_fallback_finds_depth(
        #[case] reserve0: u128,
        #[case] reserve1: u128,
        #[case] slippage: f64,
    ) {
        let sim = ConstantProductSim::new(reserve0, reserve1);
        let t0 = cp_token(0x01, "T0");
        let t1 = cp_token(0x02, "T1");

        let spot = sim.spot_price(&t0, &t1).unwrap();
        let min_price = spot * (1.0 - slippage);

        const SCALE: u128 = 10u128.pow(18);
        let min_price_scaled = (min_price * SCALE as f64) as u128;
        let limit_price = Price::new(BigUint::from(min_price_scaled), BigUint::from(SCALE));

        let params = QueryPoolSwapParams::new(
            t0.clone(),
            t1.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: limit_price,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = query_pool_swap(&sim, &params);
        assert!(result.is_ok(), "Brent solver failed: {result:?}");

        let swap = result.unwrap();
        let depth = swap.amount_in();
        assert!(!depth.is_zero(), "Brent depth should be non-zero");

        // Verify the depth satisfies the execution price constraint
        let sim_result = sim
            .get_amount_out(depth.clone(), &t0, &t1)
            .unwrap();
        let exec_price = sim_result.amount.to_f64().unwrap() / depth.to_f64().unwrap();
        assert!(
            exec_price >= min_price * 0.99,
            "execution price {exec_price} should be >= min_price {min_price} (within tolerance)"
        );
    }

    #[rstest]
    #[case::balanced(1_000_000, 1_000_000, 0.01)]
    #[case::unbalanced_2000x(1_000_000, 2_000_000_000, 0.01)]
    #[case::unbalanced_inverse(2_000_000_000, 1_000_000, 0.01)]
    #[case::wide_slippage(10_000_000, 10_000_000, 0.05)]
    fn test_brent_vs_binary_search_depths(
        #[case] reserve0: u128,
        #[case] reserve1: u128,
        #[case] slippage: f64,
    ) {
        let sim = ConstantProductSim::new(reserve0, reserve1);
        let t0 = cp_token(0x01, "T0");
        let t1 = cp_token(0x02, "T1");

        // --- Legacy binary search (spot price impact) ---
        let bs_depth =
            find_depth_binary_search_legacy(slippage, &sim, &t0, &t1).expect("binary search");

        // --- Brent solver (execution price) ---
        let spot = sim.spot_price(&t0, &t1).unwrap();
        let min_price = spot * (1.0 - slippage);
        const SCALE: u128 = 10u128.pow(18);
        let min_price_scaled = (min_price * SCALE as f64) as u128;
        let limit_price = Price::new(BigUint::from(min_price_scaled), BigUint::from(SCALE));

        let params = QueryPoolSwapParams::new(
            t0.clone(),
            t1.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: limit_price,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let brent_depth = query_pool_swap(&sim, &params)
            .expect("Brent solver")
            .amount_in()
            .clone();

        // Both should find non-zero depths
        assert!(!bs_depth.is_zero(), "binary search depth should be non-zero");
        assert!(!brent_depth.is_zero(), "Brent depth should be non-zero");

        // Same order of magnitude: Brent measures execution price (less conservative) so it
        // will typically find a larger depth than binary search (which measures spot price
        // impact). For constant product AMMs, the ratio is roughly 2x at small slippage.
        let bs_f = bs_depth.to_f64().unwrap();
        let brent_f = brent_depth.to_f64().unwrap();
        let ratio = brent_f / bs_f;
        assert!(
            ratio > 0.1 && ratio < 20.0,
            "depths should be same order of magnitude: binary_search={bs_f}, brent={brent_f}, ratio={ratio}"
        );

        // Verify binary search depth satisfies spot price impact constraint
        let bs_result = sim
            .get_amount_out(bs_depth.clone(), &t0, &t1)
            .unwrap();
        let new_spot = bs_result
            .new_state
            .spot_price(&t0, &t1)
            .unwrap();
        let price_impact = ((new_spot - spot) / spot).abs();
        assert!(
            price_impact <= slippage + 1e-9,
            "binary search depth should satisfy spot price constraint: impact={price_impact}, threshold={slippage}"
        );

        // Verify Brent depth satisfies execution price constraint
        let brent_result = sim
            .get_amount_out(brent_depth.clone(), &t0, &t1)
            .unwrap();
        let exec_price = brent_result.amount.to_f64().unwrap() / brent_depth.to_f64().unwrap();
        assert!(
            exec_price >= min_price * 0.99,
            "Brent depth should satisfy execution price constraint: exec={exec_price}, min={min_price}"
        );
    }

    #[tokio::test]
    async fn test_compute_with_brent_fallback() {
        let t0 = cp_token(0x01, "T0");
        let t1 = cp_token(0x02, "T1");
        let sim = ConstantProductSim::new(1_000_000, 2_000_000_000);

        // Build market data manually (can't use setup_market which requires MockProtocolSim)
        let mut market = SharedMarketData::new();
        let comp = crate::algorithm::test_utils::component("cp_pool", &[t0.clone(), t1.clone()]);
        market.upsert_components(std::iter::once(comp));
        market.update_states([("cp_pool".to_string(), Box::new(sim) as Box<dyn ProtocolSim>)]);
        market.upsert_tokens(vec![t0.clone(), t1.clone()]);
        market.update_last_updated(crate::types::BlockInfo {
            number: 1,
            hash: "0x00".into(),
            timestamp: 0,
        });

        let market_ref = std::sync::Arc::new(tokio::sync::RwLock::new(market));
        let derived = DerivedData::new_shared();

        let changed = ChangedComponents {
            added: std::collections::HashMap::from([(
                "cp_pool".to_string(),
                vec![t0.address.clone(), t1.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };

        // Compute spot prices first
        let spot_prices = SpotPriceComputation::new()
            .compute(&market_ref, &derived, &changed)
            .await
            .expect("spot price computation should succeed");
        assert_eq!(spot_prices.len(), 2, "should have spot prices for both directions");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_prices, 0);

        // Compute pool depths (should trigger Brent fallback)
        let pool_depths = PoolDepthComputation::default()
            .compute(&market_ref, &derived, &changed)
            .await
            .expect("pool depth computation should succeed");

        assert_eq!(pool_depths.len(), 2, "should have depths for both directions");

        let key_01: PoolDepthKey = ("cp_pool".into(), t0.address.clone(), t1.address.clone());
        let key_10: PoolDepthKey = ("cp_pool".into(), t1.address.clone(), t0.address.clone());

        let depth_01 = pool_depths
            .get(&key_01)
            .expect("should have T0→T1 depth");
        let depth_10 = pool_depths
            .get(&key_10)
            .expect("should have T1→T0 depth");

        assert!(!depth_01.is_zero(), "T0→T1 depth should be non-zero");
        assert!(!depth_10.is_zero(), "T1→T0 depth should be non-zero");
    }

    #[test]
    #[ignore]
    fn bench_brent_vs_binary_search() {
        use std::time::Instant;

        let configs: Vec<(u128, u128, &str)> = vec![
            (1_000_000, 1_000_000, "balanced 1M:1M"),
            (1_000_000, 2_000_000_000, "unbalanced 1M:2B"),
            (1_000_000_000, 1_000_000_000, "balanced 1B:1B"),
        ];
        let slippages = [0.01, 0.05];
        let iterations = 100;

        let t0 = cp_token(0x01, "T0");
        let t1 = cp_token(0x02, "T1");

        eprintln!("\n{:-<80}", "");
        eprintln!("Brent vs Binary Search Benchmark ({iterations} iterations each)");
        eprintln!("{:-<80}", "");

        for (reserve0, reserve1, label) in &configs {
            for slippage in &slippages {
                let sim = ConstantProductSim::new(*reserve0, *reserve1);

                // Binary search timing
                let start = Instant::now();
                for _ in 0..iterations {
                    let _ = find_depth_binary_search_legacy(*slippage, &sim, &t0, &t1);
                }
                let bs_elapsed = start.elapsed();

                // Brent solver timing
                let spot = sim.spot_price(&t0, &t1).unwrap();
                let min_price = spot * (1.0 - slippage);
                const SCALE: u128 = 10u128.pow(18);
                let min_price_scaled = (min_price * SCALE as f64) as u128;
                let limit_price = Price::new(BigUint::from(min_price_scaled), BigUint::from(SCALE));
                let params = QueryPoolSwapParams::new(
                    t0.clone(),
                    t1.clone(),
                    SwapConstraint::TradeLimitPrice {
                        limit: limit_price,
                        tolerance: 0.0,
                        min_amount_in: None,
                        max_amount_in: None,
                    },
                );

                let start = Instant::now();
                for _ in 0..iterations {
                    let _ = query_pool_swap(&sim, &params);
                }
                let brent_elapsed = start.elapsed();

                let bs_avg = bs_elapsed.as_micros() as f64 / iterations as f64;
                let brent_avg = brent_elapsed.as_micros() as f64 / iterations as f64;
                let speedup = bs_avg / brent_avg;

                eprintln!(
                    "{label} | slippage={slippage:.1%} | BS={bs_avg:.1}µs | Brent={brent_avg:.1}µs | speedup={speedup:.2}x"
                );
            }
        }
        eprintln!("{:-<80}", "");
    }
}
