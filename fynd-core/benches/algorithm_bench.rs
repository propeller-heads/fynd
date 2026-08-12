//! Comparison of the split-capable routing algorithms over the aggregator trade dataset.
//!
//! Runs one solver per config against a single market and reports output net of gas against a
//! baseline, per token pair and overall.
//!
//! The market is either the recorded fixture (`tests/fixtures/market_recording.json.zst`, the
//! default, no network) or one block captured live from Tycho with `--market live`. Offline runs
//! replay the same block every time and so compare with each other; a live run is a point-in-time
//! market whose configs compare only with each other. The run solves at `--gas-price-gwei`, or at
//! the market's own gas price when that flag is absent.
//!
//! # Running it
//!
//! ```text
//! ./scripts/bench.sh --name my-change --orders 2000
//! ./scripts/bench.sh --name deep --orders 400 --jobs 1 --configs water_fill_d3
//! ```
//!
//! The script builds this optimised with debug symbols and runs it. For a flamegraph of a single
//! algorithm use `./scripts/profile.sh`, which runs one config on one thread and writes nothing.
//!
//! # Which algorithms run
//!
//! One TOML file per configuration in `benches/configs/`, named by file stem. `--configs` takes a
//! comma-separated list of those stems and defaults to all of them. Adding a configuration is
//! adding a file; nothing here needs to change. See `benches/configs/README.md`.
//!
//! `bellman_ford_d2` is the baseline and is always included, listed or not. It is not a
//! like-for-like depth comparison — the report's "Reading this" section says why.
//!
//! # Solve times are contended unless you ask otherwise
//!
//! Orders run `--jobs` at a time, so by default the timings are wall clock under load: comparable
//! across configs within one run, meaningless as absolutes. `--jobs 1` gives isolated per-solve
//! latencies and a single solving thread, which is what a flamegraph needs. Configs never overlap
//! either way — each solver is torn down before the next is built.
//!
//! # The dataset
//!
//! `aggregator_trades_50k_1k_usd.json` in the repo root: 50,000 real aggregator sell orders of
//! $1,000 or more, drawn from the week either side of the market fixture's last block. Both of
//! each order's tokens are in the fixture by construction, so a miss means the algorithm found no
//! route rather than the market not holding the token. `fynd-core/benches/dataset.sql` is the
//! query that produced it, with the reasoning behind each filter.
//!
//! Orders are still filtered on load — anything that is not a sell is dropped, and so is anything
//! naming a token the recording never saw — and whatever that removes is counted in the report.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

mod common;

use chrono::Utc;
use clap::Parser;
use common::{
    available_configs, block_components, build_market, build_solver, load_bench_config,
    load_blocked_tokens, mean_and_median, print_protocol_breakdown, protocol_breakdown,
    resolved_gas_price_gwei, symbol_table, timings_of, token_label,
    trades::{load_trade_orders, recorded_tokens, TradeLoadSummary, TradeOrder},
    usd_out, wei_per_token, BenchConfig, BlockedTokens, LiveFlags, Market, MarketSource,
    ProtocolCount,
};
use futures::stream::StreamExt;
use fynd_core::{types::QuoteStatus, QuoteOptions, QuoteRequest, Solver};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_simulation::tycho_common::models::Address;

/// Config every bps figure is measured against. Always run, listed or not.
const BASELINE: &str = "bellman_ford_d2";

/// How a config's answer compares with the baseline's on one order.
///
/// Decided on the integers, not on the bps figure: `amount_out_net_gas` routinely exceeds 2^53, so
/// two different outputs can land on the same `f64` and a real difference would read as a tie.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Strictly more output than the baseline.
    Better,
    /// Strictly less.
    Worse,
    /// Exactly equal — the same route, or two routes that happen to pay out identically.
    Tie,
    /// Solved here, no route from the baseline. Coverage, not quality.
    SolvedOnly,
    /// The baseline solved it and this config did not.
    Missed,
    /// Neither solved it.
    NeitherSolved,
}

impl Outcome {
    fn of(candidate: Option<&BigUint>, baseline: Option<&BigUint>) -> Self {
        match (candidate, baseline) {
            (Some(candidate), Some(baseline)) => match candidate.cmp(baseline) {
                std::cmp::Ordering::Greater => Self::Better,
                std::cmp::Ordering::Less => Self::Worse,
                std::cmp::Ordering::Equal => Self::Tie,
            },
            (Some(_), None) => Self::SolvedOnly,
            (None, Some(_)) => Self::Missed,
            (None, None) => Self::NeitherSolved,
        }
    }

    /// Whether both sides solved the order, so their outputs can be compared in bps.
    fn is_comparable(self) -> bool {
        match self {
            Self::Better | Self::Worse | Self::Tie => true,
            Self::SolvedOnly | Self::Missed | Self::NeitherSolved => false,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::Worse => "worse",
            Self::Tie => "tie",
            Self::SolvedOnly => "only",
            Self::Missed => "missed",
            Self::NeitherSolved => "none",
        }
    }
}

/// Orders between progress lines.
const PROGRESS_EVERY: usize = 100;

/// Pairs listed in the report's per-pair table. The rest are in `pairs.csv`.
const PAIR_ROWS: usize = 30;

#[derive(Parser, Debug)]
#[command(
    about = "Compare routing algorithms against a recorded or a live market",
    long_about = "Runs one solver per algorithm over the same market and reports output net of \
                  gas against the baseline, per token pair and overall.\n\n\
                  The market is either the recorded fixture (--market offline, reproducible) or \
                  one block captured live from Tycho (--market live).\n\n\
                  Writes bench-results/<name>/\
                  {report.md,orders.csv,pairs.csv,protocols.csv,routes.jsonl}."
)]
struct Args {
    /// Name for this run. Names the output directory, so runs can be told apart and diffed.
    #[arg(long, default_value = "run")]
    name: String,

