//! Builds a [`TopologyGraph`] from the recorded market and checks its shape.
//!
//! The unit tests in `graph/token_graph.rs` use graphs of four or five tokens. This one uses a real
//! mainnet snapshot, where components share tokens, some hold more than two, and the busiest pairs
//! are traded by a dozen pools at once.
//!
//! The counts below come from the fixture. Regenerating it will change them, along with a good deal
//! else that reads the same file.

use std::str::FromStr;

use fynd_core::{
    graph::{GraphManager, TopologyGraphManager},
    types::ComponentId,
};
use fynd_test_fixtures::read_recording;
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::Address;

/// Tokens in the snapshot, one node each.
const TOKENS: usize = 1688;
/// Directed token pairs, one edge each — two per pair of tokens that trade.
const EDGES: usize = 4026;
/// Components in the snapshot, spread across those edges.
const COMPONENTS: usize = 2377;

const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";

fn address(hex: &str) -> Address {
    Address::from_str(hex).expect("token address is valid hex")
}

/// The component topology the recording announces: every component, and the tokens it holds.
fn recorded_topology() -> FxHashMap<ComponentId, Vec<Address>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/market_recording.json.zst");
    let recording = read_recording(&path).expect("market recording fixture");

    let mut topology = FxHashMap::default();
    for update in recording.updates {
        for (component_id, component) in update.new_pairs {
            topology.insert(
                component_id,
                component
                    .tokens
                    .into_iter()
                    .map(|token| token.address)
                    .collect(),
            );
        }
    }
    topology
}

fn graph_from_fixture() -> TopologyGraphManager<()> {
    let mut manager = TopologyGraphManager::<()>::new();
    manager.initialize_graph(&recorded_topology());
    manager
}

#[test]
fn test_graph_holds_one_node_per_token_and_one_edge_per_pair() {
    let manager = graph_from_fixture();
    let graph = manager.graph();

    assert_eq!(graph.node_count(), TOKENS);
    assert_eq!(graph.edge_count(), EDGES);

    // Every component is on an edge, and no edge is left holding none. An edge with no pools would
    // mean a component was removed without its edge going too.
    let mut seen: Vec<&ComponentId> = Vec::new();
    for edge in graph.edge_indices() {
        let pools = graph
            .edge_weight(edge)
            .expect("edge has a weight")
            .pools();
        assert!(!pools.is_empty(), "an edge with no pools is a pair that does not trade");
        seen.extend(
            pools
                .iter()
                .map(|pool| &pool.component_id),
        );
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), COMPONENTS);
}

#[test]
fn test_pools_are_the_same_both_ways() {
    let manager = graph_from_fixture();
    let graph = manager.graph();

    // A pool trades in both directions, so the two edges of a pair carry the same components.
    for edge in graph.edge_indices() {
        let (from, to) = graph
            .edge_endpoints(edge)
            .expect("edge has endpoints");
        let mut forward: Vec<&ComponentId> = graph
            .pools_between(from, to)
            .iter()
            .map(|pool| &pool.component_id)
            .collect();
        let mut backward: Vec<&ComponentId> = graph
            .pools_between(to, from)
            .iter()
            .map(|pool| &pool.component_id)
            .collect();
        forward.sort_unstable();
        backward.sort_unstable();
        assert_eq!(forward, backward);
    }
}

#[test]
fn test_busy_pairs_hold_every_pool_that_trades_them() {
    let manager = graph_from_fixture();
    let graph = manager.graph();
    let node = |hex: &str| {
        manager
            .graph()
            .get_token_ix(&address(hex))
            .expect("token is in the graph")
    };

    // The thick pairs are the point of one-edge-per-pair: each of these is a single edge.
    assert_eq!(
        graph
            .pools_between(node(USDC), node(USDT))
            .len(),
        15
    );
    assert_eq!(
        graph
            .pools_between(node(USDC), node(WETH))
            .len(),
        12
    );
    assert_eq!(
        graph
            .pools_between(node(WETH), node(USDT))
            .len(),
        10
    );

    // And WETH reaches its neighbours once each, not once per pool.
    assert_eq!(graph.neighbors(node(WETH)).count(), 1285);
}

#[test]
fn test_every_token_is_findable_by_address() {
    let manager = graph_from_fixture();
    let graph = manager.graph();

    for node in graph.node_indices() {
        assert_eq!(graph.get_token_ix(&graph[node]), Some(node));
    }
}
