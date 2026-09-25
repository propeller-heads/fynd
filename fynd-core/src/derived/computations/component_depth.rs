//! Component depth computation.
//!
//! Computes liquidity depths for all components using `query_pool_swap`, falling back to
//! `depth_search::search_depth` when the component doesn't implement it natively.
//! Depth represents the maximum input amount before reaching the configured slippage
//! threshold from the spot price.
//!
//! # Dependencies
//!
//! This computation depends on [`SpotPrices`](crate::derived::types::SpotPrices) being
//! available in the [`DerivedData`](crate::derived::store::DerivedData).
//! Ensure `SpotPriceComputation` runs before this computation.

use async_trait::async_trait;
use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::Zero;
use rustc_hash::FxHashSet;
use tracing::{debug, instrument, warn, Span};
use tycho_simulation::{
    tycho_common::simulation::errors::SimulationError,
    tycho_core::simulation::protocol_sim::{Price, QueryPoolSwapParams, SwapConstraint},
};

use crate::{
    algorithm::sim_guard::GuardedProtocolSim,
    derived::{
        computation::{
            ComputationId, ComputationOutput, ComputationRequirements, DerivedComputation,
            FailedItem, FailedItemError,
        },
        computations::{depth_search::search_depth, spot_price::SpotPriceComputation},
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        store::DerivedData,
        types::ComponentDepths,
    },
    feed::market_data::{MarketData, MarketState},
    types::ComponentId,
};

/// Computes component depths for all components in all directions.
///
/// For each component and token pair, uses `query_pool_swap` (with `search_depth` as fallback)
/// to find the maximum input amount that results in at most the configured slippage
/// from spot price.
#[derive(Debug)]
pub struct ComponentDepthComputation {
    slippage_threshold: f64,
}

impl Default for ComponentDepthComputation {
    fn default() -> Self {
        Self { slippage_threshold: 0.01 }
    }
}

impl ComponentDepthComputation {
    /// Creates a new ComponentDepthComputation with the given slippage threshold.
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
impl DerivedComputation for ComponentDepthComputation {
    type Output = ComponentDepths;

    // Legacy ID: this string is the `computation` label value on Prometheus metrics
    // (derived_computation_* series), so renaming it would break existing dashboards.
    const ID: ComputationId = "pool_depths";

    fn requirements(&self) -> ComputationRequirements {
        ComputationRequirements::fresh([SpotPriceComputation::ID])
    }

    fn persist(
        store: &mut DerivedData,
        output: ComputationOutput<Self::Output>,
        block: u64,
        is_full_recompute: bool,
    ) {
        store.set_component_depths(output.data, output.failed_items, block, is_full_recompute);
    }

    #[instrument(level = "debug", skip(market, store, changed), fields(computation_id = Self::ID, updated_component_depths))]
    async fn compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
        // Read derived data from store
        let (spot_prices, mut component_depths) = {
            let store_guard = store.read().await;
            // Get precomputed spot prices (required dependency).
            let spot_prices = store_guard
                .spot_prices()
                .ok_or(ComputationError::MissingDependency(SpotPriceComputation::ID))?
                .clone();
            // Start with existing depths (or empty for full recompute).
            let component_depths = if changed.is_full_recompute {
                ComponentDepths::default()
            } else {
                store_guard
                    .component_depths()
                    .cloned()
                    .unwrap_or_default()
            };
            (spot_prices, component_depths)
        };

        // Remove component depths for removed components.
        for component_id in &changed.removed {
            component_depths.retain(|key, _| &key.0 != component_id);
        }

        // Snapshot market data under brief lock.
        let (snapshot, components_to_compute) = {
            let market_guard = market.read().await;
            let topology = market_guard.component_topology();

            // Determine which components need (re)computation.
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

            let component_ids: FxHashSet<&ComponentId> = components_to_compute.iter().collect();
            let snapshot: MarketState = market_guard.extract_subset(&component_ids);

            (snapshot, components_to_compute)
        };

        let topology = snapshot.component_topology();
        let tokens = snapshot.token_registry_ref();