    /// Distinct orders to solve. 0 means every eligible order in the dataset.
    #[arg(long, default_value_t = 1000)]
    orders: usize,

    /// Orders in flight at once. Defaults to one per core.
    #[arg(long)]
    jobs: Option<usize>,

    /// Per-solve budget in milliseconds.
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,

    /// Gas price in gwei, fractions allowed. Without it a live run prices at whatever the chain
    /// is charging, and an offline run at the default, since the fixture carries no gas price.
    #[arg(long, value_parser = common::parse_gas_price_gwei)]
    gas_price_gwei: Option<f64>,

    /// Market flags: `--market`, and the Tycho settings a live capture needs.
    #[command(flatten)]
    live: LiveFlags,

    /// Configs to run, comma separated, named after the files in `benches/configs/`, e.g.
    /// `water_fill_d3,path_frank_wolfe_d3`. Defaults to every config on disk. The baseline is
    /// always included. Narrowing this is what makes a run readable under a profiler.
    #[arg(long, value_delimiter = ',')]
    configs: Option<Vec<String>>,

    /// Passes over the order set, per config. Extra passes add timing samples without changing
    /// the answers, which is what a profiler wants; quality is taken from the first pass.
    #[arg(long, default_value_t = 1)]
    repeats: usize,

    /// Trade dataset path.
    #[arg(long)]
    trades: Option<PathBuf>,

    /// Directory the run's output directory is created under. Deliberately not under `target/`:
    /// these are results to keep and compare, and `cargo clean` would take them.
    #[arg(long, default_value = "bench-results")]
    out_dir: PathBuf,
}

/// The configs a run covers, and the ones it could not.
///
/// Separate from [`Run`] because this is the only part that changes as the run proceeds: a config
/// that fails to build moves from `ready` to `skipped` after the settings are fixed.
struct ConfigSet {
    /// Loaded and ready to build, baseline first.
    ready: Vec<BenchConfig>,
    /// Asked for but not run, each with why, so the report says what is missing rather than
    /// quietly describing fewer algorithms than were requested.
    skipped: Vec<(String, String)>,
}

/// Resolves `--configs` into loaded configs, always including the baseline.
///
/// A name that will not load is skipped rather than fatal, but never silently: it is printed and
/// recorded in the report, because a typo that quietly shrinks the run is worse than one that
/// stops it.
fn resolve_configs(requested: Option<&[String]>) -> ConfigSet {
    let requested: Vec<String> = match requested {
        Some(names) => names
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect(),
        None => available_configs(),
    };

    // The baseline first, so the reports lead with what everything else is measured against.
    let baseline = load_bench_config(BASELINE)
        .unwrap_or_else(|reason| panic!("the baseline config {BASELINE} is unusable: {reason}"));
    let mut ready = vec![baseline];
    let mut skipped: Vec<(String, String)> = Vec::new();

    for name in requested {
        if ready
            .iter()
            .any(|config| config.label == name)
        {
            continue;
        }
        match load_bench_config(&name) {
            Ok(config) => ready.push(config),
            Err(reason) => skipped.push((name, reason)),
        }
    }

    ConfigSet { ready, skipped }
}

/// How the run is sized, after defaults are resolved. Fixed for the whole run.
struct Run {
    name: String,
    /// Orders in flight at once.
    jobs: usize,
    /// Workers in the pool. Each one builds and maintains its own copy of the graph, so more than
    /// there are cores costs setup time and memory without adding throughput.
    workers: usize,
    timeout_ms: u64,
    gas_price_gwei: f64,
    orders: usize,
    repeats: usize,
    trades: PathBuf,
    out_dir: PathBuf,
}

impl Run {
    fn resolve(args: Args, market: &Market) -> (Self, ConfigSet) {
        let cores = num_cpus::get();
        let jobs = args.jobs.unwrap_or(cores).max(1);
        let trades = args
            .trades
            .unwrap_or_else(common::default_trades_path);
        let out_dir = args.out_dir.join(&args.name);
        let configs = resolve_configs(args.configs.as_deref());
        let run = Self {
            name: args.name,
            jobs,
            workers: jobs.min(cores),
            timeout_ms: args.timeout_ms,
            gas_price_gwei: resolved_gas_price_gwei(args.gas_price_gwei, market),
            orders: args.orders,
            repeats: args.repeats.max(1),
            trades,
            out_dir,
        };
        (run, configs)
    }
}

/// One leg of a route: a swap through one pool, from one token to the next.
///
/// Amounts are per leg, so a split route's legs carry the amount that actually went through each
/// pool rather than a fraction to multiply out. That is what a flow diagram needs.
struct RouteEdge {
    token_in: Address,
    token_out: Address,
    /// The pool, as Tycho identifies it. Usually its on-chain address.
    component_id: String,
    protocol: String,
    amount_in: BigUint,
    amount_out: BigUint,
    gas: BigUint,
    /// Fraction of the parent order this leg carries, as the solver assigned it. 0 when the route
    /// does not split.
    split: f64,
}

/// One config's answer to one order.
struct Measurement {
    net_out: Option<BigUint>,
    /// Gross output, before gas is netted off.
    amount_out: Option<BigUint>,
    gas: Option<BigUint>,
    /// The route, leg by leg. Empty when nothing was solved.
    edges: Vec<RouteEdge>,
    /// Microseconds, measured by the caller so queueing and dispatch are counted. A shallow solve
    /// lands well under a millisecond, and rounding those to 0 loses the distribution.
    elapsed_us: u128,
}

impl Measurement {
    fn unsolved(elapsed_us: u128) -> Self {
        Self { net_out: None, amount_out: None, gas: None, edges: Vec::new(), elapsed_us }
    }
}

