//! Component depth computation.
//!
//! Computes the depth of every component with `query_pool_swap` and a `PoolTargetPrice`
//! constraint. A component that does not answer that query uses tycho's generic search. The depth
//! is the input after which the pool's marginal price, net of its fee, has fallen by the configured
//! marginal price drop.
//!
//! # Dependencies
//!
//! This computation depends on [`SpotPrices`](crate::derived::types::SpotPrices) being
//! available in the [`DerivedData`](crate::derived::store::DerivedData).
//! Ensure `SpotPriceComputation` runs before this computation.

use async_trait::async_trait;
use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::{Float, One};
use rustc_hash::FxHashSet;
use tracing::{debug, instrument, warn, Span};
use tycho_simulation::{
    evm::query_pool_swap::query_pool_swap,
    tycho_common::{
        models::{token::Token, Address},
        simulation::errors::SimulationError,
    },
    tycho_core::simulation::protocol_sim::{
        Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint,
    },
};

use crate::{
    algorithm::sim_guard::GuardedProtocolSim,
    derived::{
        computation::{
            ComputationId, ComputationOutput, ComputationRequirements, DerivedComputation,
            FailedItem, FailedItemError,
        },
        computations::spot_price::SpotPriceComputation,
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        store::DerivedData,
        types::{ComponentDepthKey, ComponentDepths, SpotPrices},
    },
    feed::market_data::{MarketData, MarketState},
    types::ComponentId,
};

/// Converts `price`, in whole tokens of `token_out` per whole token of `token_in`, to the exact
/// fraction of smallest units that `query_pool_swap` reads.
fn to_raw_price(price: f64, decimals_in: u32, decimals_out: u32) -> Price {
    let (mantissa, exponent, _) = price.integer_decode();
    let mut numerator = BigUint::from(mantissa);
    let mut denominator = BigUint::one();
    let shift = usize::from(exponent.unsigned_abs());
    if exponent >= 0 {
        numerator <<= shift;
    } else {
        denominator <<= shift;
    }
    if decimals_out >= decimals_in {
        numerator *= BigUint::from(10u32).pow(decimals_out - decimals_in);
    } else {
        denominator *= BigUint::from(10u32).pow(decimals_in - decimals_out);
    }
    Price::new(numerator, denominator)
}

/// The default share by which a pool's net marginal price falls at its depth.
pub(crate) const DEFAULT_MARGINAL_PRICE_DROP: f64 = 0.015;

/// Computes component depths for all components in all directions.
///
/// For each component and token pair, finds the input after which the pool's marginal price,
/// net of its fee, has fallen by `marginal_price_drop`: with `query_pool_swap` and a
/// `PoolTargetPrice` constraint, or with tycho's generic search for a pool that has no such query.
#[derive(Debug)]
pub struct ComponentDepthComputation {
    /// The share by which the pool's net marginal price falls at the depth.
    marginal_price_drop: f64,
}

impl Default for ComponentDepthComputation {
    fn default() -> Self {
        Self { marginal_price_drop: DEFAULT_MARGINAL_PRICE_DROP }
    }
}

