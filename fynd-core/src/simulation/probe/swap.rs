//! The swap a probe sends: one component, one direction, an amount and the output it predicts.

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::tycho_common::{
    models::{token::Token, Address},
    simulation::protocol_sim::ProtocolSim,
};

use super::sizing::{ProbeSizer, SizingError};
use crate::{
    algorithm::sim_guard::GuardedProtocolSim,
    derived::DerivedData,
    fallback::is_pamm,
    feed::{
        exclusivity::is_exclusive, market_data::MarketState, protocol_registry::is_rfq_protocol,
    },
    types::SwapDirection,
    BlockInfo, Route, Swap,
};

/// Why no probe swap could be planned for a component.
///
/// None of these says anything about the component's health: the market or the derived data
/// cannot support a probe right now, or the component is of a kind that is not probed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbePlanError {
    /// The market holds no such component.
    #[error("component {0} is not in the market")]
    UnknownComponent(String),
    /// The component's venue is not executed on chain by the Tycho router alone: an RFQ venue, a
    /// pAMM, or exclusive liquidity.
    #[error("component {component_id} of {protocol_system} cannot be probed on chain")]
    NotProbeable {
        /// The component.
        component_id: String,
        /// Its protocol system.
        protocol_system: String,
    },
    /// Tycho reports the component's protocol out of sync, so its state is not at the block.
    #[error("protocol {0} is out of sync, so its state is not at the latest block")]
    OutOfSync(String),
    /// The market holds no simulation state for the component.
    #[error("component {0} has no simulation state")]
    NoState(String),
    /// The market has applied no block yet.
    #[error("the market has not applied a block yet")]
    NoBlock,
    /// No direction of the component could be sized and priced. The message is the reason of the
    /// last direction tried.
    #[error("no swap direction of component {component_id} can be probed: {reason}")]
    NoDirection {
        /// The component.
        component_id: String,
        /// Why the last direction failed.
        reason: String,
    },
}

/// One swap through one component, with the output its state predicts.
///
/// Planned from the market by [`ProbeSwap::plan`], and sent through [`ComponentProbe`]. It owns a
/// copy of the component's state, so it can be probed after the market lock is released.
///
/// [`ComponentProbe`]: crate::simulation::probe::ComponentProbe
pub struct ProbeSwap {
    pub(super) swap: Swap,
    pub(super) tokens: [Token; 2],
    pub(super) block: BlockInfo,
    pub(super) state_label: String,
    pub(super) gas_price: Option<BigUint>,
}

impl ProbeSwap {
    /// Plans a probe through `component_id` from the current market.
    ///
    /// Takes the component's swap directions in turn, starting at index `rotation` modulo their
    /// number, and uses the first one that `sizer` can size and whose state predicts a non-zero
    /// output. Pass the number of times the component has been probed as `rotation`, so that
    /// repeated probes cover every token of a component with more than two.
    ///
    /// The expected output is `get_amount_out` on the component's state in `market`, which is
    /// at `market.last_updated()`. The probe is simulated on that block.
    ///
    /// # Errors
    ///
    /// A [`ProbePlanError`] when the component is unknown, not probeable, out of sync or without
    /// state, when the market has no block, or when no direction can be planned.
    pub fn plan(
        market: &MarketState,
        derived: &DerivedData,
        sizer: &ProbeSizer,
        component_id: &str,
        rotation: usize,
    ) -> Result<Self, ProbePlanError> {
        let component = market
            .get_component(component_id)
            .ok_or_else(|| ProbePlanError::UnknownComponent(component_id.to_string()))?;
        let protocol_system = component.protocol_system.clone();
        if is_rfq_protocol(&protocol_system) || is_pamm(component) || is_exclusive(component) {
            return Err(ProbePlanError::NotProbeable {
                component_id: component_id.to_string(),
                protocol_system,
            });
        }
        if market.is_unsynced_pool(component_id) {
            return Err(ProbePlanError::OutOfSync(protocol_system));
        }
        let state = market
            .get_simulation_state(component_id)
            .ok_or_else(|| ProbePlanError::NoState(component_id.to_string()))?;
        let block = market
            .last_updated()
            .cloned()
            .ok_or(ProbePlanError::NoBlock)?;
        let directions = market
            .swap_directions_of(component_id)
            .unwrap_or_default();

        let mut last_reason = "the component offers no swap direction".to_string();
        for offset in 0..directions.len() {
            let direction = &directions[(rotation % directions.len() + offset) % directions.len()];
            match Self::plan_direction(market, derived, sizer, component_id, state, direction) {
                Ok((swap, tokens)) => {
                    return Ok(Self {
                        swap,
                        tokens,
                        block,
                        state_label: market.label().clone(),
                        gas_price: market
                            .gas_price()
                            .map(|price| price.effective_gas_price()),
                    });
                }
                Err(reason) => last_reason = reason,
            }
        }
        Err(ProbePlanError::NoDirection {
            component_id: component_id.to_string(),
            reason: last_reason,
        })
    }