async fn measure(solver: &Solver, order: &TradeOrder) -> Measurement {
    let request = QuoteRequest::new(vec![order.to_order()], QuoteOptions::default());
    let start = std::time::Instant::now();
    let quote = solver.quote(request).await;
    let elapsed_us = start.elapsed().as_micros();

    match quote {
        Ok(quote) => {
            let order_quote = &quote.orders()[0];
            if order_quote.status() != QuoteStatus::Success {
                return Measurement::unsolved(elapsed_us);
            }
            let edges: Vec<RouteEdge> = order_quote
                .route()
                .map(|route| {
                    route
                        .swaps()
                        .iter()
                        .map(|swap| RouteEdge {
                            token_in: swap.token_in().clone(),
                            token_out: swap.token_out().clone(),
                            component_id: swap.component_id().to_string(),
                            protocol: swap.protocol().to_string(),
                            amount_in: swap.amount_in().clone(),
                            amount_out: swap.amount_out().clone(),
                            gas: swap.gas_estimate().clone(),
                            split: *swap.split(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Measurement {
                net_out: Some(order_quote.amount_out_net_gas().clone()),
                amount_out: Some(order_quote.amount_out().clone()),
                gas: Some(order_quote.gas_estimate().clone()),
                edges,
                elapsed_us,
            }
        }
        Err(_) => Measurement::unsolved(elapsed_us),
    }
}

/// One config's full run: the answers, plus every solve time across all passes.
struct ConfigRun {
    /// One per order, from the first pass. Later passes repeat the same market, so they repeat
    /// the same answers; they exist to add timing samples.
    measurements: Vec<Measurement>,
    /// Every solve time recorded, across every pass.
    times_us: Vec<u128>,
    /// Wall clock for the whole config, setup excluded.
    solving_ms: u128,
}

/// Solves the order set `repeats` times against one solver, `jobs` in flight.
async fn run_config(
    solver: &Solver,
    orders: &[TradeOrder],
    jobs: usize,
    repeats: usize,
) -> ConfigRun {
    let started = std::time::Instant::now();
    let mut measurements: Option<Vec<Measurement>> = None;
    let mut times_us: Vec<u128> = Vec::with_capacity(orders.len() * repeats);

    for pass in 0..repeats {
        let pass_measurements = solve_pass(solver, orders, jobs).await;
        times_us.extend(
            pass_measurements
                .iter()
                .map(|measurement| measurement.elapsed_us),
        );
        if measurements.is_none() {
            measurements = Some(pass_measurements);
        }
        if repeats > 1 {
            println!("    pass {}/{repeats} done", pass + 1);
        }
    }

    ConfigRun {
        measurements: measurements.expect("at least one pass"),
        times_us,
        solving_ms: started.elapsed().as_millis(),
    }
}

/// One pass over the orders, `jobs` in flight, returning measurements in input order.
async fn solve_pass(solver: &Solver, orders: &[TradeOrder], jobs: usize) -> Vec<Measurement> {
    let done = AtomicUsize::new(0);
    let total = orders.len();
    let mut measurements: Vec<Option<Measurement>> = (0..total).map(|_| None).collect();

    let mut results = futures::stream::iter(orders.iter().enumerate())
        .map(|(index, order)| {
            let done = &done;
            async move {
                let measurement = measure(solver, order).await;
                let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                if finished.is_multiple_of(PROGRESS_EVERY) {
                    println!("    {finished}/{total}");
                }
                (index, measurement)
            }
        })
        .buffer_unordered(jobs);

    while let Some((index, measurement)) = results.next().await {
        measurements[index] = Some(measurement);
    }

    measurements
        .into_iter()
        .map(|measurement| measurement.expect("every order measured"))
        .collect()
}

/// Difference of `candidate` against `baseline` in basis points of the baseline.
///
/// The subtraction happens on the integers. At these magnitudes the difference between two outputs
/// is often smaller than the gap between adjacent `f64`s, so converting first and subtracting after
/// rounds real differences to zero. Only the ratio is taken in floating point, where the relative
/// precision is ample.
fn diff_bps(candidate: &BigUint, baseline: &BigUint) -> Option<f64> {
    let base = baseline.to_f64()?;
    if base == 0.0 {
        return None;
    }
    let (magnitude, sign) = if candidate >= baseline {
        ((candidate - baseline).to_f64()?, 1.0)
    } else {
        ((baseline - candidate).to_f64()?, -1.0)
    };
    Some(sign * magnitude / base * 10_000.0)
}

/// One config's record over the whole order set.
struct ConfigStats {
    solved: usize,
    /// Orders the baseline and this config both solved — the ones the bps figures cover.
    compared: usize,
    mean_bps: f64,
    median_bps: f64,
    better: usize,
    worse: usize,
    tie: usize,
    /// Solved by this config where the baseline found nothing.
    solved_only: usize,
    /// Solved by the baseline where this config found nothing.
    missed: usize,
    p50_us: u128,
    p95_us: u128,
    slowest_us: u128,
    /// Wall clock for the config's whole run, setup excluded.
    solving_ms: u128,
}

fn config_stats(config_run: &ConfigRun, baseline: &[Measurement]) -> ConfigStats {
    let mut stats = ConfigStats {
        solved: 0,
        compared: 0,
        mean_bps: 0.0,
        median_bps: 0.0,
        better: 0,
        worse: 0,
        tie: 0,
        solved_only: 0,
        missed: 0,
        p50_us: 0,
        p95_us: 0,
        slowest_us: 0,
        solving_ms: config_run.solving_ms,
    };
    let mut all_bps: Vec<f64> = Vec::new();

    for (index, measurement) in config_run
        .measurements
        .iter()
        .enumerate()
    {
        let net = measurement.net_out.as_ref();
        let base = baseline[index].net_out.as_ref();
        if net.is_some() {
            stats.solved += 1;
        }
        let outcome = Outcome::of(net, base);
        match outcome {
            Outcome::Better => stats.better += 1,
            Outcome::Worse => stats.worse += 1,
            Outcome::Tie => stats.tie += 1,
            Outcome::SolvedOnly => stats.solved_only += 1,
            Outcome::Missed => stats.missed += 1,
            Outcome::NeitherSolved => {}
        }
        if !outcome.is_comparable() {
            continue;
        }
        stats.compared += 1;
        // `Tie` means the two outputs are equal, so the bps is exactly zero
        if let (Some(net), Some(base)) = (net, base) {
            if let Some(bps) = diff_bps(net, base) {
                all_bps.push(bps);
            }
        }
    }

    let mut times = config_run.times_us.clone();
    let timings = timings_of(&mut times);
    stats.p50_us = timings.p50_us;
    stats.p95_us = timings.p95_us;
    stats.slowest_us = timings.slowest_us;
    (stats.mean_bps, stats.median_bps) = mean_and_median(&mut all_bps);

    stats
}

/// One token pair, with the orders that traded it.
struct TokenPair {
    token_in: Address,
    token_out: Address,
    /// Indices into the order list, so a pair's measurements can be picked out of any config.
    order_indices: Vec<usize>,
}

/// Groups the orders by `(token_in, token_out)`, keeping first-seen order.
fn pairs_of(orders: &[TradeOrder]) -> Vec<TokenPair> {
    let mut index_of: HashMap<(Address, Address), usize> = HashMap::new();
    let mut pairs: Vec<TokenPair> = Vec::new();

    for (index, order) in orders.iter().enumerate() {
        let key = (order.token_in.clone(), order.token_out.clone());
        let position = *index_of.entry(key).or_insert_with(|| {
            pairs.push(TokenPair {
                token_in: order.token_in.clone(),
                token_out: order.token_out.clone(),
                order_indices: Vec::new(),
            });
            pairs.len() - 1
        });
        pairs[position]
            .order_indices
            .push(index);
    }

    pairs
}

/// One config's record on one token pair.
struct PairStats {
    solved: usize,
    compared: usize,
    mean_bps: f64,
    median_bps: f64,
    better: usize,
    worse: usize,
}

fn pair_stats(
    pair: &TokenPair,
    measurements: &[Measurement],
    baseline: &[Measurement],
) -> PairStats {
    let mut stats =
        PairStats { solved: 0, compared: 0, mean_bps: 0.0, median_bps: 0.0, better: 0, worse: 0 };
    let mut all_bps: Vec<f64> = Vec::with_capacity(pair.order_indices.len());

    for &index in &pair.order_indices {
        let net = measurements[index].net_out.as_ref();
        let base = baseline[index].net_out.as_ref();
        if net.is_some() {
            stats.solved += 1;
        }
        let outcome = Outcome::of(net, base);
        match outcome {
            Outcome::Better => stats.better += 1,
            Outcome::Worse => stats.worse += 1,
            Outcome::Tie | Outcome::SolvedOnly | Outcome::Missed | Outcome::NeitherSolved => {}
        }
        if !outcome.is_comparable() {
            continue;
        }
        stats.compared += 1;
        if let (Some(net), Some(base)) = (net, base) {
            if let Some(bps) = diff_bps(net, base) {
                all_bps.push(bps);
            }
        }
    }

    (stats.mean_bps, stats.median_bps) = mean_and_median(&mut all_bps);
    stats
}

fn pair_name(pair: &TokenPair, symbols: &HashMap<Address, String>) -> String {
    format!("{}->{}", token_label(&pair.token_in, symbols), token_label(&pair.token_out, symbols))
}

/// Everything a run produced, so an output writer takes one argument rather than seven.
///
/// The per-config and per-pair statistics are computed once, in `main`, and shared: every table
/// reads the same numbers instead of re-deriving them, and adding an output file does not mean
/// threading the same bundle through another signature.
struct BenchOutcome<'a> {
    run: &'a Run,
    /// Which market the run solved against, and what it was.
    source: &'a MarketSource,
    /// Pools per protocol in the graph every config solved, so usage can be read against what
    /// was there to use.
    market_protocols: &'a [ProtocolCount],
    /// Every config that produced data, in run order; the baseline is first.
    results: &'a [(BenchConfig, ConfigRun)],
    /// Configs asked for but not run, each with why.
    skipped: &'a [(String, String)],
    orders: &'a [TradeOrder],
    pairs: &'a [TokenPair],
    /// One entry per config, by label.
    stats: &'a HashMap<String, ConfigStats>,
    /// Indexed by position in `pairs`, then by config label.
    pair_stats: &'a [HashMap<String, PairStats>],
    symbols: &'a HashMap<Address, String>,
    wei: &'a HashMap<Address, f64>,
    blocked: &'a BlockedTokens,
    summary: &'a TradeLoadSummary,
}

impl BenchOutcome<'_> {
    fn baseline(&self) -> &[Measurement] {
        &self.results[0].1.measurements
    }

    /// Every config except the baseline — the ones the bps columns are written for.
    fn rivals(&self) -> impl Iterator<Item = &(BenchConfig, ConfigRun)> {
        self.results
            .iter()
            .filter(|(config, _)| config.label != BASELINE)
    }
}

/// One JSON object per line: an order, and what every config did with it.
///
/// Newline-delimited rather than one array so it can be streamed, appended to and `grep`ed, and
/// because a 50k-order run would otherwise be a single unwieldy document. Each line stands alone —
/// it carries the symbols for the tokens it mentions — so a viewer can render one order without
/// loading anything else.
///
/// The shape is a flow graph: `edges` are legs, each with its own amounts, so a Sankey or
/// node-link diagram can be drawn straight from it. Nodes are the distinct tokens across `edges`;
/// they are not listed separately because they are derivable and would only drift.
fn routes_jsonl(outcome: &BenchOutcome<'_>) -> String {
    let mut out = String::new();
    for (index, order) in outcome.orders.iter().enumerate() {
        let mut tokens = serde_json::Map::new();
        let insert_token = |tokens: &mut serde_json::Map<String, serde_json::Value>,
                            address: &Address| {
            tokens.insert(
                address.to_string(),
                serde_json::Value::String(token_label(address, outcome.symbols)),
            );
        };
        insert_token(&mut tokens, &order.token_in);
        insert_token(&mut tokens, &order.token_out);

        let mut routes = serde_json::Map::new();
        for (config, config_run) in outcome.results {
            let measurement = &config_run.measurements[index];
            let edges: Vec<serde_json::Value> = measurement
                .edges
                .iter()
                .map(|edge| {
                    insert_token(&mut tokens, &edge.token_in);
                    insert_token(&mut tokens, &edge.token_out);
                    serde_json::json!({
                        "token_in": edge.token_in.to_string(),
                        "token_out": edge.token_out.to_string(),
                        "component_id": edge.component_id,
                        "protocol": edge.protocol,
                        "amount_in": edge.amount_in.to_string(),
                        "amount_out": edge.amount_out.to_string(),
                        "gas": edge.gas.to_string(),
                        "split": edge.split,
                    })
                })
                .collect();

            routes.insert(
                config.label.clone(),
                serde_json::json!({
                    "solved": measurement.net_out.is_some(),
                    "amount_out": measurement.amount_out.as_ref().map(ToString::to_string),
                    "amount_out_net_gas": measurement.net_out.as_ref().map(ToString::to_string),
                    "usd_out": measurement
                        .net_out
                        .as_ref()
                        .and_then(|net| usd_out(order, net, outcome.wei)),
                    "gas": measurement.gas.as_ref().map(ToString::to_string),
                    "elapsed_us": measurement.elapsed_us,
                    "edges": edges,
                }),
            );
        }

        let line = serde_json::json!({
            "index": index,
            "order": {
                "id": order.id,
                "token_in": order.token_in.to_string(),
                "token_out": order.token_out.to_string(),
                "amount_in": order.amount_in.to_string(),
                "amount_usd": order.amount_usd,
            },
            "tokens": tokens,
            "baseline": BASELINE,
            "routes": routes,
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// What the run was, for the viewer's run picker and the report header.
fn run_json(outcome: &BenchOutcome<'_>) -> String {
    let run = outcome.run;
    serde_json::json!({
        "name": run.name,
        "finished_at": Utc::now().to_rfc3339(),
        // Tagged by `MarketSource`'s own `source` field, which is what the viewer filters on.
        "market": outcome.source,
        "orders": outcome.orders.len(),
        "pairs": outcome.pairs.len(),
        "baseline": BASELINE,
        "configs": outcome
            .results
            .iter()
            .map(|(config, _)| config.label.clone())
            .collect::<Vec<_>>(),
        "skipped": outcome.skipped,
        "gas_price_gwei": run.gas_price_gwei,
        "timeout_ms": run.timeout_ms,
        "jobs": run.jobs,
        "workers": run.workers,
        "repeats": run.repeats,
        "dataset": run.trades.display().to_string(),
        "dataset_orders": {
            "seen": outcome.summary.seen,
            "eligible": outcome.summary.eligible,
            "kept": outcome.summary.kept,
        },
        "blocked_tokens": outcome.blocked.symbols,
        "blocked_components": outcome.blocked.dropped_component_count,
    })
    .to_string()
}

/// Rebuilds `<root>/index.json` by scanning for runs.
///
/// Scanned rather than appended to, so a run directory that is deleted stops being offered and a
/// half-written one never lingers. Ordering is by name; the viewer sorts however it likes.
fn write_index(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };

    let mut runs: Vec<serde_json::Value> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let manifest = entry.path().join("run.json");
            let contents = std::fs::read_to_string(manifest).ok()?;
            serde_json::from_str::<serde_json::Value>(&contents).ok()
        })
        .collect();
    runs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .cmp(&b["name"].as_str())
    });

    let index = serde_json::json!({ "runs": runs }).to_string();
    let path = root.join("index.json");
    match std::fs::write(&path, index) {
        Ok(()) => println!("  wrote {}", path.display()),
        Err(error) => println!("  could not write {}: {error}", path.display()),
    }
}

fn write_file(dir: &Path, file_name: &str, contents: &str) {
    let path = dir.join(file_name);
    match std::fs::write(&path, contents) {
        Ok(()) => println!("  wrote {}", path.display()),
        Err(error) => println!("  could not write {}: {error}", path.display()),
    }
}

/// One row per order and config, for anything the report does not answer.
fn orders_csv(outcome: &BenchOutcome<'_>) -> String {
    let mut csv = String::from(
        "order,token_in,token_out,amount_in,amount_usd,config,algorithm,max_hops,\
         solved,net_out,usd_out,swaps,elapsed_us,bps_vs_baseline,outcome\n",
    );

    for (index, order) in outcome.orders.iter().enumerate() {
        let base = outcome.baseline()[index]
            .net_out
            .clone();
        for (config, config_run) in outcome.results {
            let measurement = &config_run.measurements[index];
            let (solved, net_out, swaps) = match &measurement.net_out {
                Some(net) => ("true", net.to_string(), measurement.edges.len().to_string()),
                None => ("false", String::new(), String::new()),
            };
            let value_out = measurement
                .net_out
                .as_ref()
                .and_then(|net| usd_out(order, net, outcome.wei))
                .map(|usd| format!("{usd:.2}"))
                .unwrap_or_default();
            let bps = measurement
                .net_out
                .as_ref()
                .zip(base.as_ref())
                .and_then(|(net, base)| diff_bps(net, base))
                .map(|bps| format!("{bps:.1}"))
                .unwrap_or_default();
            let outcome_label = Outcome::of(measurement.net_out.as_ref(), base.as_ref()).label();
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                order.id,
                order.token_in,
                order.token_out,
                order.amount_in,
                order
                    .amount_usd
                    .map(|usd| format!("{usd:.2}"))
                    .unwrap_or_default(),
                config.label,
                config.algorithm,
                config.max_hops,
                solved,
                net_out,
                value_out,
                swaps,
                measurement.elapsed_us,
                bps,
                outcome_label,
            ));
        }
    }
    csv
}

