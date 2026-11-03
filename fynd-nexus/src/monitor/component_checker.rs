//! Checks one component: plans a swap through it from the market and probes it on chain.

use std::future::Future;

use fynd_core::{
    derived::SharedDerivedDataRef,
    feed::market_data::MarketData,
    simulation::{
        ComponentProbe, ProbeOutcome, ProbePlanError, ProbeSizer, ProbeSwap, RevertBlame,
    },
};

use crate::monitor::component_health::Observation;

/// What checking a component came to.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    /// The component is of a kind that is not executed on chain by the router alone, so it
    /// cannot be probed.
    NotProbeable,
    /// The component was probed, or the probe could not be set up.
    Observed(Observation),
}

/// Checks a component by sending one swap through it.
pub trait ComponentChecker: Send + Sync {
    /// Checks the component `component_id`. `rotation` is how many times it was probed before,
    /// so that repeated checks of a component with more than two tokens try every direction.
    fn check(
        &self,
        component_id: &str,
        rotation: usize,
    ) -> impl Future<Output = CheckResult> + Send;
}

/// Checks components of the live market by simulating a swap through each on chain.
pub struct MarketComponentChecker {
    market: MarketData,
    derived: SharedDerivedDataRef,
    sizer: ProbeSizer,
    probe: ComponentProbe,
}

impl MarketComponentChecker {
    /// A checker that plans swaps from `market` and `derived`, sized by `sizer`, and probes them
    /// with `probe`.
    pub fn new(
        market: MarketData,
        derived: SharedDerivedDataRef,
        sizer: ProbeSizer,
        probe: ComponentProbe,
    ) -> Self {
        Self { market, derived, sizer, probe }
    }
}

impl ComponentChecker for MarketComponentChecker {
    async fn check(&self, component_id: &str, rotation: usize) -> CheckResult {
        let planned = {
            let market = self.market.read().await;
            let derived = self.derived.read().await;
            ProbeSwap::plan(
                market.base_market_state(),
                &derived,
                &self.sizer,
                component_id,
                rotation,
            )
        };
        let swap = match planned {
            Ok(swap) => swap,
            Err(ProbePlanError::NotProbeable { .. }) => return CheckResult::NotProbeable,
            Err(error) => {
                return CheckResult::Observed(Observation::SetupFailed { reason: error.to_string() })
            }
        };
        let block = swap.block().number();
        let observation = match self.probe.probe(swap).await {
            ProbeOutcome::Executed { deviation_bps, .. } => {
                Observation::Executed { block, deviation_bps }
            }
            ProbeOutcome::Reverted { reason, blame: RevertBlame::Pool } => {
                Observation::PoolReverted { block, reason }
            }
            ProbeOutcome::Reverted { reason, blame: RevertBlame::Token } => {
                Observation::TokenReverted { block, reason }
            }
            ProbeOutcome::SetupFailed { reason } => Observation::SetupFailed { reason },
            ProbeOutcome::BlockUnavailable => Observation::BlockUnavailable,
        };
        CheckResult::Observed(observation)
    }
}
