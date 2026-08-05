//! Isolated reproduction of the open GNO → AAVE defect.
//!
//! Run:
//!
//! ```text
//! cargo nextest run -p fynd-core --features test-utils --test decomposition_gno \
//!     --run-ignored all --no-capture > /tmp/gno.log 2>&1
//! ```
//!
//! Prints, for one order (3500 GNO → AAVE on the recorded fixture):
//!
//! 1. the route each algorithm returns, swap by swap;
//! 2. every `decomposition` debug event of that solve, which includes the candidate subgraph, the
//!    branches and their token paths, the outer split assigned to each, and why assembly succeeded
//!    or failed.
//!
//! # What is known
//!
//! `water_fill` solves this for ~983e18 by splitting roughly 83% `GNO→WETH` / 17% `GNO→USDT` and
//! converging on the deep `USDC→AAVE` pool `0x…b1d706`. `bellman_ford` reaches the same pool down a
//! single 5-hop path. `decomposition` reaches neither: it returns its *reference* route
//! (`GNO→WETH`, then WETH split across two thin `WETH→AAVE` pools) worth ~212e18.
//!
//! It returns the reference because its own candidate — which scores higher, and which contains
//! both of water_fill's token paths — cannot be assembled:
//!
//! * The candidate's branch splits sum to ~0.605, not 1.0. The market genuinely cannot absorb the
//!   whole order down these paths: pool `0x…b91744` (`GNO→USDT`) hard-errors above ~584 GNO with
//!   `Invalid input: Ticks exceeded`, which is tycho's UniswapV3 simulator running out of indexed
//!   tick data. That ~584 is *correct* — water_fill independently allocates 601 GNO there.
//! * `assemble::build_route_result` then stretches the shortfall back to 1.0, because
//!   `split_primitives::assign_splits_and_amounts` divides every split by their sum
//!   (`split_primitives.rs:594-601`). `b91744` is asked for ~2005 GNO, throws `Ticks exceeded`, and
//!   the *whole* candidate is discarded — including branches that had nothing wrong with them.
//!
//! # Where to look next
//!
//! The stretch is the binding constraint, not the allocation. Two independent things are wrong:
//!
//! 1. **A solution that intends to route less than the order is scaled up to the full order.** The
//!    solver sized every pool for ~60% of the order; the assembled route hands them 100%. See
//!    `assemble::LOW_ROUTED_FLOW` and its module docs, which currently argue for stretching — that
//!    argument is what is in question. Fynd's `Route` cannot express "spend only 60%", so honouring
//!    the shortfall needs the route to carry its own spent input, and the encoder's `split = 0.0` =
//!    *spend the remaining balance* convention reconciled with it.
//! 2. **One unservable pool discards every branch.** `build_split_route` simulates the whole plan
//!    and the first failure aborts it (`split_primitives.rs:686-689`), so `b91744` takes the
//!    healthy `GNO>WETH>USDC>AAVE` branch down with it. Pinned in isolation by
//!    `assemble::tests::test_one_unservable_pool_discards_the_whole_stretched_solution` and its
//!    companion `..._the_healthy_branch_alone_assembles_at_the_full_order`.
//!
//! Ruled out already, each with evidence in the git history: the pool sell-limit checks, the
//! branch cap, the pruning bound, the choice of split optimizer, and — as of the branch-grouping
//! commit — shared pools across outer splits.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use fynd_core::{types::QuoteStatus, PoolConfig, QuoteOptions, QuoteRequest, Solver};
use fynd_test_fixtures::{read_recording, TestScenario};
use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt, EnvFilter, Layer};

/// The order under investigation. Prefix match against `tests/fixtures/pairs/ethereum.json`.
const SCENARIO: &str = "GNO_to_AAVE";

/// Every algorithm is asked the same question, so their routes are directly comparable.
const ALGORITHMS: &[&str] = &["bellman_ford", "water_fill", "decomposition"];

