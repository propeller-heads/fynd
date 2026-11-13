//! Spot price computation.
//!
//! Computes spot prices for every swap direction of every component using
//! `ProtocolSim::spot_price()`.
//! Spot prices are the instantaneous exchange rates without slippage.
//!
//! `ProtocolSim::spot_price()` is cheap for all component types: VM components return a
//! pre-computed HashMap lookup, native components do simple arithmetic. This means the read lock
//! hold time is negligible (microseconds per component), so we hold it for the full loop rather
//! than paying the cost of cloning simulation states via `extract_subset()`.

use async_trait::async_trait;
use tracing::{debug, instrument, warn, Span};

use crate::{
    derived::{
        computation::{
            ComputationId, ComputationOutput, DerivedComputation, FailedItem, FailedItemError,
        },
        computations::{components_without_entries, drop_unlisted_directions},
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        store::DerivedData,
        types::SpotPrices,
    },
    feed::market_data::MarketData,
    nexus::{
        client::{NexusClient, NexusDerivedClient},
        derived::SpotPriceSnapshot,
    },
};

/// Computes spot prices for all components.
///
/// Computes the spot price for each swap direction of a component.
///
/// Uses `ProtocolSim::spot_price()` to compute the instantaneous exchange rate.
///
/// With a nexus client it loads the spot prices from `fynd-nexus` instead.
#[derive(Debug, Default)]
pub struct SpotPriceComputation {
    nexus: Option<NexusDerivedClient<SpotPrices>>,
}

impl SpotPriceComputation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the spot prices from the `fynd-nexus` service behind `client` instead of computing
    /// them.
    pub fn with_nexus_client(self, client: NexusClient) -> Self {
        Self { nexus: Some(NexusDerivedClient::new(client, Self::ID, true)) }
    }
}

#[async_trait]
impl DerivedComputation for SpotPriceComputation {
    type Output = SpotPrices;

    const ID: ComputationId = "spot_prices";

    fn persist(store: &mut DerivedData, output: ComputationOutput<Self::Output>, block: u64) {
        store.set_spot_prices(output.data, output.failed_items, block);
    }

    #[instrument(level = "debug", skip(market, store, changed), fields(computation_id = Self::ID, updated_spot_prices))]
    async fn compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
        if let Some(nexus) = &self.nexus {
            let prices = nexus
                .get_latest_snapshot(
                    market,
                    SpotPriceSnapshot::into_spot_prices,
                    |market, prices| {
                        components_without_entries(market, prices.keys().map(|key| key.0.as_str()))
                    },
                )
                .await?;
            return Ok(ComputationOutput::success(prices));
        }
        self.compute_locally(market, read_stored_spot_prices(store).await, changed)
            .await
    }
}

/// The stored spot prices, or `None` when none were stored yet.
async fn read_stored_spot_prices(store: &SharedDerivedDataRef) -> Option<SpotPrices> {
    store
        .read()
        .await
        .spot_prices()
        .cloned()
}

