//! Component depth computation.
//!
//! Computes liquidity depths for all components using `query_pool_swap`, falling back to
//! the generic Brent solver from tycho-simulation when the component doesn't implement it
//! natively. Depth represents the maximum input amount a component absorbs before its execution
//! price falls the configured slippage threshold below the price it starts executing at.
//!
//! # Dependencies
//!
//! This computation depends on [`SpotPrices`](crate::derived::types::SpotPrices) being
//! available in the [`DerivedData`](crate::derived::store::DerivedData).
//! Ensure `SpotPriceComputation` runs before this computation.

use std::collections::HashSet;

use async_trait::async_trait;
use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::Zero;
use tracing::{debug, instrument, warn, Span};
use tycho_simulation::{
    evm::query_pool_swap::query_pool_swap,
    tycho_common::{models::token::Token, simulation::errors::SimulationError},
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
        types::ComponentDepths,
    },
    feed::market_data::{MarketData, MarketState},
    types::ComponentId,
};

const DEFAULT_SLIPPAGE_THRESHOLD: f64 = 0.01;

const RETAINED_PRICE_SCALE: u128 = 1_000_000_000_000_000_000;

/// Computes component depths for all components in all directions.
///
/// For each component and token pair, uses `query_pool_swap` (with Brent solver fallback)
/// to find the maximum input amount that still executes within the configured slippage
/// of the component's own executable start price.
#[derive(Debug)]
pub struct ComponentDepthComputation {
    retained_price_numerator: u128,
}

impl Default for ComponentDepthComputation {
    fn default() -> Self {
        Self::new(DEFAULT_SLIPPAGE_THRESHOLD)
            .expect("the default slippage threshold is a valid configuration")
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
        let retained_price_numerator =
            ((1.0 - slippage_threshold) * RETAINED_PRICE_SCALE as f64).round() as u128;
        Ok(Self { retained_price_numerator })
    }

    // A spot-anchored target `spot * (1 - slippage)` is reachable only when
    // `(1 - f)^2 >= 1 - slippage`, which at a 1% threshold excludes every pool over ~0.5013%.
    fn plan_depth_search(
        &self,
        sim_state: &dyn ProtocolSim,
        token_in: &Token,
        token_out: &Token,
        spot_price: f64,
    ) -> Result<DepthSearch, FailedItemError> {
        let (max_amount_in, _) = sim_state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .map_err(|e| FailedItemError::SimulationFailed(format!("get_limits failed: {e}")))?;

        let start =
            probe_execution_start(sim_state, token_in, token_out, spot_price, &max_amount_in)
                .map_err(|e| {
                    FailedItemError::SimulationFailed(format!(
                        "no executable start price (max_in={max_amount_in}): {e}"
                    ))
                })?;

        let target = Price::new(
            start.amount_out * BigUint::from(self.retained_price_numerator),
            start.amount_in * BigUint::from(RETAINED_PRICE_SCALE),
        );

        // `query_pool_swap` rejects a target the component holds across its whole range, so
        // its limit is the answer.
        let holds_target_at_limit = sim_state
            .get_amount_out_guarded(max_amount_in.clone(), token_in, token_out)
            .is_ok_and(|capacity| executes_at_or_above(&max_amount_in, &capacity.amount, &target));
        if holds_target_at_limit {
            return Ok(DepthSearch::LimitedByComponentCapacity(max_amount_in));
        }

        Ok(DepthSearch::Target(target))
    }
}

#[derive(Debug)]
enum DepthSearch {
    LimitedByComponentCapacity(BigUint),
    Target(Price),
}

// Cross-multiplied rather than divided, so the comparison of raw amounts is exact.
fn executes_at_or_above(amount_in: &BigUint, amount_out: &BigUint, price: &Price) -> bool {
    amount_out * &price.denominator >= &price.numerator * amount_in
}

#[derive(Debug, Clone)]
struct ExecutionStart {
    amount_in: BigUint,
    amount_out: BigUint,
}

impl ExecutionStart {
    fn prices_above(&self, other: &Self) -> bool {
        &self.amount_out * &other.amount_in > &other.amount_out * &self.amount_in
    }
}

