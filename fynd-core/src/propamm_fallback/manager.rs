//! Which pAMM components a worker's graph may hold.
//!
//! A `propammfallback:` leg only reaches the chain through the fallback pool the solver picks for
//! it, so a pAMM whose pair this market holds no candidate for can never produce a quotable
//! route. The answer expires — pools arrive and leave — so it is re-decided on every market event.

use metrics::counter;
use rustc_hash::{FxHashMap, FxHashSet};
use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    feed::{events::MarketEvent, market_data::MarketDataView},
    propamm_fallback::{is_pamm, must_withhold_pamm, FallbackPoolIndex},
    types::ComponentId,
};

/// Where one pAMM stands with a worker's graph.
///
/// One value per pAMM, so a pAMM can never be admitted and withheld at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PammState {
    /// The graph holds it.
    Admitted,
    /// The graph left it out, because the market holds no pool it can fall back to.
    Withheld,
}

/// Which pAMMs one worker's graph holds, and the market facts that decide it.
///
/// Built for one worker, and only that worker reads it: two worker pools facing the same market
/// hold different graphs, because they exclude different components.
pub(crate) struct PammManager {
    /// Candidate pools a pAMM leg can fall back to, by pair, kept current from market events.
    fallback_pools: FallbackPoolIndex,
    /// Where each pAMM this rule decided stands. A pAMM the caller's own rule drops is absent,
    /// so a withheld pAMM always means one this market does not back.
    states: FxHashMap<ComponentId, PammState>,
    /// The worker pool this belongs to, as the `pool` label on the admission counter.
    worker_pool_name: String,
}

impl PammManager {
    /// Starts with no pools and no pAMMs on record.
    pub(crate) fn new(worker_pool_name: String) -> Self {
        Self {
            fallback_pools: FallbackPoolIndex::default(),
            states: FxHashMap::default(),
            worker_pool_name,
        }
    }

    /// The pools a pAMM leg can fall back to, for selecting and pricing a route that holds one.
    pub(crate) fn fallback_pools(&self) -> &FallbackPoolIndex {
        &self.fallback_pools
    }

    /// The pAMMs a graph built from this market must leave out, because the market holds no pool
    /// they can fall back to.
    ///
    /// Reindexes the fallback pools and decides every pAMM in one call, all from the `market` the
    /// caller is about to build from. The caller filters `topology` with the returned ids, so the
    /// graph and this record cannot disagree. `topology` is the caller's own copy, walked here
    /// rather than allocated again.
    ///
    /// `caller_drops` is the caller's own rule. A pAMM it drops never reaches the graph for a
    /// reason this manager does not own, so it is left off the record entirely rather than
    /// withheld — otherwise the event path would try to put it back.
    pub(crate) fn withhold_from_graph(
        &mut self,
        market: &MarketDataView<'_>,
        topology: &FxHashMap<ComponentId, Vec<Address>>,
        caller_drops: &dyn Fn(&ProtocolComponent) -> bool,
    ) -> FxHashSet<ComponentId> {
        self.fallback_pools = FallbackPoolIndex::build(market);

        self.states.clear();
        let mut withheld = FxHashSet::default();
        for component_id in topology.keys() {
            let Some(component) = market.get_component(component_id) else {
                continue;
            };
            if !is_pamm(component) || caller_drops(component) {
                continue;
            }
            let state = if must_withhold_pamm(component, &self.fallback_pools) {
                withheld.insert(component_id.clone());
                PammState::Withheld
            } else {
                PammState::Admitted
            };
            self.states
                .insert(component_id.clone(), state);
        }
        withheld
    }

    /// Rewrites `event`, as the market broadcast it, so that applying it leaves the graph holding
    /// exactly the pAMMs this market backs.
    ///
    /// An unbacked pAMM drops out of the additions. A pAMM whose last candidate pool has left joins
    /// the removals. A pAMM withheld earlier joins the additions once the market backs it: the
    /// event names the pool that moved, never the pAMMs that fall back to it.
    ///
    /// Takes the event the market broadcast, before the caller's own filter touches it. The pool
    /// index describes the market rather than one worker's graph, so a pAMM falls back to a pool
    /// its worker excludes just as well, and the pAMM and its pool can arrive in one block.
    /// `caller_drops` is the caller's own rule, and this leaves every component it drops for the
    /// caller's filter to remove, on no record of its own.
    pub(crate) fn apply_pamm_admission(
        &mut self,
        market: &MarketDataView<'_>,
        caller_drops: &dyn Fn(&ProtocolComponent) -> bool,
        event: &mut MarketEvent,
    ) {
        self.fallback_pools
            .apply_event(market, event);

        let MarketEvent::MarketUpdated { added_components, removed_components, .. } = event;
        // Only an added or removed component moves a pool in or out of the index, so a block that
        // carries neither cannot change any of these answers.
        if added_components.is_empty() && removed_components.is_empty() {
            return;
        }

        // Destructured so the closure below can read the index while it writes the states.
        let Self { fallback_pools, states, worker_pool_name } = self;
        let unbacked = |component_id: &ComponentId| {
            market
                .get_component(component_id)
                .is_some_and(|component| must_withhold_pamm(component, fallback_pools))
        };
        let count = |outcome: &'static str| {
            counter!("propamm_admissions_total", "outcome" => outcome, "pool" => worker_pool_name.clone())
                .increment(1);
        };

        for component_id in removed_components.iter() {
            states.remove(component_id);
        }

        added_components.retain(|component_id, _| {
            let Some(component) = market.get_component(component_id) else {
                // The market cannot classify it, so neither can this rule. `remove_components`
                // keeps such a component for the same reason.
                return true;
            };
            if !is_pamm(component) || caller_drops(component) {
                return true;
            }
            if must_withhold_pamm(component, fallback_pools) {
                states.insert(component_id.clone(), PammState::Withheld);
                count("withheld");
                return false;
            }
            states.insert(component_id.clone(), PammState::Admitted);
            count("admitted");
            true
        });

        let readmitted: Vec<_> = states
            .iter()
            .filter(|(_, state)| **state == PammState::Withheld)
            .filter_map(|(component_id, _)| {
                let component = market.get_component(component_id)?;
                (!must_withhold_pamm(component, fallback_pools))
                    .then(|| (component_id.clone(), component.tokens.clone()))
            })
            .collect();
        for (component_id, tokens) in readmitted {
            states.insert(component_id.clone(), PammState::Admitted);
            count("admitted");
            added_components.insert(component_id, tokens);
        }

        let mut evicted = Vec::new();
        for (component_id, state) in states.iter() {
            if *state == PammState::Admitted && unbacked(component_id) {
                evicted.push(component_id.clone());
            }
        }
        for component_id in &evicted {
            states.insert(component_id.clone(), PammState::Withheld);
            count("evicted");
        }
        if !evicted.is_empty() {
            tracing::debug!(
                pool = %worker_pool_name,
                components = ?evicted,
                "dropping pAMM components whose last fallback pool left the market"
            );
            removed_components.extend(evicted);
        }
    }

    /// Rebuilds the pool index without deciding any pAMM, so a test can drive the event path
    /// against a market it never built a graph from.
    #[cfg(test)]
    pub(crate) fn rebuild_pools(&mut self, market: &MarketDataView<'_>) {
        self.fallback_pools = FallbackPoolIndex::build(market);
    }

    #[cfg(test)]
    pub(crate) fn state_of(&self, component_id: &str) -> Option<PammState> {
        self.states.get(component_id).copied()
    }
}
