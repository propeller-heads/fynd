//! Per-worker component filtering: each worker ingests only the components its worker pool admits.
//!
//! `MarketState` is never duplicated. A worker drops what it must not route through from its own
//! graph topology at startup and from every incoming [`MarketEvent`], so two worker pools reading
//! the same market can still route through different liquidity. Today two settings feed the
//! predicate: the worker pool's `LiquidityScope` (see
//! [`is_exclusive`](super::exclusivity::is_exclusive)) and its `exclude_protocols` list
//! ([`is_excluded_protocol`]).

use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    feed::{events::MarketEvent, market_data::MarketState},
    types::ComponentId,
};

/// Whether `component` belongs to a protocol system in `exclude_protocols`.
///
/// An entry names a protocol system exactly (`uniswap_v2`), or, when it ends with `:`, the whole
/// family under that prefix: `propammfallback:` excludes `propammfallback:fermiswap` and every
/// other venue the PropAMMRouter serves. An empty list excludes nothing.
pub(crate) fn is_excluded_protocol(
    exclude_protocols: &[String],
    component: &ProtocolComponent,
) -> bool {
    exclude_protocols
        .iter()
        .any(|entry| match entry.ends_with(':') {
            true => component
                .protocol_system
                .starts_with(entry.as_str()),
            false => component.protocol_system == *entry,
        })
}

/// Removes from `topology` every component `drop` returns `true` for.
///
/// A component id the market does not know is kept: the caller's own topology is the authority on
/// which ids exist, and a filter cannot classify what it cannot read.
pub(crate) fn remove_components(
    market: &MarketState,
    topology: FxHashMap<ComponentId, Vec<Address>>,
    drop: &dyn Fn(&ProtocolComponent) -> bool,
) -> FxHashMap<ComponentId, Vec<Address>> {
    topology
        .into_iter()
        .filter(|(id, _)| {
            market
                .get_component(id)
                .is_none_or(|c| !drop(c))
        })
        .collect()
}

/// Removes those component ids from a market event's added/updated/removed lists, so a component
/// the worker must not route through is never ingested mid-stream.
pub(crate) fn filter_event(
    market: &MarketState,
    event: MarketEvent,
    drop: &dyn Fn(&ProtocolComponent) -> bool,
) -> MarketEvent {
    let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
        event;

    let added_components = remove_components(market, added_components, drop);
    let removed_components = filter_component_ids(market, &removed_components, drop);
    let updated_components = filter_component_ids(market, &updated_components, drop);

    MarketEvent::MarketUpdated { added_components, removed_components, updated_components }
}

/// Keeps only the component ids `drop` returns `false` for.
fn filter_component_ids(
    market: &MarketState,
    ids: &[ComponentId],
    drop: &dyn Fn(&ProtocolComponent) -> bool,
) -> Vec<ComponentId> {
    ids.iter()
        .filter(|id| {
            market
                .get_component(id)
                .is_none_or(|c| !drop(c))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        algorithm::test_utils::{component, component_with_protocol, token},
        feed::exclusivity::{is_exclusive, mark_exclusive},
    };

    fn exclusive_component(id: &str) -> ProtocolComponent {
        let mut c = component(id, &[token(0x01, "A"), token(0x02, "B")]);
        mark_exclusive(&mut c);
        c
    }

    fn public_component(id: &str) -> ProtocolComponent {
        component(id, &[token(0x01, "A"), token(0x02, "B")])
    }

    fn pamm_component(id: &str) -> ProtocolComponent {
        component_with_protocol(
            id,
            "propammfallback:fermiswap",
            &[token(0x01, "A"), token(0x02, "B")],
        )
    }

    fn market_with(components: Vec<ProtocolComponent>) -> MarketState {
        let mut market = MarketState::new();
        market.upsert_components(components);
        market
    }

    #[rstest::rstest]
    #[case::family_prefix(&["propammfallback:"], "propammfallback:fermiswap", true)]
    #[case::exact_system(&["uniswap_v2"], "uniswap_v2", true)]
    #[case::other_system(&["uniswap_v2"], "uniswap_v3", false)]
    #[case::exact_entry_is_not_a_prefix(&["propammfallback"], "propammfallback:fermiswap", false)]
    #[case::no_exclusions(&[], "propammfallback:fermiswap", false)]
    fn test_is_excluded_protocol(
        #[case] exclude_protocols: &[&str],
        #[case] protocol_system: &str,
        #[case] expected: bool,
    ) {
        let exclude_protocols: Vec<String> = exclude_protocols
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let component = component_with_protocol("c-1", protocol_system, &[token(0x01, "A")]);

        assert_eq!(is_excluded_protocol(&exclude_protocols, &component), expected);
    }

    #[test]
    fn test_remove_components_drops_exclusive_and_excluded_protocols() {
        let market = market_with(vec![
            public_component("pub-1"),
            exclusive_component("excl-1"),
            pamm_component("pamm-1"),
        ]);
        let exclude_protocols = vec!["propammfallback:".to_string()];
        let drop =
            |c: &ProtocolComponent| is_exclusive(c) || is_excluded_protocol(&exclude_protocols, c);

        let filtered = remove_components(&market, market.component_topology(), &drop);

        assert!(filtered.contains_key("pub-1"));
        assert!(!filtered.contains_key("excl-1"));
        assert!(!filtered.contains_key("pamm-1"));
    }

    #[test]
    fn test_filter_event() {
        let market = market_with(vec![
            public_component("pub-1"),
            exclusive_component("excl-1"),
            pamm_component("pamm-1"),
        ]);
        let ids = || vec!["pub-1".to_string(), "excl-1".to_string(), "pamm-1".to_string()];
        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter(ids().into_iter().map(|id| (id, vec![]))),
            removed_components: ids(),
            updated_components: ids(),
        };
        let exclude_protocols = vec!["propammfallback:".to_string()];
        let drop =
            |c: &ProtocolComponent| is_exclusive(c) || is_excluded_protocol(&exclude_protocols, c);

        let MarketEvent::MarketUpdated { added_components, removed_components, updated_components } =
            filter_event(&market, event, &drop);

        assert!(added_components.contains_key("pub-1"));
        assert!(!added_components.contains_key("excl-1"));
        assert!(!added_components.contains_key("pamm-1"));
        assert_eq!(removed_components, vec!["pub-1".to_string()]);
        assert_eq!(updated_components, vec!["pub-1".to_string()]);
    }
}