        let mut succeeded = 0usize;
        let mut failed_items: Vec<FailedItem> = Vec::new();

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
                warn!(component_id, "missing simulation state, skipping component");
                component_depths.retain(|key, _| &key.0 != component_id);
                for perm in token_addresses.iter().permutations(2) {
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, perm[0], perm[1]),
                        error: FailedItemError::MissingSimulationState,
                    });
                }
                continue;
            };

            let component_tokens: Result<Vec<_>, _> = token_addresses
                .iter()
                .map(|addr| tokens.get(addr).ok_or(addr))
                .collect();
            let Ok(component_tokens) = component_tokens else {
                warn!(component_id, "missing token metadata, skipping component");
                component_depths.retain(|key, _| &key.0 != component_id);
                for perm in token_addresses.iter().permutations(2) {
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, perm[0], perm[1]),
                        error: FailedItemError::MissingTokenMetadata,
                    });
                }
                continue;
            };

            for perm in component_tokens.iter().permutations(2) {
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
                    component_depths.remove(&key);
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, token_in.address, token_out.address),
                        error: FailedItemError::MissingSpotPrice,
                    });
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
                    component_depths.remove(&key);
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, token_in.address, token_out.address),
                        error: FailedItemError::ExtremeDecimalMismatch {
                            from: token_in.decimals,
                            to: token_out.decimals,
                        },
                    });
                    continue;
                }

                let numerator = BigUint::from((min_price * 10_f64.powi(SCALE_EXP)) as u128);
                let denominator = BigUint::from(10u64).pow(denominator_exp as u32);

                if numerator.is_zero() {
                    debug!(
                        component_id,
                        token_in = %token_in.address,
                        token_out = %token_out.address,
                        spot_price,
                        "spot price too small to compute depth, skipping pair"
                    );
                    component_depths.remove(&key);
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, token_in.address, token_out.address),
                        error: FailedItemError::SpotPriceTooSmall(*spot_price),
                    });
                    continue;
                }

                let limit_price = Price::new(numerator, denominator);

                let params = QueryPoolSwapParams::new(
                    (**token_in).clone(),
                    (**token_out).clone(),
                    SwapConstraint::TradeLimitPrice {
                        limit: limit_price,
                        tolerance: 0.0,
                        min_amount_in: None,
                        max_amount_in: None,
                    },
                );

                let fallback_depth_search =
                    || search_depth(sim_state, *spot_price, min_price, token_in, token_out);
                let depth_result = match sim_state.query_pool_swap(&params) {
                    Ok(swap) => Ok(swap.amount_in().clone()),
                    Err(SimulationError::FatalError(msg))
                        if msg == "query_pool_swap not implemented" =>
                    {
                        fallback_depth_search()
                    }
                    Err(SimulationError::InvalidInput(msg, _))
                        if msg.contains("does not support TradeLimitPrice") =>
                    {
                        fallback_depth_search()
                    }
                    Err(e) => Err(e),
                }
                .map_err(|e| {
                    ComputationError::SimulationFailed(format!(
                        "depth query failed for {}/{}: {e}",
                        token_in.address, token_out.address
                    ))
                });

                match depth_result {
                    Ok(depth) => {
                        component_depths.insert(key, depth);
                        succeeded += 1;
                    }
                    Err(e) => {
                        // Diagnostic: probe with 1 unit to understand why depth search failed.
                        // Guarded so a panicking component degrades to a diagnostic string instead
                        // of killing the computation worker.
                        let probe_info = sim_state
                            .get_amount_out_guarded(BigUint::from(1u32), token_in, token_out)
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
                            "component depth failed, skipping pair"
                        );
                        component_depths.remove(&key);
                        failed_items.push(FailedItem {
                            key: format!(
                                "{}/{}/{}",
                                component_id, token_in.address, token_out.address
                            ),
                            error: FailedItemError::SimulationFailed(format!(
                                "{e}: {probe_info}, {limits_info}"
                            )),
                        });
                    }
                }
            }
        }

        debug!(
            succeeded,
            failed = failed_items.len(),
            total = component_depths.len(),
            "component depth computation complete"
        );
        Span::current().record("updated_component_depths", component_depths.len());

        Ok(ComputationOutput::with_failures(component_depths, failed_items))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            setup_market_weighted, setup_market_weighted_boxed, token, token_with_decimals,
            MockProtocolSim,
        },
        derived::{
            computation::FailedItemError,
            store::DerivedData,
            types::{ComponentDepthKey, SpotPrices},
        },
        feed::market_data::MarketData,
    };

    #[test]
    fn computation_id() {
        assert_eq!(ComponentDepthComputation::ID, "pool_depths");
    }

    #[test]
    fn default_slippage_is_one_percent() {
        let comp = ComponentDepthComputation::default();
        assert!((comp.slippage_threshold - 0.01).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case(0.001)]
    #[case(0.01)]
    #[case(0.5)]
    #[case(0.99)]
    fn new_with_valid_slippage(#[case] threshold: f64) {
        let comp = ComponentDepthComputation::new(threshold).unwrap();
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
        let result = ComponentDepthComputation::new(threshold);
        assert!(
            matches!(result, Err(ComputationError::InvalidConfiguration(_))),
            "expected InvalidConfiguration for {_desc}, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_compute_handles_empty_market() {
        let market = MarketData::new_shared();
        let derived = DerivedData::new_shared();
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(SpotPrices::default(), vec![], 0, true);
        let changed = ChangedComponents::default();

        let output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        assert!(output.data.is_empty());
    }

    #[tokio::test]
    async fn test_compute_missing_spot_prices_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, _) =
            setup_market_weighted(vec![("component", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        let derived = DerivedData::new_shared(); // No spot prices
        let changed = ChangedComponents::default();

        let result = ComponentDepthComputation::default()
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

        let (market, _) = setup_market_weighted(vec![(
            "component",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price)
                .with_liquidity(1_000_000)
                .with_tokens(&[eth.clone(), usdc.clone()]),
        )]);
        let derived = DerivedData::new_shared();
        let spot_comp = SpotPriceComputation::new();
        let changed = ChangedComponents {
            added: FxHashMap::from_iter([(
                "component".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };
        let spot_output = spot_comp
            .compute(&market, &derived, &changed)
            .await
            .expect("spot price computation should succeed");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_output.data, vec![], 0, true);

        let component_depths_output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("computation should succeed");
        let component_depths = component_depths_output.data;

        assert_eq!(component_depths.len(), 2, "should have depths for both directions");

        let key_eth_usdc: ComponentDepthKey =
            ("component".into(), eth.address.clone(), usdc.address.clone());
        let key_usdc_eth: ComponentDepthKey =
            ("component".into(), usdc.address.clone(), eth.address.clone());

        assert!(component_depths.contains_key(&key_eth_usdc), "should have depth for ETH→USDC");
        assert!(component_depths.contains_key(&key_usdc_eth), "should have depth for USDC→ETH");

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
            component_depths
                .get(&key_eth_usdc)
                .unwrap(),
            &expected_depth(&eth, &usdc),
            "ETH→USDC depth"
        );
        assert_eq!(
            component_depths
                .get(&key_usdc_eth)
                .unwrap(),
            &expected_depth(&usdc, &eth),
            "USDC→ETH depth"
        );
    }

    #[tokio::test]
    async fn test_compute_depth_search_fallback() {
        // UniswapV2's own `query_pool_swap` rejects `TradeLimitPrice`, so `compute` falls back to
        // `search_depth` for both directions.
        use alloy::primitives::U256;
        use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let univ2 = UniswapV2State::new(
            U256::from(5_000u64) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(10_000_000u64) * U256::from(10u64).pow(U256::from(6u64)),
        );
        let (market, _) =
            setup_market_weighted_boxed(vec![("component", &weth, &usdc, Box::new(univ2))]);
        let derived = DerivedData::new_shared();
        let changed = ChangedComponents {
            added: FxHashMap::from_iter([(
                "component".to_string(),
                vec![weth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };
        let spot_output = SpotPriceComputation::new()
            .compute(&market, &derived, &changed)
            .await
            .expect("spot price computation should succeed");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_output.data, vec![], 0, true);

        let output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("computation should succeed");

        assert!(!output.has_failures(), "failed items: {:?}", output.failed_items);
        for (token_in, token_out) in [(&weth, &usdc), (&usdc, &weth)] {
            let key: ComponentDepthKey =
                ("component".into(), token_in.address.clone(), token_out.address.clone());
            let depth = output
                .data
                .get(&key)
                .expect("the depth is stored");
            assert!(!depth.is_zero(), "{} depth is zero", token_in.symbol);
        }
    }

    #[tokio::test]
    async fn test_compute_partial_failure_missing_spot_price() {
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");

        let (market, _) = setup_market_weighted(vec![(
            "component",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0)
                .with_liquidity(1_000_000)
                .with_tokens(&[eth.clone(), usdc.clone()]),
        )]);
        let derived = DerivedData::new_shared();

        // Provide spot price for only one direction so the other becomes a FailedItem
        let mut partial_spot = SpotPrices::default();
        let key_eth_usdc = ("component".to_string(), eth.address.clone(), usdc.address.clone());
        partial_spot.insert(key_eth_usdc, 2000.0);
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(partial_spot, vec![], 0, true);

        let changed = ChangedComponents {
            added: FxHashMap::from_iter([(
                "component".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };

        let output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("should succeed with partial results");

        assert!(output.has_failures(), "missing USDC→ETH spot price should produce a failed item");

        // ETH→USDC direction should succeed
        let key_eth_usdc: ComponentDepthKey =
            ("component".into(), eth.address.clone(), usdc.address.clone());
        assert!(output.data.contains_key(&key_eth_usdc), "ETH→USDC depth should be present");

        // USDC→ETH direction should be in failed_items
        let usdc_eth_key = format!("component/{}/{}", usdc.address, eth.address);
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == usdc_eth_key &&
                    matches!(item.error, FailedItemError::MissingSpotPrice)),
            "USDC→ETH should appear in failed_items with missing spot price error"
        );
    }

    #[tokio::test]
    async fn test_compute_partial_failure_missing_simulation_state() {
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");

        // Empty market — no simulation state
        let market = MarketData::new_shared();
        let derived = DerivedData::new_shared();
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(SpotPrices::default(), vec![], 0, true);

        let changed = ChangedComponents {
            added: FxHashMap::from_iter([(
                "phantom_component".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: false,
        };

        let output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("should succeed with partial results");

        assert!(output.has_failures());

        let eth_usdc_key = format!("phantom_component/{}/{}", eth.address, usdc.address);
        let usdc_eth_key = format!("phantom_component/{}/{}", usdc.address, eth.address);
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == eth_usdc_key &&
                    matches!(item.error, FailedItemError::MissingSimulationState)),
            "ETH→USDC should fail with MissingSimulationState"
        );
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == usdc_eth_key &&
                    matches!(item.error, FailedItemError::MissingSimulationState)),
            "USDC→ETH should fail with MissingSimulationState"
        );
    }

    #[tokio::test]
    async fn test_compute_partial_failure_component_depth_computation() {
        // Without .with_tokens(), get_limits doesn't scale by decimals,
        // but get_amount_out does — causing a liquidity overflow on swap.
        let token_in = token_with_decimals(0x01, "A", 6);
        let token_out = token_with_decimals(0x02, "B", 18);

        let (market, _) = setup_market_weighted(vec![(
            "component",
            &token_in,
            &token_out,
            MockProtocolSim::new(1.0).with_liquidity(100),
        )]);
        let derived = DerivedData::new_shared();

        let changed = ChangedComponents {
            added: FxHashMap::from_iter([(
                "component".to_string(),
                vec![token_in.address.clone(), token_out.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };

        let spot_output = SpotPriceComputation::new()
            .compute(&market, &derived, &changed)
            .await
            .expect("spot price computation should succeed");
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(spot_output.data, vec![], 0, true);

        let output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("should succeed with partial results");

        assert!(
            output.has_failures(),
            "decimal mismatch between get_limits and get_amount_out should cause failures"
        );
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key.starts_with("component/") &&
                    matches!(&item.error, FailedItemError::SimulationFailed(_))),
            "should have ComputationFailed failure, got: {:?}",
            output.failed_items
        );
    }
}
