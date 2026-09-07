//! Which pAMM components a worker's graph may hold.
//!
//! A `propammfallback:` leg only reaches the chain through the Uniswap V3 pool it falls back to, so
//! a pAMM this market does not back can never produce a quotable route. The answer expires — pools
//! arrive and leave, tiers change — so it is re-decided on every market event.

use metrics::counter;
use rustc_hash::{FxHashMap, FxHashSet};
use tycho_simulation::tycho_common::models::protocol::ProtocolComponent;

use crate::{
    feed::{events::MarketEvent, market_data::MarketDataView},
    propamm_fallback::{is_pamm, is_unbacked_pamm, FallbackPoolIndex, FeeTiers, SharedFeeTiers},
    types::ComponentId,
};

/// Where one pAMM stands with a worker's graph.
///
/// One value per pAMM, so a pAMM can never be admitted and withheld at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PammState {
    /// The graph holds it.
    Admitted,
    /// The graph left it out, because the market backs no Uniswap V3 pool it can fall back to.
    Withheld,
}

/// Which pAMMs one worker's graph holds, and the market facts that decide it.
///
/// Built for one worker, and only that worker reads it: two worker pools facing the same market
/// hold different graphs, because they exclude different components.
pub(crate) struct PammManager {
    /// Fee tiers the PropAMMRouter falls back on, read from chain by `FeeTierFetcher`.
    fee_tiers: SharedFeeTiers,
    /// Uniswap V3 pools the PropAMMRouter can fall back to, kept current from market events.
    pools: FallbackPoolIndex,
    /// Where each pAMM this rule decided stands. A pAMM the caller's own rule drops is absent,
    /// so a withheld pAMM always means one this market does not back.
    states: FxHashMap<ComponentId, PammState>,
    /// The fee tiers the graph was last filtered with, `None` before the first read.
    built_with_fee_tiers: Option<FeeTiers>,
    /// The worker pool this belongs to, as the `pool` label on the admission counter.
    pool_name: String,
}

impl PammManager {
    /// Starts with no tiers, no pools and no pAMMs on record.
    pub(crate) fn new(pool_name: String) -> Self {
        Self {
            fee_tiers: SharedFeeTiers::default(),
            pools: FallbackPoolIndex::default(),
            states: FxHashMap::default(),
            built_with_fee_tiers: None,
            pool_name,
        }
    }

    /// Sets the tiers used to locate a pAMM leg's Uniswap V3 fallback pool.
    pub(crate) fn with_fee_tiers(mut self, fee_tiers: SharedFeeTiers) -> Self {
        self.fee_tiers = fee_tiers;
        self
    }

    /// The tiers as they stand, `None` before `FeeTierFetcher` reads the router.
    pub(crate) fn fee_tiers(&self) -> Option<FeeTiers> {
        self.fee_tiers.snapshot()
    }

    /// The pools a pAMM leg can fall back to, for pricing a route that holds one.
    pub(crate) fn pools(&self) -> &FallbackPoolIndex {
        &self.pools
    }

    /// Whether `fee_tiers` has moved since the graph was filtered.
    ///
    /// A tier decides which pool a pAMM falls back to, so a change can make one routable or
    /// unroutable, and only a rebuild can add a component back.
    pub(crate) fn needs_rebuild(&self, fee_tiers: Option<&FeeTiers>) -> bool {
        fee_tiers != self.built_with_fee_tiers.as_ref()
    }

    /// The pAMMs a graph built from this market must leave out, because the market backs no
    /// Uniswap V3 pool they can fall back to.
    ///
    /// Reindexes the fallback pools, reads the tiers and decides every pAMM in one call, all from
    /// the `market` the caller is about to build from. The caller filters its topology with the
    /// returned ids, so the graph and this record cannot disagree.
    ///
    /// `caller_drops` is the caller's own rule. A pAMM it drops never reaches the graph for a
    /// reason this manager does not own, so it is left off the record entirely rather than
    /// withheld — otherwise the event path would try to put it back.
    pub(crate) fn withhold_from_graph(
        &mut self,
        market: &MarketDataView<'_>,
        caller_drops: &dyn Fn(&ProtocolComponent) -> bool,
    ) -> FxHashSet<ComponentId> {
        self.pools = FallbackPoolIndex::build(market);
        let fee_tiers = self.fee_tiers.snapshot();

        self.states.clear();
        let mut withheld = FxHashSet::default();
        for component_id in market.component_topology().keys() {
            let Some(component) = market.get_component(component_id) else {
                continue;
            };
            if !is_pamm(component) || caller_drops(component) {
                continue;
            }
            let state = if is_unbacked_pamm(component, fee_tiers.as_ref(), &self.pools) {
                withheld.insert(component_id.clone());
                PammState::Withheld
            } else {
                PammState::Admitted
            };
            self.states
                .insert(component_id.clone(), state);
        }
        self.built_with_fee_tiers = fee_tiers;
        withheld
    }

