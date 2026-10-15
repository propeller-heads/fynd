//! Prints every count pinned against the market recording fixture (in `expected_outputs.json` and
//! `tests/token_graph.rs`), and the component count of each protocol system.
//!
//! The counts depend on the code that decodes the recording, not only on the recording. A
//! tycho-simulation bump that decodes more or fewer pools changes them, and then
//! `test_derived_data_matches_expected` and `tests/token_graph.rs` fail.
//!
//! Run `scripts/fixture-counts.sh` on `main` and on your branch, and compare the two reports.
//! The per-protocol table shows which protocol changed. Put that table in the pull request,
//! then update the values the report marks as changed.

use std::{
    collections::{BTreeMap, HashSet},
    str::FromStr,
};

use fynd_core::{
    graph::{GraphManager, TopologyGraphManager},
    types::ComponentId,
};
use fynd_test_fixtures::{expected::load_expected_file, read_recording};
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::Address;

use crate::harness::TestHarness;

fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Prints `name: expected -> actual`, marked when the two differ.
fn row(location: &str, name: &str, expected: usize, actual: usize) {
    let mark = if expected == actual { "   " } else { "!! " };
    println!("{mark}{location:<34} {name:<28} {expected:>6} -> {actual:>6}");
}

#[tokio::test]
#[ignore = "report only: run with scripts/fixture-counts.sh"]
async fn report_fixture_counts() {
    let recording = read_recording(&fixtures_dir().join("market_recording.json.zst"))
        .expect("market recording fixture");
    let updates = recording
        .decode_updates()
        .await
        .expect("market recording fixture decodes");

    let mut by_protocol: BTreeMap<String, usize> = BTreeMap::new();
    let mut topology: FxHashMap<ComponentId, Vec<Address>> = FxHashMap::default();
    for update in updates {
        for (component_id, component) in update.new_pairs {
            if !topology.contains_key(&component_id) {
                *by_protocol
                    .entry(component.protocol_system.clone())
                    .or_default() += 1;
            }
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

    let mut manager = TopologyGraphManager::<()>::new();
    manager.initialize_graph(&topology);
    let graph = manager.graph();

    let harness = TestHarness::from_fixture().await;
    let market_ref = harness.solver().market_data();
    let market = market_ref.read().await;
    let derived_ref = harness.solver().derived_data();
    let derived = derived_ref.read().await;
    let with_spot_prices: HashSet<_> = derived
        .spot_prices()
        .expect("spot prices computed")
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();
    let with_depths: HashSet<_> = derived
        .component_depths()
        .expect("component depths computed")
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();
    let token_prices = derived
        .token_prices()
        .expect("token prices computed")
        .len();

    let expected_file = load_expected_file(&fixtures_dir().join("expected_outputs.json"))
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let meta = &expected_file.metadata;
    let expected_derived = meta
        .derived_data
        .as_ref()
        .expect("expected_outputs.json has derived_data");

    println!("\nComponents per protocol system (decoded from the recording):");
    for (protocol, count) in &by_protocol {
        println!("    {protocol:<34} {count:>6}");
    }
    println!("    {:<34} {:>6}", "total", topology.len());

    println!("\nPinned counts (expected -> actual, !! = update this value):");
    let json = "expected_outputs.json";
    row(json, "num_components", meta.num_components, market.component_topology().len());
    row(json, "num_tokens", meta.num_tokens, market.token_registry_ref().len());
    row(
        json,
        "components_with_spot_prices",
        expected_derived.components_with_spot_prices,
        with_spot_prices.len(),
    );
    row(json, "components_with_depths", expected_derived.components_with_depths, with_depths.len());
    row(json, "token_prices", expected_derived.token_prices, token_prices);
    let token_graph = "tests/token_graph.rs";
    row(token_graph, "TOKENS", token_graph_const("TOKENS"), graph.node_count());
    row(token_graph, "EDGES", token_graph_const("EDGES"), graph.edge_count());
    // Counted as the test counts it: distinct components across the graph's edges.
    let components_on_edges: HashSet<&ComponentId> = graph
        .edge_indices()
        .flat_map(|edge| {
            graph
                .edge_weight(edge)
                .expect("edge has a weight")
                .pools()
                .iter()
                .map(|pool| &pool.component_id)
        })
        .collect();
    row(token_graph, "COMPONENTS", token_graph_const("COMPONENTS"), components_on_edges.len());
    let node = |hex: &str| {
        graph
            .get_token_ix(&Address::from_str(hex).expect("token address is valid hex"))
            .expect("token is in the graph")
    };
    let pools = |a: &str, b: &str| {
        graph
            .pools_between(node(a), node(b))
            .len()
    };
    row(token_graph, "USDC_USDT_POOLS", token_graph_const("USDC_USDT_POOLS"), pools(USDC, USDT));
    row(token_graph, "USDC_WETH_POOLS", token_graph_const("USDC_WETH_POOLS"), pools(USDC, WETH));
    row(token_graph, "WETH_USDT_POOLS", token_graph_const("WETH_USDT_POOLS"), pools(WETH, USDT));
    row(
        token_graph,
        "WETH_NEIGHBOURS",
        token_graph_const("WETH_NEIGHBOURS"),
        graph.neighbors(node(WETH)).count(),
    );
}

const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";

/// Reads `const <name>: usize = <value>;` from `tests/token_graph.rs`.
fn token_graph_const(name: &str) -> usize {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/token_graph.rs");
    let source = std::fs::read_to_string(path).expect("tests/token_graph.rs reads");
    let prefix = format!("const {name}: usize = ");
    let value = source
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("tests/token_graph.rs has no `{prefix}<number>;`"));
    value
        .trim_end_matches(';')
        .replace('_', "")
        .parse()
        .unwrap_or_else(|_| panic!("tests/token_graph.rs: `{prefix}{value}` is not a plain number"))
}