const MAX_HOPS: usize = 3;
const TIMEOUT_MS: u64 = 5000;

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn take(&self) -> String {
        let mut buffer = self.0.lock().expect("log mutex");
        String::from_utf8_lossy(&std::mem::take(&mut *buffer)).into_owned()
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log mutex")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Solving happens on dedicated worker threads, so the subscriber must be global — a
/// `set_default` guard on the test thread captures nothing.
fn install_global_capture() -> CapturedLog {
    let sink = CapturedLog::default();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(sink.clone())
        .with_target(false)
        .without_time()
        .with_ansi(false)
        .with_filter(EnvFilter::new("fynd_core::algorithm::decomposition=debug"));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .expect("no other global subscriber in this test binary");
    sink
}

fn pool_config(algorithm: &str) -> HashMap<String, PoolConfig> {
    let toml = format!(
        r#"
[pools.gno]
algorithm = "{algorithm}"
num_workers = 1
task_queue_capacity = 100
max_hops = {MAX_HOPS}
timeout_ms = {TIMEOUT_MS}
"#
    );
    fynd_test_fixtures::parse_pools_toml(&toml).expect("pool config parses")
}

async fn solver_for(algorithm: &str) -> (Solver, String) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/market_recording.json.zst");
    let recording = read_recording(&path).expect("market recording fixture");
    let chain_name = recording.metadata.chain.clone();
    let chain = fynd_core::types::parse_chain(&chain_name).expect("fixture chain is supported");
    let gas_price = recording
        .metadata
        .gas_price_as_biguint();
    let solver =
        Solver::from_recording(chain, recording.updates, pool_config(algorithm), gas_price)
            .await
            .expect("solver builds from the recording");
    solver
        .wait_until_ready(Duration::from_secs(180))
        .await
        .expect("solver ready");
    (solver, chain_name)
}

/// Last six hex characters, enough to tell pools and tokens apart in a table.
fn short(value: &impl std::fmt::Display) -> String {
    let text = value.to_string();
    text.chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn scenario_for(chain_name: &str) -> TestScenario {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pairs")
        .join(format!("{chain_name}.json"));
    let json = std::fs::read_to_string(&path).expect("scenario pairs file");
    let scenarios: Vec<TestScenario> =
        fynd_test_fixtures::load_test_scenarios(&json).expect("scenario pairs parse");
    scenarios
        .into_iter()
        .find(|s| s.name.starts_with(SCENARIO))
        .unwrap_or_else(|| panic!("{SCENARIO} is not in the fixture"))
}

#[tokio::test]
#[ignore = "diagnostic: builds one pipeline per algorithm, run explicitly"]
async fn diagnose_gno_to_aave() {
    let mut sink = None;
    for algorithm in ALGORITHMS {
        let (solver, chain_name) = solver_for(algorithm).await;
        // Installed after the first build so the trace is solve events, not startup noise.
        if sink.is_none() {
            sink = Some(install_global_capture());
        }
        let scenario = scenario_for(&chain_name);
        let sink = sink
            .as_ref()
            .expect("capture installed");
        let _ = sink.take();

        println!("\n=== {algorithm} on {} (sell {}) ===", scenario.name, scenario.amount);
        let request = QuoteRequest::new(vec![scenario.to_order()], QuoteOptions::default());
        match solver.quote(request).await {
            Ok(quote) => {
                let order_quote = &quote.orders()[0];
                println!(
                    "status={:?} net={} gas={} solve_ms={}",
                    order_quote.status(),
                    order_quote.amount_out_net_gas(),
                    order_quote.gas_estimate(),
                    quote.solve_time_ms(),
                );
                if order_quote.status() == QuoteStatus::Success {
                    if let Some(route) = order_quote.route() {
                        for swap in route.swaps() {
                            println!(
                                "    {:>8} {:>8} -> {:>8}  in={:<26} out={:<26} split={:.4}",
                                short(&swap.component_id()),
                                short(&swap.token_in()),
                                short(&swap.token_out()),
                                swap.amount_in(),
                                swap.amount_out(),
                                swap.split(),
                            );
                        }
                    }
                }
            }
            Err(error) => println!("solver error: {error}"),
        }

        if *algorithm == "decomposition" {
            println!("\n--- decomposition trace ---");
            for line in sink.take().lines() {
                println!("  {line}");
            }
        }
    }
}
