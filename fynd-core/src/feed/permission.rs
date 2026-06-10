//! Per-worker permission scoping for permissioned ("fair-flow hook") pools.
//!
//! Some pools are exclusive to Fynd: they must never appear in a normal public quote, yet a
//! dedicated "surplus" worker is allowed to route through them to capture the surplus
//! (`egAmount`) they offer above the best public-market rate. Isolation is achieved by filtering
//! each worker's local graph topology/events through a `PermissionPolicy` according to its
//! `ComponentScope` — the shared `MarketState` is never duplicated.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    feed::{events::MarketEvent, market_data::MarketState},
    types::ComponentId,
};

/// Classifies a [`ProtocolComponent`] as permissioned (Fynd-exclusive) or public.
///
/// The predicate is supplied by the caller rather than hard-coded against an id or protocol name,
/// so the notion of "permissioned" can evolve (e.g. a hook address allowlist) without touching the
/// routing core.
#[derive(Clone)]
pub struct PermissionPolicy {
    /// Returns `true` when the component is permissioned and therefore excluded from public
    /// quotes.
    is_permissioned: Arc<dyn Fn(&ProtocolComponent) -> bool + Send + Sync>,
}

impl PermissionPolicy {
    /// Creates a policy from a predicate identifying permissioned components.
    pub fn new<F>(predicate: F) -> Self
    where
        F: Fn(&ProtocolComponent) -> bool + Send + Sync + 'static,
    {
        Self { is_permissioned: Arc::new(predicate) }
    }

    /// Returns `true` if the component is permissioned (Fynd-exclusive).
    pub fn is_permissioned(&self, component: &ProtocolComponent) -> bool {
        (self.is_permissioned)(component)
    }
}

impl std::fmt::Debug for PermissionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionPolicy")
            .finish_non_exhaustive()
    }
}

/// The set of components a worker is allowed to see in its local graph.
///
/// Determines whether permissioned components are filtered out before the worker builds or updates
/// its graph. Each worker gets exactly one scope for its lifetime, derived from its pool's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentScope {
    /// Public workers: permissioned components are dropped so they can never appear in a quote.
    ExcludePermissioned,
    /// Surplus workers: every component is visible, including permissioned ones.
    IncludeAll,
}

/// Per-worker permission scoping: which components a worker may ingest into its local graph.
///
/// Bundles a [`ComponentScope`] with an optional [`PermissionPolicy`]. All graph filtering for a
/// worker flows through this type, so the worker never reasons about permissioned-ness itself — it
/// just hands its topology and incoming events here.
#[derive(Clone, Debug)]
pub(crate) struct PermissionContext {
    /// Scope governing whether permissioned components are filtered out.
    scope: ComponentScope,
    /// Predicate classifying components as permissioned. `None` ⇒ no filtering is ever applied.
    policy: Option<PermissionPolicy>,
}

impl PermissionContext {
    /// Default context: see every component, apply no permission filtering.
    pub(crate) fn include_all() -> Self {
        Self { scope: ComponentScope::IncludeAll, policy: None }
    }

    /// Context derived from a pool's role: a scope plus the (optional) classifying policy.
    pub(crate) fn new(scope: ComponentScope, policy: Option<PermissionPolicy>) -> Self {
        Self { scope, policy }
    }

    /// Returns the policy to enforce, or `None` when this worker filters nothing (scope is
    /// `IncludeAll`, or no policy was configured).
    fn active_policy(&self) -> Option<&PermissionPolicy> {
        match (self.scope, &self.policy) {
            (ComponentScope::ExcludePermissioned, Some(policy)) => Some(policy),
            _ => None,
        }
    }

    /// Filters a full topology map to the components this worker may see.
    ///
    /// Public workers drop permissioned components; surplus workers (and workers with no policy)
    /// receive the map unchanged.
    pub(crate) fn filter_topology(
        &self,
        market: &MarketState,
        topology: HashMap<ComponentId, Vec<Address>>,
    ) -> HashMap<ComponentId, Vec<Address>> {
        let Some(policy) = self.active_policy() else {
            return topology;
        };
        // TODO: drop entries whose ComponentId resolves (via market.get_component(id)) to a
        // ProtocolComponent for which policy.is_permissioned is true.
        let _ = (policy, market);
        todo!("filter permissioned components from the worker's initial topology")
    }