    /// Updates the pool index from one market event, as the market broadcast it.
    ///
    /// Unfiltered, because the index describes the market rather than one worker's graph, and
    /// before [`select_pamm_updates`](Self::select_pamm_updates), so a pAMM arriving in the same
    /// block as its pool is judged against an index that holds it.
    pub(crate) fn update_pools(&mut self, market: &MarketDataView<'_>, event: &MarketEvent) {
        self.pools.apply_event(market, event);
    }

    /// Rewrites `event`, as the market broadcast it, so that applying it leaves the graph holding
    /// exactly the pAMMs this market backs.
    ///
    /// An unbacked pAMM drops out of the additions. A pAMM whose fallback pool has left joins the
    /// removals. A pAMM withheld earlier joins the additions once the market backs it: the event
    /// names the pool that moved, never the pAMMs that fall back to it.
    ///
    /// Takes the event the market broadcast, before the caller's own filter touches it. A pAMM
    /// falls back to a pool its worker excludes just as well, so a block the caller's filter
    /// empties can still change these answers. `caller_drops` is the caller's own rule, and this
    /// leaves every component it drops for the caller's filter to remove, on no record of its own.
    ///
    /// `fee_tiers` is the one the caller tested with [`needs_rebuild`](Self::needs_rebuild), so
    /// both decisions read one value.
    pub(crate) fn select_pamm_updates(
        &mut self,
        market: &MarketDataView<'_>,
        fee_tiers: Option<&FeeTiers>,
        caller_drops: &dyn Fn(&ProtocolComponent) -> bool,
        event: &mut MarketEvent,
    ) {
        let MarketEvent::MarketUpdated { added_components, removed_components, .. } = event;
        // Only an added or removed component moves a pool in or out of the index, so a block that
        // carries neither cannot change any of these answers.
        if added_components.is_empty() && removed_components.is_empty() {
            return;
        }

        // Destructured so the closure below can read the index while it writes the states.
        let Self { pools, states, pool_name, .. } = self;
        let unbacked = |component_id: &ComponentId| {
            market
                .get_component(component_id)
                .is_some_and(|component| is_unbacked_pamm(component, fee_tiers, pools))
        };
        let count = |outcome: &'static str| {
            counter!("propamm_admissions_total", "outcome" => outcome, "pool" => pool_name.clone())
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
            if is_unbacked_pamm(component, fee_tiers, pools) {
                states.insert(component_id.clone(), PammState::Withheld);
                count("dropped");
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
                (!is_unbacked_pamm(component, fee_tiers, pools))
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
                pool = %pool_name,
                components = ?evicted,
                "dropping pAMM components whose Uniswap V3 fallback left the market"
            );
            removed_components.extend(evicted);
        }
    }

    /// Rebuilds the pool index without deciding any pAMM, so a test can drive the event path
    /// against a market it never built a graph from.
    #[cfg(test)]
    pub(crate) fn rebuild_pools(&mut self, market: &MarketDataView<'_>) {
        self.pools = FallbackPoolIndex::build(market);
    }

    #[cfg(test)]
    pub(crate) fn state_of(&self, component_id: &str) -> Option<PammState> {
        self.states.get(component_id).copied()
    }

    #[cfg(test)]
    pub(crate) fn count_in(&self, state: PammState) -> usize {
        self.states
            .values()
            .filter(|recorded| **recorded == state)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn built_with_fee_tiers(&self) -> Option<&FeeTiers> {
        self.built_with_fee_tiers.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn admit_for_test(&mut self, component_id: ComponentId) {
        self.states
            .insert(component_id, PammState::Admitted);
    }

    #[cfg(test)]
    pub(crate) fn forget_for_test(&mut self) {
        self.states.clear();
    }
}
