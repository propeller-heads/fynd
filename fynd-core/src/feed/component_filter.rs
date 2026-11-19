//! Per-worker component filtering: each worker ingests only the components its worker pool admits.
//!
//! `MarketState` is never duplicated. A worker drops what it must not route through from its own
//! graph topology at startup and from every incoming `MarketEvent`, so two worker pools reading
//! the same market can still route through different liquidity. Today two settings feed the
//! predicate: the worker pool's `LiquidityScope` (via `exclusivity::is_exclusive`) and its
//! `exclude_protocols` list (via `is_excluded_protocol`).

use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::protocol::ProtocolComponent;

use crate::{
    feed::{
        events::MarketEvent,
        exclusivity::{is_exclusive, set_exclusive},
        market_data::MarketState,
    },
    types::{ComponentId, ComponentTopology},
};

/// Whether `component` belongs to a protocol system in `exclude_protocols`.
///
/// An entry names a protocol system exactly (`uniswap_v2`), or, when it ends with `:`, the whole
/// family under that prefix: `fallback:` excludes `fallback:fermiswap`, `fallback:rfq:metric` and
/// every other `fallback:` venue. An empty list excludes nothing.
pub(crate) fn is_excluded_protocol(
    exclude_protocols: &[String],
    component: &ProtocolComponent,
) -> bool {
    exclude_protocols
        .iter()
        .any(|entry| protocol_matches(entry, &component.protocol_system))
}

/// Matches an exact protocol system or a colon-terminated family prefix.
pub(crate) fn protocol_matches(entry: &str, system: &str) -> bool {
    if entry.ends_with(':') {
        system.starts_with(entry)
    } else {
        system == entry
    }
}