impl SpotPriceComputation {
    /// Recomputes the spot prices of the `changed` components on top of `stored`, or from nothing
    /// when `stored` is `None`.
    async fn compute_locally(
        &self,
        market: &MarketData,
        stored: Option<SpotPrices>,
        changed: &ChangedComponents,
    ) -> Result<ComputationOutput<SpotPrices>, ComputationError> {
        let first_run = stored.is_none();
        let mut spot_prices = stored.unwrap_or_default();
        // Remove spot prices for removed components.
        for component_id in &changed.removed {
            spot_prices.retain(|key, _| &key.0 != component_id);
        }

        let market_guard = market.read().await;
        let tokens = market_guard.token_registry_ref();

        // Determine which components need (re)computation.
        let components_to_compute: Vec<_> = changed
            .added
            .keys()
            .chain(changed.updated.iter())
            .cloned()
            .collect();

        drop_unlisted_directions(&mut spot_prices, &changed.added);

        let mut succeeded = 0usize;
        let mut failed_items: Vec<FailedItem> = Vec::new();

        for component_id in &components_to_compute {
            let directions = match changed.added.get(component_id) {
                Some(component_topology) => Some(&component_topology.swap_directions[..]),
                None => market_guard.swap_directions_of(component_id),
            };
            let Some(directions) = directions else {
                continue;
            };

            let Some(sim_state) = market_guard.get_simulation_state(component_id) else {
                warn!(component_id, "missing simulation state, skipping component");
                spot_prices.retain(|key, _| &key.0 != component_id);
                for (token_in, token_out) in directions.iter() {
                    failed_items.push(FailedItem {
                        key: format!("{component_id}/{token_in}/{token_out}"),
                        error: FailedItemError::MissingSimulationState,
                    });
                }
                continue;
            };

            for (token_in_address, token_out_address) in directions.iter() {
                let key =
                    (component_id.clone(), token_in_address.clone(), token_out_address.clone());
                let (Some(token_in), Some(token_out)) =
                    (tokens.get(token_in_address), tokens.get(token_out_address))
                else {
                    warn!(
                        component_id,
                        %token_in_address,
                        %token_out_address,
                        "missing token metadata, skipping swap direction"
                    );
                    spot_prices.remove(&key);
                    failed_items.push(FailedItem {
                        key: format!("{component_id}/{token_in_address}/{token_out_address}"),
                        error: FailedItemError::MissingTokenMetadata,
                    });
                    continue;
                };

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

        // Still returns Ok, so workers can become ready with no spot prices; this warning is the
        // only signal.
        if first_run && succeeded == 0 && !components_to_compute.is_empty() {
            warn!(
                attempted = components_to_compute.len(),
                failed = failed_items.len(),
                "first spot price run priced no component"
            );
        }

        Ok(ComputationOutput::with_failures(spot_prices, failed_items))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            component, component_with_directions, setup_market_weighted, token, MockProtocolSim,
        },
        derived::store::DerivedData,
        feed::market_data::MarketData,
    };

    #[test]
    fn computation_id() {
        assert_eq!(SpotPriceComputation::ID, "spot_prices");
    }

    #[tokio::test]
    async fn handles_empty_market() {
        let market_ref = MarketData::new_shared();
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
        // component1: has sim state → spot prices computed
        // component2: no sim state → FailedItem
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");
        let dai = token(0x03, "DAI");

        let (market, _) =
            setup_market_weighted(vec![("component1", &eth, &usdc, MockProtocolSim::new(2000.0))]);

        // Add component2 without sim state
        {
            let mut m = market.write().await;
            let component2 = component("component2", &[eth.clone(), dai.clone()]);
            m.upsert_components(std::iter::once(component2));
            m.upsert_tokens([dai.clone()]);
        }

        let derived = DerivedData::new_shared();
        let changed = ChangedComponents {
            updated: vec!["component1".to_string(), "component2".to_string()],
            ..Default::default()
        };

        let output = SpotPriceComputation::new()
            .compute(&market, &derived, &changed)
            .await
            .expect("a partial failure still returns Ok");

        assert!(output.has_failures(), "component2 missing sim state should produce a failed item");

        let key_eth_usdc = ("component1".to_string(), eth.address.clone(), usdc.address.clone());
        let key_usdc_eth = ("component1".to_string(), usdc.address.clone(), eth.address.clone());
        assert!(output.data.contains_key(&key_eth_usdc), "ETH→USDC price should be present");
        assert!(output.data.contains_key(&key_usdc_eth), "USDC→ETH price should be present");

        // Component-level failures are expanded to pair-level keys
        let key_eth_dai = format!("component2/{}/{}", eth.address, dai.address);
        let key_dai_eth = format!("component2/{}/{}", dai.address, eth.address);
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == key_eth_dai || item.key == key_dai_eth),
            "component2 pair keys should appear in failed_items"
        );
    }

    #[tokio::test]
    async fn test_compute_from_stored_prices() {
        let eth = token(0x01, "ETH");
        let usdc = token(0x02, "USDC");
        let kept = ("kept".to_string(), eth.address.clone(), usdc.address.clone());
        let gone = ("gone".to_string(), eth.address.clone(), usdc.address.clone());
        let derived = DerivedData::new_shared();
        derived
            .try_write()
            .unwrap()
            .set_spot_prices(
                SpotPrices::from_iter([(kept.clone(), 42.0), (gone.clone(), 7.0)]),
                vec![],
                1,
            );
        let changed = ChangedComponents { removed: vec!["gone".to_string()], ..Default::default() };

        let output = SpotPriceComputation::new()
            .compute(&MarketData::new_shared(), &derived, &changed)
            .await
            .unwrap();

        assert_eq!(output.data, SpotPrices::from_iter([(kept, 42.0)]));
    }

    /// A venue of three tokens lists one direction, ETH to USDC. It arrives again after it
    /// dropped the reverse direction, whose price the store still holds.
    #[tokio::test]
    async fn test_compute_listed_directions() {
        let (eth, usdc, dai) = (token(0x01, "ETH"), token(0x02, "USDC"), token(0x03, "DAI"));
        let (market, _) =
            setup_market_weighted(vec![("venue", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        {
            let mut guard = market.write().await;
            guard.upsert_tokens([dai.clone()]);
            guard.upsert_components([component_with_directions(
                "venue",
                &[eth.clone(), usdc.clone(), dai],
                &[(&eth, &usdc)],
            )]);
        }
        let dropped = ("venue".to_string(), usdc.address.clone(), eth.address.clone());
        let derived = DerivedData::new_shared();
        derived.write().await.set_spot_prices(
            SpotPrices::from_iter([(dropped, 0.0005)]),
            vec![],
            0,
        );
        let topology = market.read().await.component_topology()["venue"].clone();
        let changed = ChangedComponents {
            added: FxHashMap::from_iter([("venue".to_string(), topology)]),
            ..Default::default()
        };

        let output = SpotPriceComputation::new()
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        let listed = ("venue".to_string(), eth.address.clone(), usdc.address.clone());
        assert_eq!(output.data.keys().collect::<Vec<_>>(), vec![&listed]);
        assert!(!output.has_failures());
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