// Dust trades understate the execution rate protocol-specifically (outputs floor, integer fees
// round up, VM adapters carry fixed-point error), so measured prices rise with size until the
// true, decreasing curve takes over. Probing takes the peak rather than trusting one size.
fn probe_execution_start(
    sim_state: &dyn ProtocolSim,
    token_in: &Token,
    token_out: &Token,
    spot_price: f64,
    max_amount_in: &BigUint,
) -> Result<ExecutionStart, SimulationError> {
    // Clamped: `10^bits` already exceeds the limit, and seeding above it would report a
    // direction untradeable when its limit may still buy a whole output unit.
    let seed_exponent =
        first_nonzero_output_exponent(spot_price, token_in.decimals, token_out.decimals)
            .min(u32::try_from(max_amount_in.bits()).unwrap_or(u32::MAX));
    let mut amount_in = BigUint::from(10u64)
        .pow(seed_exponent)
        .min(max_amount_in.clone())
        .max(BigUint::from(1u32));
    let mut peak: Option<ExecutionStart> = None;
    let mut last_rejection: Option<SimulationError> = None;

    while &amount_in <= max_amount_in {
        match sim_state.get_amount_out_guarded(amount_in.clone(), token_in, token_out) {
            Ok(result) if !result.amount.is_zero() => {
                let probe =
                    ExecutionStart { amount_in: amount_in.clone(), amount_out: result.amount };
                if let Some(best) = &peak {
                    if !probe.prices_above(best) {
                        break;
                    }
                }
                peak = Some(probe);
            }
            Ok(_) => {}
            // A rejection below an adapter's minimum describes the probe, not the pool: step up.
            Err(error) => last_rejection = Some(error),
        }
        amount_in *= 10u32;
    }

    peak.ok_or_else(|| {
        last_rejection.unwrap_or_else(|| {
            SimulationError::RecoverableError(
                "no trade size within the pool's limit returns any output".to_string(),
            )
        })
    })
}

