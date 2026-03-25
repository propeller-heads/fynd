//! Spot price computation.
//!
//! Computes spot prices for all pools in both directions using `ProtocolSim::spot_price()`.
//! Spot prices are the instantaneous exchange rates without slippage.
//!
//! `ProtocolSim::spot_price()` is cheap for all pool types: VM pools return a pre-computed
//! HashMap lookup, native pools do simple arithmetic. This means the read lock hold time is
//! negligible (microseconds per pool), so we hold it for the full loop rather than paying the
//! cost of cloning simulation states via `extract_subset()`.

use async_trait::async_trait;
use itertools::Itertools;
use tracing::{debug, instrument, warn, Span};

use crate::{
    derived::{
        computation::{
            ComputationId, ComputationOutput, DerivedComputation, FailedItem, FailedItemError,
        },
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
        // Start with existing prices (or empty for full recompute).
        let mut spot_prices = if changed.is_full_recompute {
            SpotPrices::new()
        } else {
            let mut existing_prices = store
                .read()
                .await
                .spot_prices()
                .cloned()
                .unwrap_or_default();
            // Remove spot prices for removed components.
            for component_id in &changed.removed {
                existing_prices.retain(|key, _| &key.0 != component_id);
            }
            existing_prices
        };

        let market_guard = market.read().await;
        let topology = market_guard.component_topology();
        let tokens = market_guard.token_registry_ref();

        // Determine which components need (re)computation.
        let components_to_compute: Vec<_> = if changed.is_full_recompute {
            topology.keys().cloned().collect()
        } else {
            changed
                .added
                .keys()
                .chain(changed.updated.iter())
                .cloned()
                .collect()
        };
        let num_components_to_compute = components_to_compute.len();

        let mut succeeded = 0usize;
        let mut failed_items: Vec<FailedItem> = Vec::new();
        let num_components_to_compute = components_to_compute.len();

        for component_id in &components_to_compute {
            // Get token addresses: changed.added for new components, topology for existing.
            let token_addresses = changed
                .added
                .get(component_id)
                .or_else(|| topology.get(component_id));

            let Some(token_addresses) = token_addresses else {
                continue;
            };

            let Some(sim_state) = market_guard.get_simulation_state(component_id) else {
                warn!(component_id, "missing simulation state, skipping pool");
                spot_prices.retain(|key, _| &key.0 != component_id);
                for perm in token_addresses.iter().permutations(2) {
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, perm[0], perm[1]),
                        error: FailedItemError::MissingSimulationState,
                    });
                }
                continue;
            };

            let pool_tokens: Result<Vec<_>, _> = token_addresses
                .iter()
                .map(|addr| tokens.get(addr).ok_or(addr))
                .collect();
            let Ok(pool_tokens) = pool_tokens else {
                warn!(component_id, "missing token metadata, skipping pool");
                spot_prices.retain(|key, _| &key.0 != component_id);
                for perm in token_addresses.iter().permutations(2) {
                    failed_items.push(FailedItem {
                        key: format!("{}/{}/{}", component_id, perm[0], perm[1]),
                        error: FailedItemError::MissingTokenMetadata,
                    });
                }
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
                        failed_items.push(FailedItem {
                            key: format!(
                                "{}/{}/{}",
                                component_id, token_in.address, token_out.address
                            ),
                            error: FailedItemError::SimulationFailed(e.to_string()),
                        });
                    }
                }
            }
        }

        drop(market_guard);

        debug!(
            succeeded,
            failed = failed_items.len(),
            total = spot_prices.len(),
            "spot price computation complete"
        );
        Span::current().record("updated_spot_prices", spot_prices.len());

        // Return error if all calculations failed for a full recompute.
        // Partial (incremental) computations can fail for a small subset of components.
        if changed.is_full_recompute && succeeded == 0 && num_components_to_compute > 0 {
            return Err(ComputationError::TotalFailure {
                computation_id: Self::ID,
                attempted: num_components_to_compute,
            });
        }

        Ok(ComputationOutput::with_failures(spot_prices, failed_items))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, setup_market, token, MockProtocolSim},
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

        assert!(output.data.is_empty());
    }

    #[tokio::test]
    async fn partial_failure_yields_ok_with_failed_items() {
        // pool1: has sim state → spot prices computed
        // pool2: no sim state → FailedItem
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");
        let dai = token(0x03, "DAI");

        let (market, _) = setup_market(vec![("pool1", &eth, &usdc, MockProtocolSim::new(2000.0))]);

        // Add pool2 without sim state
        {
            let mut m = market.write().await;
            let pool2 = component("pool2", &[eth.clone(), dai.clone()]);
            m.upsert_components(std::iter::once(pool2));
            m.upsert_tokens([dai.clone()]);
        }

        let derived = DerivedData::new_shared();
        let changed = ChangedComponents { is_full_recompute: true, ..Default::default() };

        let output = SpotPriceComputation::new()
            .compute(&market, &derived, &changed)
            .await
            .expect("should not be total failure since pool1 succeeds");

        assert!(output.has_failures(), "pool2 missing sim state should produce a failed item");

        let key_eth_usdc = ("pool1".to_string(), eth.address.clone(), usdc.address.clone());
        let key_usdc_eth = ("pool1".to_string(), usdc.address.clone(), eth.address.clone());
        assert!(output.data.contains_key(&key_eth_usdc), "ETH→USDC price should be present");
        assert!(output.data.contains_key(&key_usdc_eth), "USDC→ETH price should be present");

        // Component-level failures are expanded to pair-level keys
        let key_eth_dai = format!("pool2/{}/{}", eth.address, dai.address);
        let key_dai_eth = format!("pool2/{}/{}", dai.address, eth.address);
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == key_eth_dai || item.key == key_dai_eth),
            "pool2 pair keys should appear in failed_items"
        );
    }

    /// MockProtocolSim spot_price behavior:
    /// - When base < quote: returns spot_price
    /// - When base > quote: returns 1/spot_price
    #[rstest]
    #[case::low_to_high(0x01, 0x02, 2.0, 2.0)]
    #[case::high_to_low(0x02, 0x01, 2.0, 0.5)]
    #[case::spot_price_4_low_to_high(0x01, 0x02, 4.0, 4.0)]
    #[case::spot_price_4_high_to_low(0x02, 0x01, 4.0, 0.25)]
    fn spot_price_direction(
        #[case] in_addr: u8,
        #[case] out_addr: u8,
        #[case] mock_spot_price: f64,
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
