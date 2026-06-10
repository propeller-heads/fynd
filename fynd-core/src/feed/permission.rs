//! Per-worker permission scoping for permissioned ("fair-flow hook") pools.
//!
//! Some pools are exclusive to Fynd: they must never appear in a normal public quote, yet a
//! dedicated "surplus" worker is allowed to route through them to capture the surplus
//! (`egAmount`) they offer above the best public-market rate. Isolation is achieved by filtering
//! each worker's local graph topology/events through a `PermissionPolicy` according to its
//! `ComponentScope` — the shared `MarketState` is never duplicated.

use std::{collections::HashMap, sync::Arc};

use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{feed::market_data::MarketState, types::ComponentId};

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

/// Filters a full topology map to the components visible under `scope`.
///
/// Returned by public workers as a strict subset of the input; surplus workers receive the input
/// unchanged.
pub fn filter_topology(
    scope: ComponentScope,
    policy: &PermissionPolicy,
    market: &MarketState,
    topology: HashMap<ComponentId, Vec<Address>>,
) -> HashMap<ComponentId, Vec<Address>> {
    // TODO: when scope == ExcludePermissioned, drop entries whose ComponentId resolves (via
    // market.get_component(id)) to a ProtocolComponent for which policy.is_permissioned is true.
    // When scope == IncludeAll, return `topology` unchanged.
    let _ = (scope, policy, market, topology);
    todo!("filter permissioned components from the worker's initial topology")
}

/// Filters a list of changed component ids to those visible under `scope`.
///
/// Used on the incremental-event path (added/updated/removed component ids) so that a public
/// worker never ingests a permissioned component mid-stream.
pub fn filter_component_ids(
    scope: ComponentScope,
    policy: &PermissionPolicy,
    market: &MarketState,
    ids: &[ComponentId],
) -> Vec<ComponentId> {
    // TODO: when scope == ExcludePermissioned, keep only ids whose ProtocolComponent (via
    // market.get_component(id)) is NOT permissioned per policy.is_permissioned. When scope ==
    // IncludeAll, return all ids unchanged.
    let _ = (scope, policy, market, ids);
    todo!("filter permissioned component ids from an incremental market event")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        algorithm::test_utils::{component, token},
        feed::market_data::MarketState,
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

        let public_view = filter_topology(
            ComponentScope::ExcludePermissioned,
            &policy,
            &market,
            topology.clone(),
        );
        assert!(public_view.contains_key("pub-1"));
        assert!(!public_view.contains_key("perm-1"));

        let surplus_view =
            filter_topology(ComponentScope::IncludeAll, &policy, &market, topology.clone());
        assert_eq!(surplus_view.len(), topology.len());
    }

    #[test]
    #[ignore = "scaffold: filter helpers are todo!()"]
    fn filter_component_ids_excludes_permissioned() {
        let policy = permissioned_protocol_policy();
        let market = market_with(vec![public_component("pub-1"), permissioned_component("perm-1")]);
        let ids = vec!["pub-1".to_string(), "perm-1".to_string()];

        let public_ids =
            filter_component_ids(ComponentScope::ExcludePermissioned, &policy, &market, &ids);
        assert_eq!(public_ids, vec!["pub-1".to_string()]);

        let surplus_ids = filter_component_ids(ComponentScope::IncludeAll, &policy, &market, &ids);
        assert_eq!(surplus_ids, ids);
    }
}
