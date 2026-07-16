//! Per-worker permission scoping for exclusive components.
//!
//! "Exclusive" means accessible only to Fynd: such components must never enter the route a
//! public pool returns, yet the surplus pool is allowed to route through them to capture the
//! surplus they offer above the best public-market rate. Both pool kinds serve the same request;
//! they differ only in which liquidity their workers may route through. Isolation is achieved by
//! filtering each worker's local graph topology/events through its `PermissionContext` — the
//! shared `MarketState` is never duplicated.

use std::{collections::HashMap, sync::Arc};

use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    feed::{events::MarketEvent, market_data::MarketState},
    types::ComponentId,
};

/// Classifies a [`ProtocolComponent`] as exclusive or public.
///
/// The predicate is supplied by the caller rather than hard-coded against an id or protocol name,
/// so the notion of "exclusive" can evolve (e.g. a hook address allowlist) without touching the
/// routing core.
#[derive(Clone)]
pub struct PermissionPolicy {
    /// Returns `true` when the component is exclusive and therefore excluded from public
    /// quotes.
    is_exclusive: Arc<dyn Fn(&ProtocolComponent) -> bool + Send + Sync>,
}

impl PermissionPolicy {
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
}

impl std::fmt::Debug for PermissionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionPolicy")
            .finish_non_exhaustive()
    }
}

/// Per-worker permission scoping: which components a worker may ingest into its local graph.
///
/// Each worker gets exactly one context for its lifetime, derived from its pool's role. All graph
/// filtering for a worker flows through this type, so the worker never reasons about
/// exclusivity itself — it just hands its topology and incoming events here.
#[derive(Clone, Debug)]
pub enum PermissionContext {
    /// See every component — the default. No filtering is applied.
    IncludeAll,
    /// See only public components — exclusive ones are filtered out by the attached policy.
    PublicOnly(PermissionPolicy),
}

impl PermissionContext {
    /// Returns the policy to enforce, or `None` when this worker filters nothing.
    fn active_policy(&self) -> Option<&PermissionPolicy> {
        match self {
            Self::PublicOnly(policy) => Some(policy),
            Self::IncludeAll => None,
        }
    }

    /// Filters a full topology map to the components this worker may see.
    ///
    /// Public workers drop exclusive components; surplus workers (and workers with no policy)
    /// receive the map unchanged.
    pub(crate) fn filter_topology(
        &self,
        market: &MarketState,
        topology: HashMap<ComponentId, Vec<Address>>,
    ) -> HashMap<ComponentId, Vec<Address>> {
        let Some(policy) = self.active_policy() else {
            return topology;
        };
        topology
            .into_iter()
            .filter(|(id, _)| {
                market
                    .get_component(id)
                    .is_none_or(|c| !policy.is_exclusive(c))
            })
            .collect()
    }

    /// Restricts a market event to the components this worker may see.
    ///
    /// Public workers drop exclusive component ids from the added/updated/removed lists so an
    /// exclusive component is never ingested mid-stream; other workers see the event unchanged.
    pub(crate) fn scope_event(&self, market: &MarketState, event: MarketEvent) -> MarketEvent {
        let Some(policy) = self.active_policy() else {
            return event;
        };
        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            event;

        let added_components = self.filter_topology(market, added_components);
        let removed_components = self.filter_component_ids(policy, market, &removed_components);
        let updated_components = self.filter_component_ids(policy, market, &updated_components);

        MarketEvent::MarketUpdated { added_components, removed_components, updated_components }
    }

    /// Keeps only the component ids that are NOT exclusive under `policy`.
    fn filter_component_ids(
        &self,
        policy: &PermissionPolicy,
        market: &MarketState,
        ids: &[ComponentId],
    ) -> Vec<ComponentId> {
        ids.iter()
            .filter(|id| {
                market
                    .get_component(id)
                    .is_none_or(|c| !policy.is_exclusive(c))
            })
            .cloned()
            .collect()
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
    fn exclusive_protocol_policy() -> PermissionPolicy {
        PermissionPolicy::new(|c: &ProtocolComponent| c.protocol_system == "vm:exclusive")
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
    fn is_exclusive_reflects_predicate() {
        let policy = exclusive_protocol_policy();
        assert!(policy.is_exclusive(&exclusive_component("perm-1")));
        assert!(!policy.is_exclusive(&public_component("pub-1")));
    }

    #[test]
    fn filter_topology_excludes_exclusive() {
        let policy = exclusive_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), exclusive_component("perm-1")]);
        let topology = market.component_topology();

        let public = PermissionContext::PublicOnly(policy);
        let public_view = public.filter_topology(&market, topology.clone());
        assert!(public_view.contains_key("pub-1"));
        assert!(!public_view.contains_key("perm-1"));

        let surplus_view = PermissionContext::IncludeAll.filter_topology(&market, topology.clone());
        assert_eq!(surplus_view.len(), topology.len());
    }

    #[test]
    fn scope_event_excludes_exclusive() {
        let policy = exclusive_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), exclusive_component("perm-1")]);
        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([
                ("pub-1".to_string(), vec![]),
                ("perm-1".to_string(), vec![]),
            ]),
            removed_components: vec!["pub-1".to_string(), "perm-1".to_string()],
            updated_components: vec!["pub-1".to_string(), "perm-1".to_string()],
        };

        let public = PermissionContext::PublicOnly(policy);
        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            public.scope_event(&market, event);
        assert!(added_components.contains_key("pub-1"));
        assert!(!added_components.contains_key("perm-1"));
        assert_eq!(removed_components, vec!["pub-1".to_string()]);
        assert_eq!(updated_components, vec!["pub-1".to_string()]);
    }
}
