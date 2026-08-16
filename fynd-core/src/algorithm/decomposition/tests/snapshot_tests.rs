//! Snapshot of the routes `decomposition` returns on the recorded market.
//!
//! This is the regression net the refactors in `PLAN-route-refactor.md` and
//! `PLAN-topology-graph.md` are measured against. Nothing else pins decomposition's *output*: the
//! replay harness in `tests/integration/` runs `bellman_ford` only, and the unit tests around it
//! build small synthetic markets that cannot notice "the real fixture now picks a different pool
//! on hop two".
//!
//! # What is pinned, and why that specifically
//!
//! Every swap's component, its token pair, its split, **in order** — plus the amounts. Two
//! different routes with the same output and hop count are a real regression, and
//! `ExpectedOutput` (which pins `amount_out_net_gas`, `gas_estimate` and `num_swaps`) cannot see
//! one. Ordering is pinned rather than compared as a set because the changes ahead shuffle
//! tie-breaks: merging the component types changes child iteration order, and the topology-graph
//! work changes which pools land in a hop and in what order. A set comparison would hide exactly
//! the differences worth looking at.
//!
//! Orders are the **largest** amount each pair offers. A small order takes one pool and pins
//! nothing about the split search, which is the part being rewritten.
//!
//! # Regenerating
//!
//! ```text
//! cargo nextest run -p fynd-core --features test-utils \
//!     decomposition::tests::snapshot_tests::regenerate --run-ignored all
//! ```
//!
//! A diff is not automatically a break. Two known ones are already baked in:
//!
//! * `GNO_to_AAVE` returns the *reference* route, not the candidate, because the candidate cannot
//!   assemble — see `tests/decomposition_gno.rs` for the open defect. Fixing `assemble`'s
//!   stretching will change this row, deliberately.
//! * A route that is absent here (`no_route`) is pinned as absent. That is a real property worth
//!   holding, but it also means a fix that *finds* a route shows up as a diff.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

/// The `[pools.*]` table of a worker-pool config.
#[derive(Deserialize)]
struct PoolsFile {
    pools: HashMap<String, PoolConfig>,
}

use crate::{
    types::{QuoteOptions, QuoteRequest, QuoteStatus},
    PoolConfig, Solver,
};

/// Hop limit for the snapshot. Three is what the benchmark configs use, and it is deep enough that
/// the branch grouping has something to group.
const MAX_HOPS: usize = 3;

/// Per-order solve budget. Generous, so the snapshot pins what the algorithm decides rather than
/// where the clock happened to stop.
const TIMEOUT_MS: u64 = 5_000;

/// Workers serving the orders.
///
/// The orders are independent and each worker holds its own graph over a shared read-only market,
/// so which worker answers an order does not change the answer — only how long the set takes.
/// Fixed rather than taken from the machine, so a run here and a run in CI do the same work.
const NUM_WORKERS: usize = 8;

/// One swap of a returned route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwapSnapshot {
    component_id: String,
    token_in: String,
    token_out: String,
    /// Six decimal places: enough to see a reallocation, coarse enough not to churn on the
    /// last-place float differences that reassociating a product produces.
    split: String,
}

/// What one order returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RouteSnapshot {
    scenario: String,
    status: String,
    amount_out: String,
    amount_out_net_gas: String,
    swaps: Vec<SwapSnapshot>,
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/algorithm/decomposition/tests/snapshots/decomposition_routes.json")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// One worker running `decomposition` alone, so the snapshot is this algorithm's answer rather than
/// the router's pick between several.
fn pool_config() -> HashMap<String, PoolConfig> {
    let toml = format!(
        r#"
[pools.decomposition]
algorithm = "decomposition"
num_workers = {NUM_WORKERS}
task_queue_capacity = 100
max_hops = {MAX_HOPS}
timeout_ms = {TIMEOUT_MS}
"#
    );
    // `fynd-test-fixtures` parses against the published crate, so the config crosses a crate
    // boundary and comes back as a different `PoolConfig`. Reparsed here against this crate's own.
    toml::from_str::<PoolsFile>(&toml)
        .expect("pool config parses")
        .pools
}

