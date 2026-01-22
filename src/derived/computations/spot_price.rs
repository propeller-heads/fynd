//! Spot price computation.
//!
//! Computes spot prices for all pools in both directions using `ProtocolSim::spot_price()`.
//! Spot prices are the instantaneous exchange rates without slippage.

use std::collections::HashMap;

use tracing::{instrument, Span};
use tycho_simulation::tycho_common::models::Address;

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        error::ComputationError,
        store::DerivedDataStore,
    },
    feed::market_data::SharedMarketData,
    types::ComponentId,
};

/// Key for spot price lookups: (component_id, token_in, token_out).
pub type SpotPriceKey = (ComponentId, Address, Address);

/// Spot prices map: key → spot price as f64.
/// Represents: 1 token_in = spot_price token_out.
pub type SpotPrices = HashMap<SpotPriceKey, f64>;

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

impl DerivedComputation for SpotPriceComputation {
    type Output = SpotPrices;

    const ID: ComputationId = "spot_prices";

    #[instrument(level = "debug", skip(market, _store), fields(computation_id = Self::ID, updated_spot_prices))]
    fn compute(
        &self,
        market: &SharedMarketData,
        _store: &DerivedDataStore,
    ) -> Result<Self::Output, ComputationError> {
        let mut spot_prices = SpotPrices::new();

        let topology = market.component_topology();
        let tokens = market.token_registry_ref();

        for (component_id, token_addresses) in topology.iter() {
            let sim_state = market
                .get_simulation_state(component_id)
                .ok_or(ComputationError::InvalidDependencyData {
                    dependency: "simulation_states",
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

                    match sim_state.spot_price(token_out, token_in) {
                        Ok(price) => {
                            let key = (
                                component_id.clone(),
                                token_in.address.clone(),
                                token_out.address.clone(),
                            );
                            spot_prices.insert(key, price);
                        }
                        Err(e) => {
                            Err(ComputationError::SimulationFailed(format!(
                                "failed to compute spot price for pool {component_id} \
                                 {}/{}: {e}",
                                token_in.address, token_out.address
                            )))?;
                        }
                    }
                }
            }
        }

        Span::current().record("updated_spot_prices", spot_prices.len());

        Ok(spot_prices)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::algorithm::test_utils::{token, MockProtocolSim};

    #[test]
    fn computation_id() {
        assert_eq!(SpotPriceComputation::ID, "spot_prices");
    }

    #[test]
    fn handles_empty_market() {
        let market = SharedMarketData::new();
        let store = DerivedDataStore::new();

        let output = SpotPriceComputation::new()
            .compute(&market, &store)
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