impl ComponentDepthComputation {
    /// Creates a new ComponentDepthComputation with the given marginal price drop.
    ///
    /// # Arguments
    /// * `marginal_price_drop` - The share by which the pool's net marginal price falls at the
    ///   depth, between 0 and 1 exclusive (e.g., 0.015 for 1.5%)
    ///
    /// # Errors
    /// Returns `InvalidConfiguration` if marginal_price_drop is not in (0, 1).
    pub fn new(marginal_price_drop: f64) -> Result<Self, ComputationError> {
        if !(marginal_price_drop > 0.0 && marginal_price_drop < 1.0) {
            return Err(ComputationError::InvalidConfiguration(format!(
                "marginal_price_drop must be between 0 and 1 exclusive, got {marginal_price_drop}"
            )));
        }
        Ok(Self { marginal_price_drop })
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
        let (spot_prices, mut component_depths) = read_stored_data(store, changed).await?;
        for component_id in &changed.removed {
            component_depths.retain(|key, _| &key.0 != component_id);
        }
        let (snapshot, components_to_compute) = snapshot_changed_components(market, changed).await;
        let topology = snapshot.component_topology();

        let mut succeeded = 0usize;
        let mut failed_items: Vec<FailedItem> = Vec::new();
        for component_id in &components_to_compute {
            // Get token addresses: changed.added for new components, topology for existing
            let Some(token_addresses) = changed
                .added
                .get(component_id)
                .or_else(|| topology.get(component_id))
            else {
                continue; // Component might have been removed in the meantime
            };
            let pair_depths = self.compute_component_depths(
                component_id,
                token_addresses,
                &snapshot,
                &spot_prices,
            );
            for (key, depth) in pair_depths {
                match depth {
                    Ok(depth) => {
                        component_depths.insert(key, depth);
                        succeeded += 1;
                    }
                    Err(error) => {
                        component_depths.remove(&key);
                        let (component_id, token_in, token_out) = key;
                        failed_items.push(FailedItem {
                            key: format!("{component_id}/{token_in}/{token_out}"),
                            error,
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

/// The depth of one directed pair of a component, or why it has none.
type PairDepth = (ComponentDepthKey, Result<BigUint, FailedItemError>);

/// Reads the stored spot prices, and the stored depths to start from. A full recompute starts
/// from no depths.
async fn read_stored_data(
    store: &SharedDerivedDataRef,
    changed: &ChangedComponents,
) -> Result<(SpotPrices, ComponentDepths), ComputationError> {
    let store_guard = store.read().await;
    let spot_prices = store_guard
        .spot_prices()
        .ok_or(ComputationError::MissingDependency(SpotPriceComputation::ID))?
        .clone();
    let component_depths = if changed.is_full_recompute {
        ComponentDepths::default()
    } else {
        store_guard
            .component_depths()
            .cloned()
            .unwrap_or_default()
    };
    Ok((spot_prices, component_depths))
}

/// Returns the components whose depths need computing, and a market snapshot of them taken under
/// a brief lock. A full recompute computes every component; otherwise the added and updated ones.
async fn snapshot_changed_components(
    market: &MarketData,
    changed: &ChangedComponents,
) -> (MarketState, Vec<ComponentId>) {
    let market_guard = market.read().await;
    let components_to_compute: Vec<ComponentId> = if changed.is_full_recompute {
        market_guard
            .component_topology()
            .into_keys()
            .collect()
    } else {
        changed
            .added
            .keys()
            .chain(changed.updated.iter())
            .cloned()
            .collect()
    };
    let component_ids: FxHashSet<&ComponentId> = components_to_compute.iter().collect();
    (market_guard.extract_subset(&component_ids), components_to_compute)
}

/// Returns the pool's marginal price from `token_in` to `token_out`, net of its fee, or `None`
/// without a spot price for the pair.
///
/// tycho's `PoolTargetPrice` compares its target with the price net of the pool's fee.
/// `spot_price` adds the fee for some pools (Uniswap V2 and V3) and is net of it for others (many
/// Uniswap V4 pools), so the net price is the lower of `spot(in→out)` and `1 / spot(out→in)`.
/// Without a reverse spot price, the forward one is the marginal price.
fn net_marginal_price(spot_prices: &SpotPrices, key: &ComponentDepthKey) -> Option<f64> {
    let spot_price = spot_prices.get(key)?;
    let (component_id, token_in, token_out) = key;
    let reverse_key = (component_id.clone(), token_out.clone(), token_in.clone());
    let marginal_price = spot_prices
        .get(&reverse_key)
        .map_or(*spot_price, |reverse_spot_price| spot_price.min(1.0 / reverse_spot_price));
    Some(marginal_price)
}

/// Queries the input that moves the pool's net marginal price to `target_price`. A pool with no
/// query of its own uses tycho's generic search.
fn query_depth(
    sim_state: &dyn ProtocolSim,
    token_in: &Token,
    token_out: &Token,
    target_price: f64,
) -> Result<BigUint, SimulationError> {
    let params = QueryPoolSwapParams::new(
        token_in.clone(),
        token_out.clone(),
        SwapConstraint::PoolTargetPrice {
            target: to_raw_price(target_price, token_in.decimals, token_out.decimals),
            tolerance: 0.0,
            min_amount_in: None,
            max_amount_in: None,
        },
    );
    let pool_swap = match sim_state.query_pool_swap(&params) {
        Err(SimulationError::FatalError(msg)) if msg == "query_pool_swap not implemented" => {
            query_pool_swap(sim_state, &params)
        }
        result => result,
    }?;
    Ok(pool_swap.amount_in().clone())
}

/// Describes a pool whose depth query failed: what a swap of one unit returns, and the pool's
/// swap limits.
///
/// The swap is guarded, so a panicking component degrades to a diagnostic string instead of
/// killing the computation worker.
fn describe_failed_query(
    sim_state: &dyn ProtocolSim,
    token_in: &Token,
    token_out: &Token,
) -> (String, String) {
    let probe_info = sim_state
        .get_amount_out_guarded(BigUint::from(1u32), token_in, token_out)
        .map(|r| format!("amount_out={}", r.amount))
        .unwrap_or_else(|e| format!("sim_error={e}"));
    let limits_info = sim_state
        .get_limits(token_in.address.clone(), token_out.address.clone())
        .map(|(max_in, max_out)| format!("max_in={max_in}, max_out={max_out}"))
        .unwrap_or_else(|e| format!("limits_error={e}"));
    (probe_info, limits_info)
}

impl ComponentDepthComputation {
    /// Computes the depth of every directed pair of `component_id`'s tokens. Every pair fails
    /// when the component misses its simulation state or the metadata of a token.
    fn compute_component_depths(
        &self,
        component_id: &ComponentId,
        token_addresses: &[Address],
        snapshot: &MarketState,
        spot_prices: &SpotPrices,
    ) -> Vec<PairDepth> {
        let fail_every_pair = |error: FailedItemError| -> Vec<PairDepth> {
            token_addresses
                .iter()
                .permutations(2)
                .map(|pair| {
                    let key = (component_id.clone(), pair[0].clone(), pair[1].clone());
                    (key, Err(error.clone()))
                })
                .collect()
        };
        let Some(sim_state) = snapshot.get_simulation_state(component_id) else {
            warn!(component_id, "missing simulation state, skipping component");
            return fail_every_pair(FailedItemError::MissingSimulationState);
        };
        let tokens = snapshot.token_registry_ref();
        let component_tokens: Option<Vec<&Token>> = token_addresses
            .iter()
            .map(|address| {
                tokens
                    .get(address)
                    .map(|token| &**token)
            })
            .collect();
        let Some(component_tokens) = component_tokens else {
            warn!(component_id, "missing token metadata, skipping component");
            return fail_every_pair(FailedItemError::MissingTokenMetadata);
        };

        component_tokens
            .iter()
            .permutations(2)
            .map(|pair| {
                let (token_in, token_out) = (*pair[0], *pair[1]);
                let key =
                    (component_id.clone(), token_in.address.clone(), token_out.address.clone());
                let depth =
                    self.compute_pair_depth(sim_state, &key, token_in, token_out, spot_prices);
                (key, depth)
            })
            .collect()
    }

    /// Computes the input after which the pool's net marginal price from `token_in` to
    /// `token_out` has fallen by `marginal_price_drop`.
    fn compute_pair_depth(
        &self,
        sim_state: &dyn ProtocolSim,
        key: &ComponentDepthKey,
        token_in: &Token,
        token_out: &Token,
        spot_prices: &SpotPrices,
    ) -> Result<BigUint, FailedItemError> {
        let component_id = &key.0;
        let Some(marginal_price) = net_marginal_price(spot_prices, key) else {
            warn!(
                component_id,
                token_in = %token_in.address,
                token_out = %token_out.address,
                "missing spot price, skipping pair"
            );
            return Err(FailedItemError::MissingSpotPrice);
        };
        let target_price = marginal_price * (1.0 - self.marginal_price_drop);
        if !(target_price.is_finite() && target_price > 0.0) {
            debug!(
                component_id,
                token_in = %token_in.address,
                token_out = %token_out.address,
                marginal_price,
                "target price is not a positive finite number, skipping pair"
            );
            return Err(FailedItemError::InvalidTargetPrice(target_price));
        }
        query_depth(sim_state, token_in, token_out, target_price).map_err(|error| {
            let (probe_info, limits_info) = describe_failed_query(sim_state, token_in, token_out);
            debug!(
                component_id,
                token_in = %token_in.address,
                token_out = %token_out.address,
                marginal_price,
                target_price,
                probe_info,
                limits_info,
                %error,
                "component depth failed, skipping pair"
            );
            FailedItemError::SimulationFailed(format!(
                "depth query failed: {error}: {probe_info}, {limits_info}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use num_traits::{ToPrimitive, Zero};
    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            setup_market_weighted, setup_market_weighted_boxed, token, token_with_decimals,
            ConstantProductSim, MockProtocolSim,
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
    fn test_default_marginal_price_drop() {
        let comp = ComponentDepthComputation::default();
        assert!((comp.marginal_price_drop - 0.015).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case(0.001)]
    #[case(0.01)]
    #[case(0.5)]
    #[case(0.99)]
    fn new_with_valid_marginal_price_drop(#[case] threshold: f64) {
        let comp = ComponentDepthComputation::new(threshold).unwrap();
        assert!((comp.marginal_price_drop - threshold).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case(0.0, "zero")]
    #[case(1.0, "one")]
    #[case(-0.1, "negative")]
    #[case(1.5, "greater than one")]
    #[case(f64::NAN, "NaN")]
    #[case(f64::INFINITY, "infinity")]
    fn new_with_invalid_marginal_price_drop(#[case] threshold: f64, #[case] _desc: &str) {
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
    async fn test_compute_native_pool_target_price() {
        // UniswapV2 answers `PoolTargetPrice` itself, for both directions.
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
    async fn test_compute_generic_search_fallback() {
        // `ConstantProductSim` has no `query_pool_swap` of its own, so `compute` falls back to
        // tycho's generic search. With no fee, its price after a swap of `x` is
        // `spot * (reserve_in / (reserve_in + x))²`, so a 1.5% fall takes
        // `x = reserve_in * (1 / sqrt(0.985) - 1)`.
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");
        let pool = ConstantProductSim {
            reserve_0: BigUint::from(1_000u32) * BigUint::from(10u32).pow(18),
            reserve_1: BigUint::from(2_000_000u32) * BigUint::from(10u32).pow(18),
            gas: 100_000,
        };
        // The ETH reserve is 1e21 wei.
        let expected_depth = 1e21 * (1.0 / 0.985f64.sqrt() - 1.0);
        let (market, _) =
            setup_market_weighted_boxed(vec![("component", &eth, &usdc, Box::new(pool))]);
        let derived = DerivedData::new_shared();
        let changed = ChangedComponents {
            added: FxHashMap::from_iter([(
                "component".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
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

        let key: ComponentDepthKey =
            ("component".into(), eth.address.clone(), usdc.address.clone());
        let depth = output
            .data
            .get(&key)
            .and_then(ToPrimitive::to_f64)
            .expect("the depth is stored");
        assert!((depth - expected_depth).abs() / expected_depth < 1e-4, "depth {depth}");
    }

    #[tokio::test]
    async fn test_compute_partial_failure_missing_spot_price() {
        // The eth_usdc pool has a spot price only from ETH to USDC, so only that direction gets a
        // depth. The eth_dai pool has both and gets both depths.
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");
        let dai = token(0x03, "DAI");
        let pool = |quote: &Token| {
            MockProtocolSim::new(2000.0)
                .with_liquidity(1_000_000)
                .with_tokens(&[eth.clone(), quote.clone()])
        };
        let (market, _) = setup_market_weighted(vec![
            ("eth_usdc", &eth, &usdc, pool(&usdc)),
            ("eth_dai", &eth, &dai, pool(&dai)),
        ]);
        let derived = DerivedData::new_shared();
        let key = |component: &str, token_in: &Token, token_out: &Token| {
            (component.to_string(), token_in.address.clone(), token_out.address.clone())
        };
        let mut partial_spot = SpotPrices::default();
        partial_spot.insert(key("eth_usdc", &eth, &usdc), 2000.0);
        partial_spot.insert(key("eth_dai", &eth, &dai), 2000.0);
        partial_spot.insert(key("eth_dai", &dai, &eth), 1.0 / 2000.0);
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(partial_spot, vec![], 0, true);
        let changed = ChangedComponents {
            added: FxHashMap::from_iter([
                ("eth_usdc".to_string(), vec![eth.address.clone(), usdc.address.clone()]),
                ("eth_dai".to_string(), vec![eth.address.clone(), dai.address.clone()]),
            ]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };

        let output = ComponentDepthComputation::default()
            .compute(&market, &derived, &changed)
            .await
            .expect("should succeed with partial results");

        assert!(output
            .data
            .contains_key(&key("eth_dai", &eth, &dai)));
        assert!(output
            .data
            .contains_key(&key("eth_dai", &dai, &eth)));
        assert!(output
            .data
            .contains_key(&key("eth_usdc", &eth, &usdc)));
        let failed_key = format!("eth_usdc/{}/{}", usdc.address, eth.address);
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == failed_key &&
                    matches!(item.error, FailedItemError::MissingSpotPrice)),
            "{failed_key} should fail with a missing spot price"
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
