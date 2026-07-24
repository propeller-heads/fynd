//! Exclusive-component classification and per-worker graph filtering.
//!
//! "Exclusive" means accessible only to Fynd: such components must never enter the route a
//! public pool returns, while an exclusive-access pool routes through them to capture the
//! surplus they offer above the best public-market rate. Both pool kinds serve the same
//! request; they differ only in which liquidity their workers may route through. Isolation is
//! achieved by filtering each worker's local graph topology/events through the pool's
//! [`ExclusivityPolicy`](crate::feed::exclusivity::ExclusivityPolicy) — the shared
//! `MarketState` is never duplicated. Workers of public
//! pools hold `Some(policy)` and filter; workers of exclusive-access pools hold `None` and
//! ingest everything.

use std::{collections::HashMap, sync::Arc};

use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    feed::{events::MarketEvent, market_data::MarketState},
    types::ComponentId,
};

/// Classifies a [`ProtocolComponent`] as exclusive or public, and filters worker inputs
/// accordingly.
///
/// The predicate is supplied by the caller rather than hard-coded against an id or protocol name,
/// so the notion of "exclusive" can evolve (e.g. a hook address allowlist) without touching the
/// routing core.
#[derive(Clone)]
pub struct ExclusivityPolicy {
    /// Returns `true` when the component is exclusive and therefore excluded from public
    /// quotes.
    is_exclusive: Arc<dyn Fn(&ProtocolComponent) -> bool + Send + Sync>,
}

impl ExclusivityPolicy {
    /// Creates a policy from a predicate identifying exclusive components.
    pub fn new<F>(predicate: F) -> Self
    where
        F: Fn(&ProtocolComponent) -> bool + Send + Sync + 'static,
    {
        Self { is_exclusive: Arc::new(predicate) }
    }

    /// Returns `true` if the component is exclusive.
    pub fn is_exclusive(&self, component: &ProtocolComponent) -> bool {
        (self.is_exclusive)(component)
    }

    /// Removes exclusive components from a full topology map.
    pub(crate) fn filter_topology(
        &self,
        market: &MarketState,
        topology: HashMap<ComponentId, Vec<Address>>,
    ) -> HashMap<ComponentId, Vec<Address>> {
        topology
            .into_iter()
            .filter(|(id, _)| {
                market
                    .get_component(id)
                    .is_none_or(|c| !self.is_exclusive(c))
            })
            .collect()
    }

    /// Removes exclusive component ids from a market event's added/updated/removed lists, so an
    /// exclusive component is never ingested mid-stream.
    pub(crate) fn scope_event(&self, market: &MarketState, event: MarketEvent) -> MarketEvent {
        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            event;

        let added_components = self.filter_topology(market, added_components);
        let removed_components = self.filter_component_ids(market, &removed_components);
        let updated_components = self.filter_component_ids(market, &updated_components);

        MarketEvent::MarketUpdated { added_components, removed_components, updated_components }
    }

    /// Keeps only the component ids that are NOT exclusive.
    fn filter_component_ids(&self, market: &MarketState, ids: &[ComponentId]) -> Vec<ComponentId> {
        ids.iter()
            .filter(|id| {
                market
                    .get_component(id)
                    .is_none_or(|c| !self.is_exclusive(c))
            })
            .cloned()
            .collect()
    }
}

impl std::fmt::Debug for ExclusivityPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExclusivityPolicy")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        algorithm::test_utils::{component, token},
        feed::{events::MarketEvent, market_data::MarketState},
    };

    /// A predicate that treats the `vm:exclusive` protocol system as exclusive.
    fn exclusive_protocol_policy() -> ExclusivityPolicy {
        ExclusivityPolicy::new(|c: &ProtocolComponent| c.protocol_system == "vm:exclusive")
    }

    fn exclusive_component(id: &str) -> ProtocolComponent {
        let mut c = component(id, &[token(0x01, "A"), token(0x02, "B")]);
        c.protocol_system = "vm:exclusive".to_string();
        c
    }

    fn public_component(id: &str) -> ProtocolComponent {
        component(id, &[token(0x01, "A"), token(0x02, "B")])
    }

    fn market_with(components: Vec<ProtocolComponent>) -> MarketState {
        let mut market = MarketState::new();
        market.upsert_components(components);
        market
    }

    #[test]
    fn test_is_exclusive_reflects_predicate() {
        let policy = exclusive_protocol_policy();
        assert!(policy.is_exclusive(&exclusive_component("excl-1")));
        assert!(!policy.is_exclusive(&public_component("pub-1")));
    }

    #[test]
    fn test_filter_topology_excludes_exclusive() {
        let policy = exclusive_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), exclusive_component("excl-1")]);
        let topology = market.component_topology();

        let filtered = policy.filter_topology(&market, topology);
        assert!(filtered.contains_key("pub-1"));
        assert!(!filtered.contains_key("excl-1"));
    }

    #[test]
    fn test_scope_event_excludes_exclusive() {
        let policy = exclusive_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), exclusive_component("excl-1")]);
        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([
                ("pub-1".to_string(), vec![]),
                ("excl-1".to_string(), vec![]),
            ]),
            removed_components: vec!["pub-1".to_string(), "excl-1".to_string()],
            updated_components: vec!["pub-1".to_string(), "excl-1".to_string()],
        };

        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            policy.scope_event(&market, event);
        assert!(added_components.contains_key("pub-1"));
        assert!(!added_components.contains_key("excl-1"));
        assert_eq!(removed_components, vec!["pub-1".to_string()]);
        assert_eq!(updated_components, vec!["pub-1".to_string()]);
    }
}