/// One row per config and protocol: what each algorithm actually routed through.
///
/// Tidy rather than wide -- one observation per row -- so it plots without reshaping, and every
/// protocol in the market gets a row per config even when unused, because a zero against 400
/// available pools is the finding.
///
/// `orders` and `usd` count a route once per protocol it crosses, so they overlap across
/// protocols; `legs` and `pools_used` do not. The columns are described in `benches/README.md`.
fn protocols_csv(outcome: &BenchOutcome<'_>) -> String {
    let mut csv = String::from(
        "config,algorithm,protocol,pools_in_market,pools_simulatable,pools_used,legs,legs_pct,\
         orders,orders_pct,usd\n",
    );

    for (config, config_run) in outcome.results {
        // Every protocol in the market gets a row, used or not: a zero is the interesting answer
        // when an algorithm never touches liquidity that was available to it.
        let mut pools_used: HashMap<&str, HashSet<&str>> = HashMap::new();
        let mut legs: HashMap<&str, usize> = HashMap::new();
        let mut orders: HashMap<&str, usize> = HashMap::new();
        let mut usd: HashMap<&str, f64> = HashMap::new();
        let (mut total_legs, mut solved_orders) = (0usize, 0usize);

        for (index, measurement) in config_run
            .measurements
            .iter()
            .enumerate()
        {
            if measurement.net_out.is_none() {
                continue;
            }
            solved_orders += 1;
            let mut seen: HashSet<&str> = HashSet::new();
            for edge in &measurement.edges {
                total_legs += 1;
                *legs.entry(&edge.protocol).or_default() += 1;
                pools_used
                    .entry(&edge.protocol)
                    .or_default()
                    .insert(&edge.component_id);
                seen.insert(&edge.protocol);
            }
            for protocol in seen {
                *orders.entry(protocol).or_default() += 1;
                if let Some(amount) = outcome.orders[index].amount_usd {
                    *usd.entry(protocol).or_default() += amount;
                }
            }
        }

        // Market order, so every config's block reads the same way down the file.
        for row in outcome.market_protocols {
            let protocol = row.protocol.as_str();
            let legs_count = legs.get(protocol).copied().unwrap_or(0);
            let orders_count = orders
                .get(protocol)
                .copied()
                .unwrap_or(0);
            let pct = |part: usize, whole: usize| {
                if whole == 0 {
                    0.0
                } else {
                    part as f64 / whole as f64 * 100.0
                }
            };
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{:.1},{},{:.1},{:.0}\n",
                config.label,
                config.algorithm,
                protocol,
                row.components,
                row.with_state,
                pools_used
                    .get(protocol)
                    .map_or(0, HashSet::len),
                legs_count,
                pct(legs_count, total_legs),
                orders_count,
                pct(orders_count, solved_orders),
                usd.get(protocol)
                    .copied()
                    .unwrap_or(0.0),
            ));
        }
    }
    csv
}

