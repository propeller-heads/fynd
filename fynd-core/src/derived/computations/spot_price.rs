//! Spot price computation.
//!
//! Computes spot prices for all pools in both directions using `ProtocolSim::spot_price()`.
//! Spot prices are the instantaneous exchange rates without slippage.

use async_trait::async_trait;
use itertools::Itertools;
use tracing::{debug, instrument, warn, Span};

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        types::SpotPrices,
    },
    feed::market_data::SharedMarketDataRef,
};

/// Computes spot prices for all pools.
///
/// For each pool with tokens A and B, computes:
/// - Spot price A -> B
/// - Spot price B -> A
///
/// Uses `ProtocolSim::spot_price()` to compute the instantaneous exchange rate.
#[derive(Debug, Default)]
pub struct SpotPriceComputation;

impl SpotPriceComputation {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DerivedComputation for SpotPriceComputation {
    type Output = SpotPrices;

    const ID: ComputationId = "spot_prices";

    #[instrument(level = "debug", skip(market, store, changed), fields(computation_id = Self::ID, updated_spot_prices))]
    async fn compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Self::Output, ComputationError> {
        let market = market.read().await;

        // Start with existing prices (or empty for full recompute)
        let mut spot_prices = if changed.is_full_recompute {
            SpotPrices::new()
        } else {
            let mut existing_prices = store
                .read()
                .await
                .spot_prices()
                .cloned()
                .unwrap_or_default();
            // Remove spot prices for removed components
            for component_id in &changed.removed {
                existing_prices.retain(|key, _| &key.0 != component_id);
            }
            existing_prices
        };

        let topology = market.component_topology();
        let tokens = market.token_registry_ref();

        // Determine which components to compute
        let components_to_compute: Vec<_> = if changed.is_full_recompute {
            topology.keys().collect()
        } else {
            changed
                .added
                .keys()
                .chain(changed.updated.iter())
                .collect()
        };

        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let num_components_to_compute = components_to_compute.len();

        for component_id in components_to_compute {
            // Get token addresses: changed.added for new components, topology for existing
            let token_addresses = changed
                .added
                .get(component_id)
                .or_else(|| topology.get(component_id));

            let Some(token_addresses) = token_addresses else {
                continue; // Component might have been removed in the meantime
            };

            let Some(sim_state) = market.get_simulation_state(component_id) else {
                warn!(component_id, "missing simulation state, skipping pool");
                spot_prices.retain(|key, _| &key.0 != component_id);
                continue;
            };

            let pool_tokens: Result<Vec<_>, _> = token_addresses
                .iter()
                .map(|addr| tokens.get(addr).ok_or(addr))
                .collect();
            let Ok(pool_tokens) = pool_tokens else {
                warn!(component_id, "missing token metadata, skipping pool");
                spot_prices.retain(|key, _| &key.0 != component_id);
                continue;
            };

            for perm in pool_tokens.iter().permutations(2) {
                let (token_in, token_out) = (*perm[0], *perm[1]);
                let key =
                    (component_id.clone(), token_in.address.clone(), token_out.address.clone());

                match sim_state.spot_price(token_in, token_out) {
                    Ok(price) => {
                        spot_prices.insert(key, price);
                        succeeded += 1;
                    }
                    Err(e) => {
                        warn!(
                            component_id,
                            token_in = %token_in.address,
                            token_out = %token_out.address,
                            error = %e,
                            "spot price failed, skipping pair"
                        );
                        spot_prices.remove(&key);
                        failed += 1;
                    }
                }
            }
        }

        debug!(succeeded, failed, total = spot_prices.len(), "spot price computation complete");
        Span::current().record("updated_spot_prices", spot_prices.len());

        // Return error if all calculations failed for a full recompute. Partial computations can
        // fail for a small subset of components.
        if changed.is_full_recompute && succeeded == 0 && num_components_to_compute > 0 {
            return Err(ComputationError::TotalFailure {
                computation_id: Self::ID,
                attempted: num_components_to_compute,
            });
        }

        Ok(spot_prices)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::{
        algorithm::test_utils::{token, MockProtocolSim},
        derived::store::DerivedData,
        feed::market_data::SharedMarketData,
    };

    #[test]
    fn computation_id() {
        assert_eq!(SpotPriceComputation::ID, "spot_prices");
    }

    #[tokio::test]
    async fn handles_empty_market() {
        let market_ref = SharedMarketData::new_shared();
        let derived_ref = DerivedData::new_shared();
        let changed = ChangedComponents::default();

        let output = SpotPriceComputation::new()
            .compute(&market_ref, &derived_ref, &changed)
            .await
            .unwrap();

        assert!(output.is_empty());
    }

    /// MockProtocolSim spot_price behavior:
    /// - When base < quote: returns 1/spot_price
    /// - When base > quote: returns spot_price
    #[rstest]
    #[case::low_to_high(0x01, 0x02, 2, 0.5)]
    #[case::high_to_low(0x02, 0x01, 2, 2.0)]
    #[case::spot_price_4_low_to_high(0x01, 0x02, 4, 0.25)]
    #[case::spot_price_4_high_to_low(0x02, 0x01, 4, 4.0)]
    fn spot_price_direction(
        #[case] in_addr: u8,
        #[case] out_addr: u8,
        #[case] mock_spot_price: u32,
        #[case] expected_price: f64,
    ) {
        let token_in = token(in_addr, "IN");
        let token_out = token(out_addr, "OUT");
        let sim = MockProtocolSim::new(mock_spot_price);

        let price = sim
            .spot_price(&token_in, &token_out)
            .unwrap();

        assert!(
            (price - expected_price).abs() < f64::EPSILON,
            "mock_spot_price={mock_spot_price}, in={in_addr:#x}, out={out_addr:#x}: got {price}, expected {expected_price}"
        );
    }
}