/// The largest order each pair of `pairs/<chain>.json` offers.
///
/// `fynd_test_fixtures::load_test_scenarios` takes the *smallest* amount of each pair, which is the
/// opposite of what a split search should be pinned on.
fn largest_orders(chain: &str) -> Vec<(String, crate::types::Order)> {
    let json = std::fs::read_to_string(fixture(&format!("pairs/{chain}.json")))
        .expect("scenario pairs file");
    let raw: serde_json::Value = serde_json::from_str(&json).expect("scenario pairs parse");

    let decimals: HashMap<&str, u32> = raw["tokens"]
        .as_array()
        .expect("tokens array")
        .iter()
        .map(|token| {
            (
                token["symbol"]
                    .as_str()
                    .expect("token symbol"),
                token["decimals"]
                    .as_u64()
                    .expect("token decimals") as u32,
            )
        })
        .collect();
    let address: HashMap<&str, tycho_simulation::tycho_core::models::Address> = raw["tokens"]
        .as_array()
        .expect("tokens array")
        .iter()
        .map(|token| {
            (
                token["symbol"]
                    .as_str()
                    .expect("token symbol"),
                token["address"]
                    .as_str()
                    .expect("token address")
                    .parse()
                    .expect("token address parses"),
            )
        })
        .collect();

    let mut orders = Vec::new();
    for pair in raw["pairs"]
        .as_array()
        .expect("pairs array")
    {
        let sell = pair["token_in"]
            .as_str()
            .expect("token_in");
        let buy = pair["token_out"]
            .as_str()
            .expect("token_out");
        let human = pair["amounts"]
            .as_array()
            .expect("amounts")
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .fold(f64::MIN, f64::max);
        let amount = human * 10f64.powi(decimals[sell] as i32);

        orders.push((
            format!("{sell}_to_{buy}_{human}"),
            crate::types::Order::new(
                address[sell].clone(),
                address[buy].clone(),
                num_bigint::BigUint::from(amount as u128),
                crate::types::OrderSide::Sell,
                tycho_simulation::tycho_core::models::Address::from([0u8; 20]),
            ),
        ));
    }
    orders.sort_by(|(left, _), (right, _)| left.cmp(right));
    orders
}

/// Solves every order and records what came back.
async fn solve_all() -> Vec<RouteSnapshot> {
    let recording =
        fynd_test_fixtures::read_recording(&fixture("market_recording.json.zst")).expect(
            "market recording fixture; it is stored in Git LFS, so `git lfs pull` if this fails",
        );
    let chain_name = recording.metadata.chain.clone();
    let chain = crate::types::parse_chain(&chain_name).expect("fixture chain is supported");
    let gas_price = recording
        .metadata
        .gas_price_as_biguint();

    let solver = Solver::from_recording(chain, recording.updates, pool_config(), gas_price)
        .await
        .expect("solver builds from the recording");
    solver
        .wait_until_ready(Duration::from_secs(180))
        .await
        .expect("solver ready");

    // Solved concurrently, not with rayon: `quote` is async and hands the order to a worker thread
    // through a channel, so a rayon pool would only block its own threads waiting on the futures.
    // `join_all` keeps the results in the order the requests were made, which the snapshot needs.
    let solves = largest_orders(&chain_name)
        .into_iter()
        .map(|(scenario, order)| {
            let solver = &solver;
            async move {
                let request = QuoteRequest::new(vec![order], QuoteOptions::default());
                let quote = solver
                    .quote(request)
                    .await
                    .expect("the solver answers");
                let result = &quote.orders()[0];

                let swaps = match result.route() {
                    Some(route) => route
                        .swaps()
                        .iter()
                        .map(|swap| SwapSnapshot {
                            component_id: swap.component_id().to_string(),
                            token_in: swap.token_in().to_string(),
                            token_out: swap.token_out().to_string(),
                            split: format!("{:.6}", swap.split()),
                        })
                        .collect(),
                    None => Vec::new(),
                };

                RouteSnapshot {
                    scenario,
                    status: format!("{:?}", result.status()),
                    amount_out: result.amount_out().to_string(),
                    amount_out_net_gas: result
                        .amount_out_net_gas()
                        .to_string(),
                    swaps,
                }
            }
        });

    futures::future::join_all(solves).await
}

/// Every order returns the route it returned when the snapshot was taken.
#[tokio::test(flavor = "multi_thread")]
async fn test_decomposition_routes_match_the_snapshot() {
    let expected: Vec<RouteSnapshot> = serde_json::from_str(
        &std::fs::read_to_string(snapshot_path()).expect(
            "route snapshot; run the ignored `regenerate_decomposition_snapshot` test to create it",
        ),
    )
    .expect("route snapshot parses");

    let actual = solve_all().await;

    // Compared row by row rather than as two vectors, so a failure names the order that moved
    // instead of printing forty-four of them.
    assert_eq!(
        actual.len(),
        expected.len(),
        "the snapshot covers {} orders but the fixture produced {}",
        expected.len(),
        actual.len()
    );
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual, expected, "route changed for {}", expected.scenario);
    }
}

/// Rewrites the snapshot from the current behaviour. Ignored: it asserts nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "regenerates the snapshot; run explicitly after an intended change"]
async fn regenerate_decomposition_snapshot() {
    let snapshots = solve_all().await;
    let path = snapshot_path();
    std::fs::create_dir_all(path.parent().expect("snapshot has a directory"))
        .expect("snapshot directory");
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&snapshots).expect("snapshot serialises")),
    )
    .expect("snapshot written");

    let routed = snapshots
        .iter()
        .filter(|snapshot| snapshot.status == format!("{:?}", QuoteStatus::Success))
        .count();
    let split = snapshots
        .iter()
        .filter(|snapshot| snapshot.swaps.len() > 1)
        .count();
    println!(
        "wrote {} orders to {} ({routed} routed, {split} with more than one swap)",
        snapshots.len(),
        path.display()
    );
}