    /// Restricts a market event to the components this worker may see.
    ///
    /// Public workers drop permissioned component ids from the added/updated/removed lists so a
    /// permissioned component is never ingested mid-stream; other workers see the event unchanged.
    pub(crate) fn scope_event(&self, market: &MarketState, event: MarketEvent) -> MarketEvent {
        let Some(policy) = self.active_policy() else {
            return event;
        };
        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            event;

        let added_ids: Vec<ComponentId> = added_components
            .keys()
            .cloned()
            .collect();
        let permitted_added: HashSet<ComponentId> = self
            .filter_component_ids(policy, market, &added_ids)
            .into_iter()
            .collect();
        let added_components = added_components
            .into_iter()
            .filter(|(id, _)| permitted_added.contains(id))
            .collect();
        let removed_components = self.filter_component_ids(policy, market, &removed_components);
        let updated_components = self.filter_component_ids(policy, market, &updated_components);

        MarketEvent::MarketUpdated { added_components, removed_components, updated_components }
    }

    /// Keeps only the component ids that are NOT permissioned under `policy`.
    fn filter_component_ids(
        &self,
        policy: &PermissionPolicy,
        market: &MarketState,
        ids: &[ComponentId],
    ) -> Vec<ComponentId> {
        // TODO: keep only ids whose ProtocolComponent (via market.get_component(id)) is NOT
        // permissioned per policy.is_permissioned.
        let _ = (policy, market, ids);
        todo!("filter permissioned component ids from an incremental market event")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        algorithm::test_utils::{component, token},
        feed::{events::MarketEvent, market_data::MarketState},
    };

    /// A predicate that treats the `vm:permissioned` protocol system as Fynd-exclusive.
    fn permissioned_protocol_policy() -> PermissionPolicy {
        PermissionPolicy::new(|c: &ProtocolComponent| c.protocol_system == "vm:permissioned")
    }

    fn permissioned_component(id: &str) -> ProtocolComponent {
        let mut c = component(id, &[token(0x01, "A"), token(0x02, "B")]);
        c.protocol_system = "vm:permissioned".to_string();
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
    #[ignore = "scaffold: filter helpers are todo!()"]
    fn is_permissioned_reflects_predicate() {
        let policy = permissioned_protocol_policy();
        assert!(policy.is_permissioned(&permissioned_component("perm-1")));
        assert!(!policy.is_permissioned(&public_component("pub-1")));
    }

    #[test]
    #[ignore = "scaffold: filter helpers are todo!()"]
    fn filter_topology_excludes_permissioned() {
        let policy = permissioned_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), permissioned_component("perm-1")]);
        let topology = market.component_topology();

        let public = PermissionContext::new(ComponentScope::ExcludePermissioned, Some(policy));
        let public_view = public.filter_topology(&market, topology.clone());
        assert!(public_view.contains_key("pub-1"));
        assert!(!public_view.contains_key("perm-1"));

        let surplus_view =
            PermissionContext::include_all().filter_topology(&market, topology.clone());
        assert_eq!(surplus_view.len(), topology.len());
    }

    #[test]
    #[ignore = "scaffold: filter helpers are todo!()"]
    fn scope_event_excludes_permissioned() {
        let policy = permissioned_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), permissioned_component("perm-1")]);
        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([
                ("pub-1".to_string(), vec![]),
                ("perm-1".to_string(), vec![]),
            ]),
            removed_components: vec!["pub-1".to_string(), "perm-1".to_string()],
            updated_components: vec!["pub-1".to_string(), "perm-1".to_string()],
        };

        let public = PermissionContext::new(ComponentScope::ExcludePermissioned, Some(policy));
        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            public.scope_event(&market, event);
        assert!(added_components.contains_key("pub-1"));
        assert!(!added_components.contains_key("perm-1"));
        assert_eq!(removed_components, vec!["pub-1".to_string()]);
        assert_eq!(updated_components, vec!["pub-1".to_string()]);
    }
}
