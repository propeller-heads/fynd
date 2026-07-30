//! Exclusive-component classification and per-worker graph filtering.
//!
//! "Exclusive" means swappable only with off-chain authorization: such components must never
//! enter the route a public worker pool returns, while an exclusive-access worker pool routes
//! through them to capture the surplus they offer above the best public-market rate. Both worker
//! pool kinds serve the same request; they differ only in which liquidity their workers may route
//! through. Isolation is achieved by filtering each worker's local graph topology/events through
//! `filter_topology`/`scope_event` when the worker pool's `LiquidityScope` is `PublicOnly` — the
//! shared `MarketState` is never duplicated. Workers of `All`-scoped worker pools ingest
//! everything.
//!
//! A component is classified from its own data via `is_exclusive`, applied generically to every
//! ingested component.

use std::collections::HashMap;

use tycho_simulation::{
    tycho_common::models::{protocol::ProtocolComponent, Address},
    EXCLUSIVE_EXTENSIONS,
};

use crate::{
    feed::{events::MarketEvent, market_data::MarketState},
    types::ComponentId,
};

/// Returns `true` when the component offers exclusive liquidity, i.e. is swappable only with
/// off-chain authorization.
pub(crate) fn is_exclusive(component: &ProtocolComponent) -> bool {
    component
        .static_attributes
        .get("extension")
        .is_some_and(|extension| {
            EXCLUSIVE_EXTENSIONS
                .iter()
                .any(|addr| addr.as_slice() == &extension[..])
        })
}

/// Removes exclusive components from a full topology map.
pub(crate) fn filter_topology(
    market: &MarketState,
    topology: HashMap<ComponentId, Vec<Address>>,
) -> HashMap<ComponentId, Vec<Address>> {
    topology
        .into_iter()
        .filter(|(id, _)| {
            market
                .get_component(id)
                .is_none_or(|c| !is_exclusive(c))
        })
        .collect()
}

/// Removes exclusive component ids from a market event's added/updated/removed lists, so an
/// exclusive component is never ingested mid-stream.
pub(crate) fn scope_event(market: &MarketState, event: MarketEvent) -> MarketEvent {
    let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
        event;

    let added_components = filter_topology(market, added_components);
    let removed_components = filter_component_ids(market, &removed_components);
    let updated_components = filter_component_ids(market, &updated_components);

    MarketEvent::MarketUpdated { added_components, removed_components, updated_components }
}

/// Keeps only the component ids that are NOT exclusive.
fn filter_component_ids(market: &MarketState, ids: &[ComponentId]) -> Vec<ComponentId> {
    ids.iter()
        .filter(|id| {
            market
                .get_component(id)
                .is_none_or(|c| !is_exclusive(c))
        })
        .cloned()
        .collect()
}

/// Stamps the signed-exclusive-swap extension onto a component's static attributes so tests can
/// build exclusive components without protocol-specific fixtures.
#[cfg(test)]
pub(crate) fn mark_exclusive(component: &mut ProtocolComponent) {
    component.static_attributes.insert(
        "extension".to_string(),
        EXCLUSIVE_EXTENSIONS[0]
            .as_slice()
            .into(),
    );
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, token},
        feed::{events::MarketEvent, market_data::MarketState},
    };

    fn exclusive_component(id: &str) -> ProtocolComponent {
        let mut c = component(id, &[token(0x01, "A"), token(0x02, "B")]);
        mark_exclusive(&mut c);
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

    fn component_with_extension(extension: Vec<u8>) -> ProtocolComponent {
        let mut c = public_component("pub-1");
        c.static_attributes
            .insert("extension".to_string(), extension.into());
        c
    }

    #[rstest::rstest]
    #[case::exclusive_extension(exclusive_component("excl-1"), true)]
    #[case::missing_attribute(public_component("pub-1"), false)]
    #[case::other_extension(
        component_with_extension(
            address!("0x517E506700271AEa091b02f42756F5E174Af5230").as_slice().to_vec()
        ),
        false
    )]
    #[case::garbage_attribute(component_with_extension(vec![0x55, 0x19]), false)]
    fn test_is_exclusive(#[case] component: ProtocolComponent, #[case] expected: bool) {
        assert_eq!(is_exclusive(&component), expected);
    }

    #[test]
    fn test_filter_topology() {
        let market = market_with(vec![public_component("pub-1"), exclusive_component("excl-1")]);
        let topology = market.component_topology();

        let filtered = filter_topology(&market, topology);
        assert!(filtered.contains_key("pub-1"));
        assert!(!filtered.contains_key("excl-1"));
    }

    #[test]
    fn test_scope_event() {
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
            scope_event(&market, event);
        assert!(added_components.contains_key("pub-1"));
        assert!(!added_components.contains_key("excl-1"));
        assert_eq!(removed_components, vec!["pub-1".to_string()]);
        assert_eq!(updated_components, vec!["pub-1".to_string()]);
    }
}