/// Removes from `topology` every component `drop` returns `true` for.
///
/// A component id the market does not know is kept: the caller's own topology is the authority on
/// which ids exist, and a filter cannot classify what it cannot read.
pub(crate) fn remove_components(
    market: &MarketState,
    topology: FxHashMap<ComponentId, ComponentTopology>,
    drop: &dyn Fn(&ProtocolComponent) -> bool,
) -> FxHashMap<ComponentId, ComponentTopology> {
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
///
/// A reclassified component is added when `drop` keeps it, and removed when `drop` kept it only
/// before the change. The returned event keeps the reclassified ids, so it can be filtered again.
pub(crate) fn filter_event(
    market: &MarketState,
    event: MarketEvent,
    drop: &dyn Fn(&ProtocolComponent) -> bool,
) -> MarketEvent {
    let MarketEvent::MarketUpdated {
        mut added_components,
        mut removed_components,
        mut updated_components,
        reclassified_components,
    } = event;

    added_components.retain(|id, _| !reclassified_components.contains(id));
    removed_components.retain(|id| !reclassified_components.contains(id));
    updated_components.retain(|id| !reclassified_components.contains(id));

    let mut added_components = remove_components(market, added_components, drop);
    let mut removed_components = filter_component_ids(market, &removed_components, drop);
    let updated_components = filter_component_ids(market, &updated_components, drop);

    for id in &reclassified_components {
        let Some(component) = market.get_component(id) else {
            continue;
        };
        let mut before = component.clone();
        set_exclusive(&mut before, !is_exclusive(component));
        match (drop(&before), drop(component)) {
            (_, false) => {
                if let Some(topology) = market.topology_of(id) {
                    added_components.insert(id.clone(), topology);
                }
            }
            (false, true) => removed_components.push(id.clone()),
            (true, true) => {}
        }
    }

    MarketEvent::MarketUpdated {
        added_components,
        removed_components,
        updated_components,
        reclassified_components,
    }
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
        component_with_protocol(id, "fallback:fermiswap", &[token(0x01, "A"), token(0x02, "B")])
    }

    fn market_with(components: Vec<ProtocolComponent>) -> MarketState {
        let mut market = MarketState::new();
        market.upsert_components(components);
        market
    }

    #[rstest::rstest]
    #[case::family_prefix(&["fallback:"], "fallback:fermiswap", true)]
    #[case::exact_system(&["uniswap_v2"], "uniswap_v2", true)]
    #[case::other_system(&["uniswap_v2"], "uniswap_v3", false)]
    #[case::exact_entry_is_not_a_prefix(&["fallback"], "fallback:fermiswap", false)]
    #[case::no_exclusions(&[], "fallback:fermiswap", false)]
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
        let exclude_protocols = vec!["fallback:".to_string()];
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
            added_components: FxHashMap::from_iter(
                ids()
                    .into_iter()
                    .map(|id| (id, ComponentTopology::default())),
            ),
            removed_components: ids(),
            updated_components: ids(),
            reclassified_components: Vec::new(),
        };
        let exclude_protocols = vec!["fallback:".to_string()];
        let drop =
            |c: &ProtocolComponent| is_exclusive(c) || is_excluded_protocol(&exclude_protocols, c);

        let MarketEvent::MarketUpdated {
            added_components,
            removed_components,
            updated_components,
            reclassified_components: _,
        } = filter_event(&market, event, &drop);

        assert!(added_components.contains_key("pub-1"));
        assert!(!added_components.contains_key("excl-1"));
        assert!(!added_components.contains_key("pamm-1"));
        assert_eq!(removed_components, vec!["pub-1".to_string()]);
        assert_eq!(updated_components, vec!["pub-1".to_string()]);
    }

    /// An event that reclassifies `id`, as the feed sends it.
    fn reclassified_event(id: &str) -> MarketEvent {
        MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: Vec::new(),
            updated_components: vec![id.to_string()],
            reclassified_components: vec![id.to_string()],
        }
    }

    /// (added, removed, updated) ids of a filtered event.
    fn changes(event: MarketEvent) -> (Vec<ComponentId>, Vec<ComponentId>, Vec<ComponentId>) {
        let MarketEvent::MarketUpdated {
            added_components,
            removed_components,
            updated_components,
            reclassified_components: _,
        } = event;
        (added_components.into_keys().collect(), removed_components, updated_components)
    }

    #[rstest::rstest]
    #[case::public_worker_drops_newly_exclusive(true, true, (vec![], vec!["c-1"], vec![]))]
    #[case::public_worker_adds_newly_public(false, true, (vec!["c-1"], vec![], vec![]))]
    #[case::exclusive_worker_re_adds_newly_exclusive(true, false, (vec!["c-1"], vec![], vec![]))]
    #[case::exclusive_worker_re_adds_newly_public(false, false, (vec!["c-1"], vec![], vec![]))]
    fn test_filter_event_reclassified(
        #[case] now_exclusive: bool,
        #[case] public_only: bool,
        #[case] expected: (Vec<&str>, Vec<&str>, Vec<&str>),
    ) {
        let component =
            if now_exclusive { exclusive_component("c-1") } else { public_component("c-1") };
        let market = market_with(vec![component]);
        let drop = |c: &ProtocolComponent| public_only && is_exclusive(c);
        let to_ids = |ids: Vec<&str>| {
            ids.into_iter()
                .map(str::to_string)
                .collect::<Vec<ComponentId>>()
        };

        let filtered = filter_event(&market, reclassified_event("c-1"), &drop);

        assert_eq!(changes(filtered), (to_ids(expected.0), to_ids(expected.1), to_ids(expected.2)));
    }

    #[test]
    fn test_filter_event_reclassified_excluded_protocol() {
        let market = market_with(vec![pamm_component("pamm-1")]);
        let drop = |_: &ProtocolComponent| true;

        let filtered = filter_event(&market, reclassified_event("pamm-1"), &drop);

        assert_eq!(changes(filtered), (vec![], vec![], vec![]));
    }

    /// A second filter resolves a reclassified component by its own rule.
    #[test]
    fn test_filter_event_reclassified_filtered_twice() {
        let market = market_with(vec![exclusive_component("c-1")]);
        let keep_all = |_: &ProtocolComponent| false;
        let public_only = |c: &ProtocolComponent| is_exclusive(c);

        let for_post_processors = filter_event(&market, reclassified_event("c-1"), &keep_all);
        let for_graph = filter_event(&market, for_post_processors.clone(), &public_only);

        assert_eq!(changes(for_post_processors), (vec!["c-1".to_string()], vec![], vec![]));
        assert_eq!(changes(for_graph), (vec![], vec!["c-1".to_string()], vec![]));
    }
}