// `amount_in = 10^(decimals_in - decimals_out) / spot` yields one raw unit of the output token.
fn first_nonzero_output_exponent(spot_price: f64, decimals_in: u32, decimals_out: u32) -> u32 {
    if spot_price <= 0.0 || !spot_price.is_finite() {
        return 0;
    }
    let exponent = (decimals_in as f64 - decimals_out as f64 - spot_price.log10()).ceil();
    exponent.max(0.0) as u32
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
                ComponentDepths::new()
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

            let component_ids: HashSet<ComponentId> = components_to_compute
                .iter()
                .cloned()
                .collect();
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

                let search =
                    match self.plan_depth_search(sim_state, token_in, token_out, *spot_price) {
                        Ok(search) => search,
                        Err(error) => {
                            debug!(
                                component_id,
                                token_in = %token_in.address,
                                token_out = %token_out.address,
                                spot_price,
                                %error,
                                "cannot anchor a depth target, skipping pair"
                            );
                            component_depths.remove(&key);
                            failed_items.push(FailedItem {
                                key: format!(
                                    "{}/{}/{}",
                                    component_id, token_in.address, token_out.address
                                ),
                                error,
                            });
                            continue;
                        }
                    };

                let limit_price = match search {
                    DepthSearch::LimitedByComponentCapacity(max_amount_in) => {
                        component_depths.insert(key, max_amount_in);
                        succeeded += 1;
                        continue;
                    }
                    DepthSearch::Target(limit_price) => limit_price,
                };

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
    use std::collections::HashMap;

    use num_traits::ToPrimitive;
    use rstest::rstest;
    use tycho_simulation::tycho_common::{
        dto::ProtocolStateDelta,
        simulation::{
            errors::TransitionError,
            protocol_sim::{Balances, GetAmountOutResult},
        },
        Bytes,
    };

    use super::*;

    /// Constant-product pool double with an explicit fee, integer math and no rounding mercy.
    ///
    /// Reproduces the two properties the depth target depends on: `spot_price` follows the
    /// contract-compliant convention of the mid price grossed up by the fee, and
    /// `get_amount_out` floors, so small trades execute far below the pool's real rate.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct FeeCurveSim {
        /// Reserve of the token whose address sorts first.
        reserve_low: BigUint,
        /// Reserve of the token whose address sorts second.
        reserve_high: BigUint,
        /// Swap fee in hundredths of a basis point, so 0.3% is 3_000.
        fee_hundredth_bps: u32,
        /// `get_limits` reports the sell-side reserve divided by this, the way real adapters cap
        /// a swap well below the reserve that backs it.
        limit_divisor: u32,
    }

    const FEE_DENOMINATOR: u32 = 1_000_000;

    impl FeeCurveSim {
        fn new(reserve_low: u128, reserve_high: u128, fee_hundredth_bps: u32) -> Self {
            Self {
                reserve_low: BigUint::from(reserve_low),
                reserve_high: BigUint::from(reserve_high),
                fee_hundredth_bps,
                limit_divisor: 1,
            }
        }

        fn with_limit_divisor(mut self, limit_divisor: u32) -> Self {
            self.limit_divisor = limit_divisor;
            self
        }

        fn reserves_for(&self, token_in: &Token, token_out: &Token) -> (&BigUint, &BigUint) {
            if token_in.address < token_out.address {
                (&self.reserve_low, &self.reserve_high)
            } else {
                (&self.reserve_high, &self.reserve_low)
            }
        }
    }

    #[typetag::serde]
    impl ProtocolSim for FeeCurveSim {
        fn fee(&self) -> f64 {
            self.fee_hundredth_bps as f64 / FEE_DENOMINATOR as f64
        }

        fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
            let (reserve_in, reserve_out) = self.reserves_for(base, quote);
            let mid = reserve_out
                .to_f64()
                .expect("reserve fits f64") /
                reserve_in
                    .to_f64()
                    .expect("reserve fits f64");
            let decimal_scale = 10_f64.powi(base.decimals as i32 - quote.decimals as i32);
            Ok(mid * decimal_scale / (1.0 - self.fee()))
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            let (reserve_in, reserve_out) = self.reserves_for(token_in, token_out);
            let net_in = amount_in * BigUint::from(FEE_DENOMINATOR - self.fee_hundredth_bps);
            let amount_out =
                (&net_in * reserve_out) / (reserve_in * BigUint::from(FEE_DENOMINATOR) + &net_in);
            Ok(GetAmountOutResult::new(amount_out, BigUint::from(100_000u32), self.clone_box()))
        }

        fn get_limits(
            &self,
            sell_token: Bytes,
            buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            let (reserve_in, reserve_out) = if sell_token < buy_token {
                (&self.reserve_low, &self.reserve_high)
            } else {
                (&self.reserve_high, &self.reserve_low)
            };
            Ok((reserve_in / BigUint::from(self.limit_divisor), reserve_out.clone()))
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("delta_transition not implemented in FeeCurveSim")
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
                .is_some_and(|other| {
                    other.reserve_low == self.reserve_low &&
                        other.reserve_high == self.reserve_high &&
                        other.fee_hundredth_bps == self.fee_hundredth_bps &&
                        other.limit_divisor == self.limit_divisor
                })
        }
    }

    /// Pool double that answers a fixed script of probe outcomes, one per power of ten from one
    /// wei up. `None` is an adapter rejecting the probe outright.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct ScriptedProbeSim {
        outputs: Vec<Option<u64>>,
    }

    #[typetag::serde]
    impl ProtocolSim for ScriptedProbeSim {
        fn fee(&self) -> f64 {
            0.0
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            Ok(1.0)
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            _token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            let probe_index = amount_in.to_string().len() - 1;
            match self.outputs.get(probe_index) {
                Some(Some(amount_out)) => Ok(GetAmountOutResult::new(
                    BigUint::from(*amount_out),
                    BigUint::zero(),
                    self.clone_box(),
                )),
                Some(None) => Err(SimulationError::RecoverableError(
                    "InvalidAmountIn: Amount too low".to_string(),
                )),
                None => panic!("probing walked past index {probe_index}, past the pool limit"),
            }
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::from(10u64).pow(self.outputs.len() as u32 - 1), BigUint::from(u64::MAX)))
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("delta_transition not implemented in ScriptedProbeSim")
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
                .is_some_and(|other| other.outputs == self.outputs)
        }
    }

    /// Runs the probing over a script whose probes run from one wei up.
    fn walk(outputs: Vec<Option<u64>>) -> Result<ExecutionStart, SimulationError> {
        let sim = ScriptedProbeSim { outputs };
        let token_in = token_with_decimals(0x01, "IN", 18);
        let token_out = token_with_decimals(0x02, "OUT", 18);
        let (max_amount_in, _) = sim
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("scripted limits");
        probe_execution_start(&sim, &token_in, &token_out, 1.0, &max_amount_in)
    }

    use crate::{
        algorithm::test_utils::{
            setup_market_weighted, token, token_with_decimals, MockProtocolSim, ONE_ETH,
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
        assert_eq!(comp.retained_price_numerator, 990_000_000_000_000_000);
    }

    #[rstest]
    #[case(0.001, 999_000_000_000_000_000)]
    #[case(0.01, 990_000_000_000_000_000)]
    #[case(0.5, 500_000_000_000_000_000)]
    // `1.0 - 0.99` is not exact in binary; the numerator carries that error and nothing more.
    #[case(0.99, 10_000_000_000_000_008)]
    fn new_with_valid_slippage(#[case] threshold: f64, #[case] expected_numerator: u128) {
        let comp = ComponentDepthComputation::new(threshold).unwrap();
        assert_eq!(comp.retained_price_numerator, expected_numerator);
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

    #[rstest]
    #[case::equal_decimals_unit_price(18, 18, 1.0, 0)]
    #[case::equal_decimals_expensive_out(18, 18, 0.0005, 4)]
    #[case::equal_decimals_cheap_out(18, 18, 2000.0, 0)]
    #[case::in_has_more_decimals(18, 6, 2000.0, 9)]
    #[case::out_has_more_decimals(6, 18, 0.0005, 0)]
    #[case::wbtc_style(18, 8, 0.00006, 15)]
    fn test_first_nonzero_output_exponent(
        #[case] decimals_in: u32,
        #[case] decimals_out: u32,
        #[case] spot_price: f64,
        #[case] expected: u32,
    ) {
        assert_eq!(first_nonzero_output_exponent(spot_price, decimals_in, decimals_out), expected);
    }

    #[rstest]
    #[case::zero(0.0)]
    #[case::negative(-1.0)]
    #[case::nan(f64::NAN)]
    #[case::infinite(f64::INFINITY)]
    fn test_first_nonzero_output_exponent_falls_back_to_one_wei(#[case] spot_price: f64) {
        assert_eq!(first_nonzero_output_exponent(spot_price, 18, 6), 0);
    }

    /// Measured prices rise while dust distortion dominates and fall once the real curve takes
    /// over. The probing must take the peak and stop there, not the first or the last probe.
    #[test]
    fn test_probe_execution_start_takes_the_peak() {
        // Prices per probe, from one wei up: nothing, 0.5, 0.8, 0.95, 0.94, 0.00001.
        let start = walk(vec![Some(0), Some(5), Some(80), Some(950), Some(9400), Some(1)])
            .expect("a priced probe exists");

        assert_eq!(start.amount_in, BigUint::from(1_000u32));
        assert_eq!(start.amount_out, BigUint::from(950u32));
    }

    /// A probe that only ties its predecessor is already off the rising branch.
    #[test]
    fn test_probe_execution_start_stops_on_a_flat_probe() {
        let start =
            walk(vec![Some(5), Some(80), Some(800), Some(80_000)]).expect("a priced probe exists");

        assert_eq!(start.amount_in, BigUint::from(10u32));
        assert_eq!(start.amount_out, BigUint::from(80u32));
    }

    #[test]
    fn test_probe_execution_start_steps_over_unpriceable_probes() {
        let start =
            walk(vec![None, Some(0), None, Some(950), Some(9400)]).expect("a priced probe exists");

        assert_eq!(start.amount_in, BigUint::from(1000u32));
        assert_eq!(start.amount_out, BigUint::from(950u32));
    }

    /// A pool that buys nothing at any size must surface as an error, not a depth of zero.
    #[rstest]
    #[case::always_floors_to_zero(vec![Some(0), Some(0), Some(0)])]
    #[case::always_rejected(vec![None, None, None])]
    fn test_probe_execution_start_without_output_is_an_error(#[case] outputs: Vec<Option<u64>>) {
        let error = walk(outputs).expect_err("a pool that buys nothing has no start price");

        assert!(
            matches!(error, SimulationError::RecoverableError(_)),
            "expected a recoverable error, got {error:?}"
        );
    }

    /// The depth target must sit exactly `1 - slippage` below the price the pool starts executing
    /// at, whatever the fee.
    #[rstest]
    #[case::five_hundredths_bp(500)]
    #[case::five_bps(5_000)]
    #[case::thirty_bps(3_000)]
    #[case::one_percent(10_000)]
    #[case::two_percent(20_000)]
    fn test_depth_target_is_fee_independent(#[case] fee_hundredth_bps: u32) {
        let token_in = token_with_decimals(0x01, "IN", 18);
        let token_out = token_with_decimals(0x02, "OUT", 18);
        let sim = FeeCurveSim::new(5_000 * ONE_ETH, 10_000_000 * ONE_ETH, fee_hundredth_bps);
        let computation = ComponentDepthComputation::default();

        let spot_price = sim
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let (max_amount_in, _) = sim
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("limits");
        let start = probe_execution_start(&sim, &token_in, &token_out, spot_price, &max_amount_in)
            .expect("a curved pool prices some probe");

        let search = computation
            .plan_depth_search(&sim, &token_in, &token_out, spot_price)
            .expect("target");
        let DepthSearch::Target(target) = search else {
            panic!("a pool this deep relative to its limit reaches the target inside its range");
        };

        // target / exec_start == 99/100 exactly, with no fee term on either side.
        assert_eq!(
            &target.numerator * &start.amount_in * BigUint::from(100u32),
            &target.denominator * &start.amount_out * BigUint::from(99u32),
            "fee {fee_hundredth_bps}: target is not exactly 1% below the executable start price"
        );

        // The start price clears the target it anchors, at every fee.
        assert!(
            executes_at_or_above(&start.amount_in, &start.amount_out, &target),
            "fee {fee_hundredth_bps}: the pool cannot execute at its own depth target"
        );
    }

    // `>` instead of `>=` in `executes_at_or_above` would search for a depth the component
    // already reaches at its limit.
    #[test]
    fn test_component_priced_exactly_at_the_target_is_capped_at_its_limit() {
        let token_in = token_with_decimals(0x01, "IN", 18);
        let token_out = token_with_decimals(0x02, "OUT", 18);
        let sim = FeeCurveSim::new(5_000 * ONE_ETH, 10_000_000 * ONE_ETH, 3_000)
            .with_limit_divisor(1_000);
        let spot_price = sim
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let (max_amount_in, _) = sim
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("limits");
        let capacity = sim
            .get_amount_out(max_amount_in.clone(), &token_in, &token_out)
            .expect("limit prices");

        // The exact rate at the limit, so the comparison lands on equality rather than near it.
        let target = Price::new(capacity.amount.clone(), max_amount_in.clone());
        assert!(executes_at_or_above(&max_amount_in, &capacity.amount, &target));

        let search = ComponentDepthComputation::default()
            .plan_depth_search(&sim, &token_in, &token_out, spot_price)
            .expect("plan");
        assert!(matches!(search, DepthSearch::LimitedByComponentCapacity(_)), "got {search:?}");
    }

    #[test]
    fn test_pool_that_never_reaches_the_target_is_capped_at_its_limit() {
        let token_in = token_with_decimals(0x01, "IN", 18);
        let token_out = token_with_decimals(0x02, "OUT", 18);
        // A limit one thousandth of the reserve backing it moves the price by about 0.1%, far
        // less than the slippage threshold, so no size inside the range crosses the target.
        let sim = FeeCurveSim::new(5_000 * ONE_ETH, 10_000_000 * ONE_ETH, 3_000)
            .with_limit_divisor(1_000);

        let spot_price = sim
            .spot_price(&token_in, &token_out)
            .expect("spot price");
        let search = ComponentDepthComputation::default()
            .plan_depth_search(&sim, &token_in, &token_out, spot_price)
            .expect("plan");

        let DepthSearch::LimitedByComponentCapacity(depth) = search else {
            panic!("expected the pool's own limit to be the answer, got {search:?}");
        };
        assert_eq!(depth, BigUint::from(5 * ONE_ETH));
    }

    #[tokio::test]
    async fn test_compute_handles_empty_market() {
        let market = MarketData::new_shared();
        let derived = DerivedData::new_shared();
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(SpotPrices::new(), vec![], 0, true);
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
            added: std::collections::HashMap::from([(
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

        // The mock prices every size at the same rate, so each direction is worth what the
        // component lets through, except where its limit truncates to nothing.
        let market_guard = market.read().await;
        let sim_state = market_guard
            .get_simulation_state("component")
            .expect("component simulation state");

        for (sell, buy) in [(&eth, &usdc), (&usdc, &eth)] {
            let key: ComponentDepthKey =
                ("component".into(), sell.address.clone(), buy.address.clone());
            let (sell_limit, _) = sim_state
                .get_limits(sell.address.clone(), buy.address.clone())
                .expect("mock limits");

            if sell_limit.is_zero() {
                assert!(
                    !component_depths.contains_key(&key),
                    "{}→{}: a direction the component cannot trade must not carry a depth",
                    sell.symbol,
                    buy.symbol
                );
                assert!(
                    component_depths_output
                        .failed_items
                        .iter()
                        .any(|item| item.key ==
                            format!("component/{}/{}", sell.address, buy.address)),
                    "{}→{}: an untradeable direction must be recorded as a failure",
                    sell.symbol,
                    buy.symbol
                );
            } else {
                assert_eq!(
                    component_depths.get(&key),
                    Some(&sell_limit),
                    "{}→{}: a pool that never crosses the target is worth its whole limit",
                    sell.symbol,
                    buy.symbol
                );
            }
        }
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

    /// Exercises the Brent solver fallback path with realistic UniV2 component states to verify
    /// it produces sensible depth values. This validates that the Price construction
    /// approach in compute() is correct across a range of real-world token pairs.
    ///
    /// Three components covering the key decimal configurations encountered in production:
    ///   - WETH/USDC: 18/6 decimals, ~$2000 price, ~$10M liquidity
    ///   - WETH/WBTC: 18/8 decimals, ~15 price, ~$5M liquidity
    ///   - USDC/USDT: 6/6 decimals, ~1 price, ~$50M liquidity
    #[test]
    fn test_brent_solver_with_realistic_components() {
        use alloy::primitives::U256;
        use tycho_simulation::evm::{
            protocol::uniswap_v2::state::UniswapV2State, query_pool_swap::query_pool_swap,
        };

        struct ComponentCase {
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
            ComponentCase {
                name: "WETH(18)/USDC(6)",
                token_in: token_with_decimals(0x01, "WETH", 18),
                token_out: token_with_decimals(0x02, "USDC", 6),
                reserve_in_human: 5_000,
                reserve_out_human: 10_000_000,
            },
            ComponentCase {
                name: "WETH(18)/WBTC(8)",
                token_in: token_with_decimals(0x01, "WETH", 18),
                token_out: token_with_decimals(0x02, "WBTC", 8),
                reserve_in_human: 5_000,
                reserve_out_human: 333,
            },
            ComponentCase {
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
        let mut partial_spot = SpotPrices::new();
        let key_eth_usdc = ("component".to_string(), eth.address.clone(), usdc.address.clone());
        partial_spot.insert(key_eth_usdc, 2000.0);
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(partial_spot, vec![], 0, true);

        let changed = ChangedComponents {
            added: std::collections::HashMap::from([(
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
            .set_spot_prices(SpotPrices::new(), vec![], 0, true);

        let changed = ChangedComponents {
            added: std::collections::HashMap::from([(
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
            added: std::collections::HashMap::from([(
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
