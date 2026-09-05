//! Which pAMM components a worker's graph may hold.
//!
//! A `propammfallback:` leg only reaches the chain through the Uniswap V3 pool it falls back to, so
//! a pAMM this market does not back can never produce a quotable route. The answer expires — pools
//! arrive and leave, tiers change — so it is re-decided on every market event.

use metrics::counter;
use rustc_hash::{FxHashMap, FxHashSet};
use tycho_simulation::{tycho_common::models::protocol::ProtocolComponent, tycho_core::Bytes};

use crate::{
    feed::{events::MarketEvent, market_data::MarketDataView},
    propamm_fallback::{is_pamm, is_unbacked_pamm, FallbackPoolIndex, FeeTiers, SharedFeeTiers},
    types::ComponentId,
};

/// Which pAMMs one worker's graph holds, and the market facts that decide it.
///
/// Built for one worker, and only that worker reads it: two worker pools facing the same market
/// hold different graphs, because they exclude different components.
pub(crate) struct PammAdmission {
    /// Fee tiers the PropAMMRouter falls back on, read from chain by `FeeTierFetcher`.
    fee_tiers: SharedFeeTiers,
    /// Uniswap V3 pools the PropAMMRouter can fall back to, kept current from market events.
    pools: FallbackPoolIndex,
    /// The pAMMs the graph holds.
    admitted: FxHashSet<ComponentId>,
    /// The pAMMs the graph left out, because the market did not back them.
    withheld: FxHashSet<ComponentId>,
    /// The fee tiers the graph was last filtered with, `None` before the first read.
    built_with_fee_tiers: Option<FeeTiers>,
    /// The worker pool this belongs to, as the `pool` label on the admission counter.
    pool_name: String,
}

impl PammAdmission {
    /// Starts with no tiers, no pools and no pAMMs on record.
    pub(crate) fn new(pool_name: String) -> Self {
        Self {
            fee_tiers: SharedFeeTiers::default(),
            pools: FallbackPoolIndex::default(),
            admitted: FxHashSet::default(),
            withheld: FxHashSet::default(),
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

    /// Whether `component` is a pAMM this market does not back, against the pools indexed here.
    ///
    /// `fee_tiers` is passed in so one pass over many components reads it once.
    pub(crate) fn is_unbacked(
        &self,
        component: &ProtocolComponent,
        fee_tiers: Option<&FeeTiers>,
    ) -> bool {
        is_unbacked_pamm(component, fee_tiers, &self.pools)
    }

    /// Rebuilds the pool index from the whole market.
    ///
    /// Call it before filtering a topology, from the same market read.
    pub(crate) fn rebuild_pools(&mut self, market: &MarketDataView<'_>) {
        self.pools = FallbackPoolIndex::build(market);
    }

    /// Records what a graph build decided. `kept` is the topology it ended up with; every other
    /// pAMM in the market was left out.
    pub(crate) fn record_build(
        &mut self,
        market: &MarketDataView<'_>,
        kept: &FxHashMap<ComponentId, Vec<Bytes>>,
        fee_tiers: Option<FeeTiers>,
    ) {
        self.admitted.clear();
        self.withheld.clear();
        for component_id in market.component_topology().keys() {
            let is_pamm_component = market
                .get_component(component_id)
                .is_some_and(is_pamm);
            if !is_pamm_component {
                continue;
            }
            let set = if kept.contains_key(component_id) {
                &mut self.admitted
            } else {
                &mut self.withheld
            };
            set.insert(component_id.clone());
        }
        self.built_with_fee_tiers = fee_tiers;
    }

    /// Updates the pool index from one market event, as the market broadcast it.
    ///
    /// Unfiltered, because the index describes the market rather than one worker's graph, and
    /// before [`select_pamm_updates`](Self::select_pamm_updates), so a pAMM arriving in the same
    /// block as its pool is judged against an index that holds it.
    pub(crate) fn update_pools(&mut self, market: &MarketDataView<'_>, event: &MarketEvent) {
        self.pools.apply_event(market, event);
    }

    /// Rewrites `event` so that applying it leaves the graph holding exactly the pAMMs this
    /// market backs.
    ///
    /// An unbacked pAMM drops out of the additions. A pAMM whose fallback pool has left joins the
    /// removals. A pAMM withheld earlier joins the additions once the market backs it: the event
    /// names the pool that moved, never the pAMMs that fall back to it.
    ///
    /// Expects an event the caller has already filtered, and the pool index to have seen the
    /// unfiltered one — see [`update_pools`](Self::update_pools). `fee_tiers` is the one the
    /// caller tested with [`needs_rebuild`](Self::needs_rebuild), so both decisions read one value.
    pub(crate) fn select_pamm_updates(
        &mut self,
        market: &MarketDataView<'_>,
        fee_tiers: Option<&FeeTiers>,
        event: &mut MarketEvent,
    ) {
        let MarketEvent::MarketUpdated { added_components, removed_components, .. } = event;
        // Only an added or removed component moves a pool in or out of the index, so a block that
        // carries neither cannot change any of these answers.
        if added_components.is_empty() && removed_components.is_empty() {
            return;
        }

        // Destructured so the closure below can read the index while it writes the two sets.
        let Self { pools, admitted, withheld, pool_name, .. } = self;
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
            admitted.remove(component_id);
            withheld.remove(component_id);
        }

        added_components.retain(|component_id, _| {
            let Some(component) = market.get_component(component_id) else {
                // The market cannot classify it, so neither can this rule. `remove_components`
                // keeps such a component for the same reason.
                return true;
            };
            if !is_pamm(component) {
                return true;
            }
            if is_unbacked_pamm(component, fee_tiers, pools) {
                withheld.insert(component_id.clone());
                count("dropped");
                return false;
            }
            admitted.insert(component_id.clone());
            count("admitted");
            true
        });

        let readmitted: Vec<ComponentId> = withheld
            .iter()
            .filter(|component_id| !unbacked(component_id))
            .cloned()
            .collect();
        for component_id in readmitted {
            withheld.remove(&component_id);
            admitted.insert(component_id.clone());
            count("admitted");
            let tokens = market
                .get_component(&component_id)
                .map(|component| component.tokens.clone())
                .unwrap_or_default();
            added_components.insert(component_id, tokens);
        }

        let evicted: Vec<ComponentId> = admitted
            .iter()
            .filter(|component_id| unbacked(component_id))
            .cloned()
            .collect();
        for component_id in &evicted {
            admitted.remove(component_id);
            withheld.insert(component_id.clone());
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

    #[cfg(test)]
    pub(crate) fn admitted(&self) -> &FxHashSet<ComponentId> {
        &self.admitted
    }

    #[cfg(test)]
    pub(crate) fn withheld(&self) -> &FxHashSet<ComponentId> {
        &self.withheld
    }

    #[cfg(test)]
    pub(crate) fn built_with_fee_tiers(&self) -> Option<&FeeTiers> {
        self.built_with_fee_tiers.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn admit_for_test(&mut self, component_id: ComponentId) {
        self.admitted.insert(component_id);
    }

    #[cfg(test)]
    pub(crate) fn forget_for_test(&mut self) {
        self.admitted.clear();
        self.withheld.clear();
    }
}