    /// Sizes and prices one direction. The error is the reason, for the caller to report.
    fn plan_direction(
        market: &MarketState,
        derived: &DerivedData,
        sizer: &ProbeSizer,
        component_id: &str,
        state: &dyn ProtocolSim,
        (token_in, token_out): &SwapDirection,
    ) -> Result<(Swap, [Token; 2]), String> {
        let (Some(token_in_data), Some(token_out_data)) =
            (market.get_token(token_in), market.get_token(token_out))
        else {
            return Err(format!("a token of {token_in} -> {token_out} is not in the market"));
        };
        let amount_in = sizer
            .amount_in(market, derived, component_id, token_in, token_out)
            .map_err(|error: SizingError| error.to_string())?;
        let result = state
            .get_amount_out_guarded(amount_in.clone(), token_in_data, token_out_data)
            .map_err(|error| format!("{token_in} -> {token_out} cannot be priced: {error}"))?;
        if result.amount.is_zero() {
            return Err(format!("{token_in} -> {token_out} predicts no output"));
        }
        let component = market
            .get_component(component_id)
            .ok_or_else(|| format!("component {component_id} left the market"))?;
        let swap = Swap::new(
            component_id.to_string(),
            component.protocol_system.clone(),
            token_in.clone(),
            token_out.clone(),
            amount_in,
            result.amount,
            result.gas,
            component.clone(),
            state.clone_box(),
        );
        Ok((swap, [token_in_data.clone(), token_out_data.clone()]))
    }

    /// The component the swap goes through.
    pub fn component_id(&self) -> &str {
        self.swap.component_id()
    }

    /// The protocol system of the component.
    pub fn protocol_system(&self) -> &str {
        self.swap.protocol()
    }

    /// The token sold.
    pub fn token_in(&self) -> &Address {
        self.swap.token_in()
    }

    /// The token bought.
    pub fn token_out(&self) -> &Address {
        self.swap.token_out()
    }

    /// The amount of the token sold, in raw units.
    pub fn amount_in(&self) -> &BigUint {
        self.swap.amount_in()
    }

    /// The amount of the token bought that the component's state predicts, in raw units.
    pub fn expected_out(&self) -> &BigUint {
        self.swap.amount_out()
    }

    /// The block the state belongs to, and so the block the probe is simulated on.
    pub fn block(&self) -> &BlockInfo {
        &self.block
    }

    /// The route of the swap, with the tokens the encoder reads.
    pub(super) fn route(&self) -> Route {
        Route::new(
            vec![self.swap.clone()],
            self.tokens
                .iter()
                .map(|token| (token.address.clone(), token.clone())),
        )
        .expect("a route of one swap is not empty")
    }
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashSet;
    use tycho_simulation::{
        tycho_client::feed::{BlockHeader, SynchronizerState},
        tycho_common::simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{addr, component_with_protocol, token, MockProtocolSim},
        feed::exclusivity::mark_exclusive,
        simulation::probe::fixtures::{derived, market, sizer, BLOCK, POOL},
    };

    #[test]
    fn test_plan_sizes_and_prices_a_direction() {
        let swap = ProbeSwap::plan(&market(), &derived(), &sizer(), POOL, 0).unwrap();

        // USDC -> WETH: 500 USDC at 0.0005 WETH per USDC is 0.25 WETH.
        assert_eq!(swap.component_id(), POOL);
        assert_eq!(swap.protocol_system(), "uniswap_v2");
        assert_eq!(swap.token_in(), &addr(0x01));
        assert_eq!(swap.token_out(), &addr(0x02));
        assert_eq!(swap.amount_in(), &BigUint::from(500_000_000u64));
        assert_eq!(swap.expected_out(), &BigUint::from(250_000_000_000_000_000u64));
        assert_eq!(swap.block().number(), BLOCK);
    }

