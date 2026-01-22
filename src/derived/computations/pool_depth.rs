//! Pool depth computation.
//!
//! Computes liquidity depths for all pools using binary search with `get_amount_out`.
//! Depth represents the maximum input amount before reaching the configured slippage
//! threshold from the spot price.

use std::collections::HashMap;

use num_bigint::BigUint;
use num_traits::{One, ToPrimitive, Zero};
use tracing::{trace, warn};
use tycho_simulation::tycho_common::{
    models::{token::Token, Address},
    simulation::protocol_sim::ProtocolSim,
};

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        error::ComputationError,
        store::DerivedDataStore,
    },
    feed::market_data::SharedMarketData,
    types::ComponentId,
};

/// Maximum iterations for binary search to prevent infinite loops.
const MAX_BINARY_SEARCH_ITERATIONS: u32 = 64;

/// Key for pool depth lookups: (component_id, token_in, token_out).
pub type PoolDepthKey = (ComponentId, Address, Address);

/// Pool depths map: key → maximum input amount at configured slippage threshold.
pub type PoolDepths = HashMap<PoolDepthKey, BigUint>;

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
    /// Uses `get_limits()` for the upper bound.
    ///
    /// # Behavior
    /// - Simulation errors indicate we're outside valid range → adjust bounds accordingly
    /// - Conversion errors (to_f64) are unexpected → terminate with error
    fn find_depth_binary_search(
        &self,
        sim_state: &dyn ProtocolSim,
        token_in: &Token,
        token_out: &Token,
        spot_price: f64,
    ) -> Result<BigUint, ComputationError> {
        let (max_input, _) = sim_state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .map_err(|e| ComputationError::SimulationFailed(format!("get_limits failed: {e:?}")))?;

        if max_input.is_zero() {
            return Ok(BigUint::zero());
        }

        let min_acceptable_price = spot_price * (1.0 - self.slippage_threshold);

        let mut low = BigUint::one();
        let mut high = max_input;
        let mut best_valid = BigUint::zero();

        for _ in 0..MAX_BINARY_SEARCH_ITERATIONS {
            if high <= &low + BigUint::one() {
                break;
            }

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

                    if effective_price >= min_acceptable_price {
                        best_valid = mid.clone();
                        low = mid;
                    } else {
                        high = mid;
                    }
                }
                Err(_) => {
                    // Simulation failed - below protocol minimum as we maintain an invariant that
                    // the `high` should always succeed at simulation.
                    low = mid;
                }
            }
        }

        Ok(best_valid)
    }
}

impl DerivedComputation for PoolDepthComputation {
    type Output = PoolDepths;

    const ID: ComputationId = "pool_depths";

    fn compute(
        &self,
        market: &SharedMarketData,
        _store: &DerivedDataStore,
    ) -> Result<Self::Output, ComputationError> {
        let mut pool_depths = PoolDepths::new();
        let mut error_count = 0usize;

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

            for (i, token_in) in pool_tokens.iter().enumerate() {
                for (j, token_out) in pool_tokens.iter().enumerate() {
                    if i == j {
                        continue;
                    }

                    let spot_price = match sim_state.spot_price(token_in, token_out) {
                        Ok(p) if p.is_finite() && p > 0.0 => p,
                        _ => {
                            error_count += 1;
                            continue;
                        }
                    };

                    match self.find_depth_binary_search(sim_state, token_in, token_out, spot_price)
                    {
                        Ok(depth) if !depth.is_zero() => {
                            let key = (
                                component_id.clone(),
                                token_in.address.clone(),
                                token_out.address.clone(),
                            );
                            pool_depths.insert(key, depth);
                        }
                        Ok(_) => {
                            trace!(component_id = %component_id, "zero depth");
                        }
                        Err(e) => {
                            trace!(component_id = %component_id, error = ?e, "depth failed");
                            error_count += 1;
                        }
                    }
                }
            }
        }

        if pool_depths.is_empty() && error_count > 0 {
            warn!(error_count, "pool depth computation failed for all pairs");
            return Err(ComputationError::NoValidResult {
                reason: format!("pool depth computation failed for all {error_count} pairs"),
            });
        }

        trace!(count = pool_depths.len(), "computed pool depths");
        Ok(pool_depths)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::algorithm::test_utils::{token, MockProtocolSim};

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

    #[test]
    fn handles_empty_market() {
        let market = SharedMarketData::new();
        let store = DerivedDataStore::new();

        let output = PoolDepthComputation::default()
            .compute(&market, &store)
            .unwrap();

        assert!(output.is_empty());
    }

    /// MockProtocolSim has constant price (no slippage), so depth equals sell_limit - 1.
    #[rstest]
    #[case::normal(2, 1_000_000, 499_999)]
    #[case::zero_for_zero_liquidity(2, 0, 0)]
    fn binary_search_finds_exact_depth_for_constant_price_pool(
        #[case] spot_price: u32,
        #[case] liquidity: u64,
        #[case] expected_depth: u64,
    ) {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let sim = MockProtocolSim::new(spot_price).with_liquidity(liquidity);
        let comp = PoolDepthComputation::default();
        let spot = sim
            .spot_price(&token_a, &token_b)
            .unwrap();

        let depth = comp
            .find_depth_binary_search(&sim, &token_a, &token_b, spot)
            .unwrap();

        assert_eq!(
            depth,
            BigUint::from(expected_depth),
            "expected depth {expected_depth} for spot_price={spot_price}, liquidity={liquidity}"
        );
    }
}