/// One row per token pair and config.
fn pairs_csv(outcome: &BenchOutcome<'_>) -> String {
    let mut csv = String::from(
        "pair,token_in,token_out,orders,config,algorithm,max_hops,\
         solved,compared,mean_bps,median_bps,better,worse\n",
    );

    for (position, pair) in outcome.pairs.iter().enumerate() {
        let name = pair_name(pair, outcome.symbols);
        for (config, _) in outcome.results {
            let stats = &outcome.pair_stats[position][&config.label];
            let is_baseline = config.label == BASELINE;
            let cell = |value: String| if is_baseline { String::new() } else { value };
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                name,
                pair.token_in,
                pair.token_out,
                pair.order_indices.len(),
                config.label,
                config.algorithm,
                config.max_hops,
                stats.solved,
                stats.compared,
                cell(format!("{:.1}", stats.mean_bps)),
                cell(format!("{:.1}", stats.median_bps)),
                cell(stats.better.to_string()),
                cell(stats.worse.to_string()),
            ));
        }
    }
    csv
}

/// The written report: run configuration, the per-config table, and the busiest pairs.
fn report_markdown(outcome: &BenchOutcome<'_>) -> String {
    let run = outcome.run;
    let orders = outcome.orders;
    let mut out = String::new();
    out.push_str(&format!("# algorithm_bench: {}\n\n", run.name));

    out.push_str("## Run\n\n");
    out.push_str("| setting | value |\n|---|---|\n");
    match outcome.source {
        MarketSource::Offline { chain_name, .. } => {
            out.push_str(&format!("| market | offline fixture, {chain_name} |\n"));
        }
        MarketSource::Live { chain_name, block, components, states, min_tvl, protocols } => {
            // One row per line: a continuation indent would put the rest inside a code block and
            // break the table.
            out.push_str(&format!("| market | **live**, {chain_name} block {block} |\n"));
            out.push_str(&format!(
                "| captured | {components} components, {states} states, min TVL {min_tvl} ETH |\n"
            ));
            out.push_str(&format!("| protocols | {} |\n", protocols.join(", ")));
        }
    }
    out.push_str(&format!("| orders run | {} |\n", orders.len()));
    out.push_str(&format!(
        "| configs | {} |\n",
        outcome
            .results
            .iter()
            .map(|(config, _)| format!("`{}`", config.label))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for (name, reason) in outcome.skipped {
        out.push_str(&format!("| **skipped** | `{name}` — {reason} |\n"));
    }
    out.push_str(&format!("| token pairs | {} |\n", outcome.pairs.len()));
    out.push_str(&format!("| baseline | `{BASELINE}` |\n"));
    out.push_str(&format!("| per-solve timeout | {}ms |\n", run.timeout_ms));
    out.push_str(&format!("| gas price | {} gwei |\n", run.gas_price_gwei));
    out.push_str(&format!("| orders in flight | {} |\n", run.jobs));
    out.push_str(&format!("| workers per pool | {} |\n", run.workers));
    out.push_str(&format!("| dataset | `{}` |\n", run.trades.display()));
    out.push_str(&format!(
        "| dataset orders | {} total, {} eligible, {} solved here |\n",
        outcome.summary.seen, outcome.summary.eligible, outcome.summary.kept
    ));
    out.push_str(&format!(
        "| dropped | {} not a sell, {} unknown token, {} malformed |\n",
        outcome.summary.dropped_not_sell,
        outcome.summary.dropped_unknown_token,
        outcome.summary.dropped_malformed
    ));
    if !outcome.blocked.symbols.is_empty() {
        out.push_str(&format!(
            "| blocked tokens | {} — {} pools dropped from the market, see \
             `benches/blocked_tokens.toml` |\n",
            outcome.blocked.symbols.join(", "),
            outcome.blocked.dropped_component_count
        ));
    }
    out.push('\n');

    out.push_str("## By algorithm\n\n");
    out.push_str(
        "Bps are against the baseline over the orders both solved. Better, worse and tie are \
         decided on the exact integer outputs, not on the bps figure: a tie means the two returned \
         the same output to the wei, which usually means the same route. `solved only` counts \
         orders this config routed and the baseline did not; `missed` the reverse. Those two are \
         coverage, not quality, so they are excluded from the bps figures.\n\n",
    );
    out.push_str(
        "| config | solved | compared | mean bps | median bps | better | worse | tie | \
         solved only | missed |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (config, _) in outcome.results {
        let stats = &outcome.stats[&config.label];
        let is_baseline = config.label == BASELINE;
        let cell = |value: String| if is_baseline { "—".to_string() } else { value };
        out.push_str(&format!(
            "| `{}` | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            config.label,
            stats.solved,
            orders.len(),
            cell(stats.compared.to_string()),
            cell(format!("{:+.1}", stats.mean_bps)),
            cell(format!("{:+.1}", stats.median_bps)),
            cell(stats.better.to_string()),
            cell(stats.worse.to_string()),
            cell(stats.tie.to_string()),
            cell(stats.solved_only.to_string()),
            cell(stats.missed.to_string()),
        ));
    }

    out.push_str("\n## Timing\n\n");
    if run.jobs > 1 {
        out.push_str(&format!(
            "Solves ran {} at a time, so these are wall clock under load rather than isolated \
             latency. Comparable across configs in this run, meaningless as absolutes. Re-run \
             with `--jobs 1` for clean per-solve numbers.\n\n",
            run.jobs
        ));
    } else {
        out.push_str("One solve at a time, so these are isolated per-solve latencies.\n\n");
    }
    out.push_str(&format!(
        "Over {} pass(es) per config; configs never overlap, each solver is torn down before \
         the next is built.\n\n",
        run.repeats
    ));
    out.push_str("| config | solves | total | p50 | p95 | slowest |\n|---|---|---|---|---|---|\n");
    for (config, _) in outcome.results {
        let stats = &outcome.stats[&config.label];
        out.push_str(&format!(
            "| `{}` | {} | {}ms | {}us | {}us | {}us |\n",
            config.label,
            orders.len() * run.repeats,
            stats.solving_ms,
            stats.p50_us,
            stats.p95_us,
            stats.slowest_us,
        ));
    }

    out.push_str(&format!(
        "\n## By token pair ({PAIR_ROWS} busiest of {})\n\n",
        outcome.pairs.len()
    ));
    out.push_str(
        "Mean bps against the baseline. A bracketed count appears only where the mean covers \
         fewer orders than the pair has, meaning one of the two failed to route the rest. Every \
         pair is in `pairs.csv`, with medians and per-config solve counts alongside.\n\n",
    );
    out.push_str("| pair | orders |");
    for (config, _) in outcome.rivals() {
        out.push_str(&format!(" {} |", config.label));
    }
    out.push_str("\n|---|---|");
    for _ in outcome.rivals() {
        out.push_str("---|");
    }
    out.push('\n');

    let mut ranked: Vec<usize> = (0..outcome.pairs.len()).collect();
    ranked.sort_by_key(|&position| {
        std::cmp::Reverse(
            outcome.pairs[position]
                .order_indices
                .len(),
        )
    });
    for &position in ranked.iter().take(PAIR_ROWS) {
        let pair = &outcome.pairs[position];
        out.push_str(&format!(
            "| {} | {} |",
            pair_name(pair, outcome.symbols),
            pair.order_indices.len()
        ));
        for (config, _) in outcome.rivals() {
            let stats = &outcome.pair_stats[position][&config.label];
            if stats.compared == 0 {
                out.push_str(" — |");
            } else if stats.compared == pair.order_indices.len() {
                out.push_str(&format!(" {:+.1} |", stats.mean_bps));
            } else {
                out.push_str(&format!(" {:+.1} ({}) |", stats.mean_bps, stats.compared));
            }
        }
        out.push('\n');
    }

    out.push_str(
        "\n## Reading this\n\n\
         - Solve times are wall clock under concurrency, not isolated latency.\n\
         - `bellman_ford`'s `max_hops` bounds the subgraph it builds, not the path length it \
           returns, so the baseline answers with longer routes than its limit suggests. For a \
           like-for-like depth comparison read `water_fill`.\n\
         - A single mispriced pool in the recording can dominate a mean. Where mean and median \
           disagree sharply, check `orders.csv` for the outlying orders before drawing a \
           conclusion, then `routes.jsonl` for the pools they went through.\n",
    );

    out
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let mut market = match build_market(args.live.clone()).await {
        Ok(market) => market,
        Err(reason) => {
            eprintln!("error: {reason}");
            std::process::exit(1);
        }
    };
    let source = market.source.clone();
    let (run, mut configs) = Run::resolve(args, &market);

    let mut blocked = load_blocked_tokens();
    blocked.dropped_component_count = block_components(&mut market.updates, &blocked.addresses);

    // After the pools are gone, so an order whose endpoint is blocked cannot be routed anyway
    let known_tokens = recorded_tokens(&market.updates);
    let (orders, summary) = load_trade_orders(&run.trades, &known_tokens, run.orders)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(!orders.is_empty(), "no dataset order survived filtering");

    println!("\n=== algorithm_bench: {} ===", run.name);
    match &source {
        MarketSource::Offline { chain_name, .. } => {
            println!("  market             offline fixture ({chain_name})");
        }
        MarketSource::Live { chain_name, block, components, states, .. } => {
            println!(
                "  market             live {chain_name} block {block} \
                 ({components} components, {states} states)"
            );
        }
    }
    println!("  dataset            {}", run.trades.display());
    println!("  orders in file     {}", summary.seen);
    println!("  eligible           {}", summary.eligible);
    println!("  solving            {}", summary.kept);
    println!("  orders in flight   {}", run.jobs);
    println!("  workers per pool   {}", run.workers);
    println!("  timeout            {}ms", run.timeout_ms);
    println!("  gas price          {} gwei", run.gas_price_gwei);
    let market_protocols = protocol_breakdown(&market.updates);
    print_protocol_breakdown(&market_protocols);
    if !blocked.symbols.is_empty() {
        println!(
            "\n  blocked            {} ({} pools dropped)",
            blocked.symbols.join(", "),
            blocked.dropped_component_count
        );
    }
    println!(
        "  configs            {}",
        configs
            .ready
            .iter()
            .map(|config| config.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (name, reason) in &configs.skipped {
        println!("  skipped '{name}': {reason}");
    }
    if let Some(recorded) = market.market_gas_price.as_ref() {
        println!("  (the recording captured {recorded} wei; --gas-price-gwei overrides it)");
    }

    // Pairing each config with its run makes the two structurally impossible to fall out of sync.
    let mut results: Vec<(BenchConfig, ConfigRun)> = Vec::new();
    // Every config replays the same market, so its prices are the same; read them once.
    let mut wei: HashMap<Address, f64> = HashMap::new();
    for config in std::mem::take(&mut configs.ready) {
        println!("\n  {} ...", config.label);
        let solver =
            match build_solver(&config, &market, run.workers, run.timeout_ms, run.gas_price_gwei)
                .await
            {
                Ok(solver) => solver,
                Err(reason) => {
                    println!("    skipped: {reason}");
                    configs
                        .skipped
                        .push((config.label.clone(), reason));
                    continue;
                }
            };
        if wei.is_empty() {
            wei = wei_per_token(&*solver.derived_data().read().await);
        }
        let config_run = run_config(&solver, &orders, run.jobs, run.repeats).await;
        results.push((config, config_run));
        // Dropped before the next is built, so no two configs ever solve at the same time.
        drop(solver);
    }

    assert!(
        results
            .first()
            .is_some_and(|(config, _)| config.label == BASELINE),
        "the baseline {BASELINE} could not be built, so nothing can be compared"
    );

    let pairs = pairs_of(&orders);
    let symbols = symbol_table();
    let baseline = results[0].1.measurements.as_slice();

    // Computed once here and shared by every writer, so no table re-derives what another already
    // has.
    let stats: HashMap<String, ConfigStats> = results
        .iter()
        .map(|(config, config_run)| (config.label.clone(), config_stats(config_run, baseline)))
        .collect();
    let pair_stats_by_pair: Vec<HashMap<String, PairStats>> = pairs
        .iter()
        .map(|pair| {
            results
                .iter()
                .map(|(config, config_run)| {
                    (config.label.clone(), pair_stats(pair, &config_run.measurements, baseline))
                })
                .collect()
        })
        .collect();

    let outcome = BenchOutcome {
        run: &run,
        source: &source,
        market_protocols: &market_protocols,
        results: &results,
        skipped: &configs.skipped,
        orders: &orders,
        pairs: &pairs,
        stats: &stats,
        pair_stats: &pair_stats_by_pair,
        symbols: &symbols,
        wei: &wei,
        blocked: &blocked,
        summary: &summary,
    };

    if let Err(error) = std::fs::create_dir_all(&run.out_dir) {
        println!("could not create {}: {error}", run.out_dir.display());
        return;
    }
    write_file(&run.out_dir, "report.md", &report_markdown(&outcome));
    write_file(&run.out_dir, "orders.csv", &orders_csv(&outcome));
    write_file(&run.out_dir, "pairs.csv", &pairs_csv(&outcome));
    write_file(&run.out_dir, "protocols.csv", &protocols_csv(&outcome));
    write_file(&run.out_dir, "routes.jsonl", &routes_jsonl(&outcome));
    write_file(&run.out_dir, "run.json", &run_json(&outcome));
    if let Some(root) = run.out_dir.parent() {
        write_index(root);
    }
}