    #[test]
    fn test_plan_rotates_through_the_directions() {
        let first = ProbeSwap::plan(&market(), &derived(), &sizer(), POOL, 0).unwrap();
        let second = ProbeSwap::plan(&market(), &derived(), &sizer(), POOL, 1).unwrap();
        let wrapped = ProbeSwap::plan(&market(), &derived(), &sizer(), POOL, 2).unwrap();

        assert_eq!((first.token_in(), first.token_out()), (&addr(0x01), &addr(0x02)));
        assert_eq!((second.token_in(), second.token_out()), (&addr(0x02), &addr(0x01)));
        assert_eq!((wrapped.token_in(), wrapped.token_out()), (&addr(0x01), &addr(0x02)));
    }

    #[test]
    fn test_plan_skips_a_direction_it_cannot_size() {
        let mut prices = derived()
            .token_prices()
            .unwrap()
            .clone();
        prices.remove(&addr(0x02));
        let mut derived = DerivedData::new();
        derived.set_token_prices(prices, vec![], BLOCK);

        let swap = ProbeSwap::plan(&market(), &derived, &sizer(), POOL, 1).unwrap();

        assert_eq!(swap.token_in(), &addr(0x01));
    }

    #[test]
    fn test_plan_reports_the_last_direction_reason() {
        let error = ProbeSwap::plan(&market(), &DerivedData::new(), &sizer(), POOL, 0)
            .err()
            .unwrap();

        let ProbePlanError::NoDirection { component_id, reason } = error else {
            panic!("expected NoDirection, got {error:?}");
        };
        assert_eq!(component_id, POOL);
        assert!(reason.contains("no token prices"), "got: {reason}");
    }

    #[test]
    fn test_plan_unknown_component() {
        let error = ProbeSwap::plan(&market(), &derived(), &sizer(), "0xdead", 0)
            .err()
            .unwrap();

        assert_eq!(error, ProbePlanError::UnknownComponent("0xdead".to_string()));
    }

    #[test]
    fn test_plan_component_without_state() {
        let mut market = market();
        let (usdc, weth) = (token(0x01, "USDC"), token(0x02, "WETH"));
        market.upsert_components([component_with_protocol("0xb1", "uniswap_v2", &[usdc, weth])]);

        let error = ProbeSwap::plan(&market, &derived(), &sizer(), "0xb1", 0)
            .err()
            .unwrap();

        assert_eq!(error, ProbePlanError::NoState("0xb1".to_string()));
    }

    #[test]
    fn test_plan_before_the_first_block() {
        let mut market = MarketState::new();
        let (usdc, weth) = (token(0x01, "USDC"), token(0x02, "WETH"));
        market.upsert_components([component_with_protocol(POOL, "uniswap_v2", &[usdc, weth])]);
        market.update_states([(
            POOL.to_string(),
            Box::new(MockProtocolSim::new(0.0005)) as Box<dyn ProtocolSim>,
        )]);

        let error = ProbeSwap::plan(&market, &derived(), &sizer(), POOL, 0)
            .err()
            .unwrap();

        assert_eq!(error, ProbePlanError::NoBlock);
    }

    #[test]
    fn test_plan_out_of_sync_protocol() {
        let mut market = market();
        market.update_sync_status(
            [(
                "uniswap_v2".to_string(),
                SynchronizerState::Stale(BlockHeader { number: 90, ..Default::default() }),
            )],
            std::time::Duration::from_secs(15),
        );

        let error = ProbeSwap::plan(&market, &derived(), &sizer(), POOL, 0)
            .err()
            .unwrap();

        assert_eq!(error, ProbePlanError::OutOfSync("uniswap_v2".to_string()));
    }

    #[test]
    fn test_plan_components_that_are_not_probeable() {
        let (usdc, weth) = (token(0x01, "USDC"), token(0x02, "WETH"));
        let mut exclusive =
            component_with_protocol("exclusive", "uniswap_v2", &[usdc.clone(), weth.clone()]);
        mark_exclusive(&mut exclusive);
        let components = [
            component_with_protocol("rfq", "rfq:bebop", &[usdc.clone(), weth.clone()]),
            component_with_protocol("pamm", "fallback:metric", &[usdc.clone(), weth.clone()]),
            exclusive,
        ];
        let mut market = market();
        let ids: FxHashSet<_> = components
            .iter()
            .map(|c| c.id.to_string())
            .collect();
        market.upsert_components(components);
        for id in &ids {
            market.update_states([(
                id.clone(),
                Box::new(MockProtocolSim::new(0.0005)) as Box<dyn ProtocolSim>,
            )]);
        }

        for id in ids {
            let error = ProbeSwap::plan(&market, &derived(), &sizer(), &id, 0)
                .err()
                .unwrap();

            assert!(matches!(error, ProbePlanError::NotProbeable { .. }), "{id}: {error:?}");
        }
    }
}
