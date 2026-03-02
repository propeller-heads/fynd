//! Pool depth computation.
//!
//! Computes liquidity depths for all pools using `query_pool_swap`, falling back to
//! the generic Brent solver from tycho-simulation when the pool doesn't implement it natively.
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
use num_traits::Zero;
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
/// For each pool and token pair, uses `query_pool_swap` (with Brent solver fallback)
/// to find the maximum input amount that results in at most the configured slippage
/// from spot price.
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

                let min_price = spot_price * (1.0 - self.slippage_threshold);

                // Price is a raw fraction (numerator/denominator) that query_pool_swap
                // converts back to f64 by multiplying by 10^(dec_in - dec_out). We keep
                // the f64→u128 multiply at a fixed precision scale and absorb the decimal
                // adjustment into the BigUint denominator.
                const SCALE_EXP: i32 = 18;
                let decimal_diff = token_in.decimals as i32 - token_out.decimals as i32;
                let denominator_exp = SCALE_EXP + decimal_diff;
                if denominator_exp < 0 {
                    warn!(
                        component_id,
                        token_in = %token_in.address,
                        token_out = %token_out.address,
                        "extreme decimal mismatch ({}→{}), skipping pair",
                        token_in.decimals, token_out.decimals
                    );
                    pool_depths.remove(&key);
                    failed += 1;
                    continue;
                }

                let numerator = BigUint::from((min_price * 10_f64.powi(SCALE_EXP)) as u128);
                let denominator = BigUint::from(10u64).pow(denominator_exp as u32);

                if numerator.is_zero() {
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

                let limit_price = Price::new(numerator, denominator);

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
    use rstest::rstest;
    use tycho_simulation::{
        tycho_common::simulation::protocol_sim::ProtocolSim, tycho_core::models::token::Token,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{setup_market, token, token_with_decimals, MockProtocolSim},
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

        let (market, _) = setup_market(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000.0))]);
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

    #[rstest]
    #[case::same_decimals_price_100(18, 18, 100.0)]
    #[case::high_to_low_price_100(18, 6, 100.0)]
    #[case::low_to_high_price_100(6, 18, 100.0)]
    #[case::same_decimals_price_2000(18, 18, 2000.0)]
    #[case::high_to_low_price_2000(18, 6, 2000.0)]
    #[case::low_to_high_price_2000(6, 18, 2000.0)]
    #[tokio::test]
    async fn test_compute_integration(
        #[case] decimals_in: u32,
        #[case] decimals_out: u32,
        #[case] spot_price: f64,
    ) {
        let eth = token_with_decimals(0, "ETH", decimals_in);
        let usdc = token_with_decimals(1, "USDC", decimals_out);

        let (market, _) = setup_market(vec![(
            "pool",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price)
                .with_liquidity(1_000_000)
                .with_tokens(&[eth.clone(), usdc.clone()]),
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

        assert_eq!(pool_depths.len(), 2, "should have depths for both directions");

        let key_eth_usdc: PoolDepthKey = ("pool".into(), eth.address.clone(), usdc.address.clone());
        let key_usdc_eth: PoolDepthKey = ("pool".into(), usdc.address.clone(), eth.address.clone());

        assert!(pool_depths.contains_key(&key_eth_usdc), "should have depth for ETH→USDC");
        assert!(pool_depths.contains_key(&key_usdc_eth), "should have depth for USDC→ETH");

        let expected_depth = |sell_token: &Token, buy_token: &Token| -> BigUint {
            let effective_price =
                if sell_token.address < buy_token.address { spot_price } else { 1.0 / spot_price };
            let base = BigUint::from((1_000_000.0 / effective_price) as u64);
            let decimal_diff = sell_token.decimals as i32 - buy_token.decimals as i32;
            if decimal_diff >= 0 {
                base * BigUint::from(10u64).pow(decimal_diff as u32)
            } else {
                base / BigUint::from(10u64).pow((-decimal_diff) as u32)
            }
        };
        assert_eq!(
            pool_depths.get(&key_eth_usdc).unwrap(),
            &expected_depth(&eth, &usdc),
            "ETH→USDC depth"
        );
        assert_eq!(
            pool_depths.get(&key_usdc_eth).unwrap(),
            &expected_depth(&usdc, &eth),
            "USDC→ETH depth"
        );
    }

    /// Verify that Price construction in compute() correctly handles decimal scaling
    /// across mixed-decimal token pairs (e.g. WETH(18)/USDC(6)).
    ///
    /// Uses the shared `query_pool_swap` function directly because UniV2's trait
    /// method rejects TradeLimitPrice, but the shared function works with any
    /// ProtocolSim via get_amount_out/spot_price.
    #[rstest]
    #[case::same_decimals(18, 18, 1000, 2000)]
    #[case::high_to_low(18, 6, 1000, 2_000_000)]
    #[case::low_to_high(6, 18, 2_000_000, 1000)]
    #[case::small_difference(8, 18, 100, 2000)]
    #[test]
    fn test_decimal_scaling_with_real_univ2(
        #[case] decimals_in: u32,
        #[case] decimals_out: u32,
        #[case] tokens_in_reserve: u64,
        #[case] tokens_out_reserve: u64,
    ) {
        use alloy::primitives::U256;
        use tycho_simulation::evm::{
            protocol::uniswap_v2::state::UniswapV2State, query_pool_swap::query_pool_swap,
        };

        let token_in = token_with_decimals(0x01, "IN", decimals_in);
        let token_out = token_with_decimals(0x02, "OUT", decimals_out);

        let reserve_in =
            U256::from(tokens_in_reserve) * U256::from(10u64).pow(U256::from(decimals_in));
        let reserve_out =
            U256::from(tokens_out_reserve) * U256::from(10u64).pow(U256::from(decimals_out));
        let univ2 = UniswapV2State::new(reserve_in, reserve_out);

        let spot_price = univ2
            .spot_price(&token_in, &token_out)
            .expect("spot_price should succeed");

        let slippage = 0.01;
        let min_price = spot_price * (1.0 - slippage);

        let decimal_diff = token_in.decimals as i32 - token_out.decimals as i32;
        let numerator = BigUint::from((min_price * 10_f64.powi(18)) as u128);
        let denominator = BigUint::from(10u64).pow((18 + decimal_diff) as u32);

        let limit_price = Price::new(numerator, denominator);

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

        let result = query_pool_swap(&univ2, &params);
        assert!(
            result.is_ok(),
            "query_pool_swap should succeed for {decimals_in}/{decimals_out} decimals, \
             got error: {:?}",
            result.err()
        );

        let swap = result.unwrap();
        assert!(
            !swap.amount_in().is_zero(),
            "amount_in should be non-zero for {decimals_in}/{decimals_out} decimals"
        );

        let post_swap_spot = swap
            .new_state()
            .spot_price(&token_in, &token_out)
            .expect("post-swap spot_price should succeed");
        let price_impact = ((post_swap_spot - spot_price) / spot_price).abs();
        assert!(
            price_impact <= slippage + 0.005,
            "post-swap price impact {price_impact:.4} should be near slippage {slippage} \
             for {decimals_in}/{decimals_out} decimals"
        );
    }

    /// Exercises the Brent solver fallback path with realistic UniV2 pool states to verify
    /// it produces sensible depth values. This validates that the Price construction
    /// approach in compute() is correct across a range of real-world token pairs.
    ///
    /// Three pools covering the key decimal configurations encountered in production:
    ///   - WETH/USDC: 18/6 decimals, ~$2000 price, ~$10M liquidity
    ///   - WETH/WBTC: 18/8 decimals, ~15 price, ~$5M liquidity
    ///   - USDC/USDT: 6/6 decimals, ~1 price, ~$50M liquidity
    #[test]
    fn test_brent_solver_with_realistic_pools() {
        use alloy::primitives::U256;
        use tycho_simulation::evm::{
            protocol::uniswap_v2::state::UniswapV2State, query_pool_swap::query_pool_swap,
        };

        struct PoolCase {
            name: &'static str,
            token_in: tycho_simulation::tycho_core::models::token::Token,
            token_out: tycho_simulation::tycho_core::models::token::Token,
            reserve_in_human: u64,
            reserve_out_human: u64,
        }

        // WETH reserve ~5000 ETH, USDC reserve ~10M USDC  → ~$2000/ETH, ~$10M TVL
        // WETH reserve ~333 ETH, WBTC reserve ~5000 WBTC  → ~15 WBTC/WETH, ~$5M TVL
        // USDC reserve ~25M, USDT reserve ~25M            → ~1:1, ~$50M TVL
        let cases = vec![
            PoolCase {
                name: "WETH(18)/USDC(6)",
                token_in: token_with_decimals(0x01, "WETH", 18),
                token_out: token_with_decimals(0x02, "USDC", 6),
                reserve_in_human: 5_000,
                reserve_out_human: 10_000_000,
            },
            PoolCase {
                name: "WETH(18)/WBTC(8)",
                token_in: token_with_decimals(0x01, "WETH", 18),
                token_out: token_with_decimals(0x02, "WBTC", 8),
                reserve_in_human: 5_000,
                reserve_out_human: 333,
            },
            PoolCase {
                name: "USDC(6)/USDT(6)",
                token_in: token_with_decimals(0x01, "USDC", 6),
                token_out: token_with_decimals(0x02, "USDT", 6),
                reserve_in_human: 25_000_000,
                reserve_out_human: 25_000_000,
            },
        ];

        let slippage = 0.01_f64;
        const SCALE_EXP: i32 = 18;

        for case in &cases {
            let decimals_in = case.token_in.decimals;
            let decimals_out = case.token_out.decimals;

            let reserve_in =
                U256::from(case.reserve_in_human) * U256::from(10u64).pow(U256::from(decimals_in));
            let reserve_out = U256::from(case.reserve_out_human) *
                U256::from(10u64).pow(U256::from(decimals_out));
            let univ2 = UniswapV2State::new(reserve_in, reserve_out);

            let spot_price = univ2
                .spot_price(&case.token_in, &case.token_out)
                .unwrap_or_else(|e| panic!("[{}] spot_price failed: {e}", case.name));

            let min_price = spot_price * (1.0 - slippage);

            let decimal_diff = decimals_in as i32 - decimals_out as i32;
            let denominator_exp = SCALE_EXP + decimal_diff;
            assert!(
                denominator_exp >= 0,
                "[{}] denominator_exp would be negative: {denominator_exp}",
                case.name
            );

            let numerator = BigUint::from((min_price * 10_f64.powi(SCALE_EXP)) as u128);
            let denominator = BigUint::from(10u64).pow(denominator_exp as u32);
            let limit_price = Price::new(numerator, denominator);

            let limit_price_f64 = min_price;

            let params = QueryPoolSwapParams::new(
                case.token_in.clone(),
                case.token_out.clone(),
                SwapConstraint::TradeLimitPrice {
                    limit: limit_price,
                    tolerance: 0.0,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            );

            let result = query_pool_swap(&univ2, &params)
                .unwrap_or_else(|e| panic!("[{}] query_pool_swap failed: {e}", case.name));

            let amount_in = result.amount_in();
            assert!(!amount_in.is_zero(), "[{}] amount_in (depth) should be non-zero", case.name);

            let post_swap_spot = result
                .new_state()
                .spot_price(&case.token_in, &case.token_out)
                .unwrap_or_else(|e| panic!("[{}] post-swap spot_price failed: {e}", case.name));
            let price_impact = ((post_swap_spot - spot_price) / spot_price).abs();

            let amount_in_human = {
                let raw: f64 = amount_in
                    .to_string()
                    .parse()
                    .unwrap_or(0.0);
                raw / 10_f64.powi(decimals_in as i32)
            };

            println!(
                "[{}] spot_price={:.6}, limit_price={:.6}, amount_in={} ({:.4} human), \
                 post_swap_spot={:.6}, price_impact={:.4}%",
                case.name,
                spot_price,
                limit_price_f64,
                amount_in,
                amount_in_human,
                post_swap_spot,
                price_impact * 100.0
            );

            assert!(
                price_impact <= slippage + 0.005,
                "[{}] price impact {:.4}% exceeds slippage {:.4}% + tolerance",
                case.name,
                price_impact * 100.0,
                slippage * 100.0
            );
        }
    }
}
