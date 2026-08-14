//! One algorithm, the orders you name — shaped for a profiler.
//!
//! Deliberately not the benchmark. `algorithm_bench` compares configurations and writes a report;
//! this runs a single configuration and writes nothing, so a flamegraph contains the solve and
//! almost nothing else. Both take their market the same way -- the recorded fixture, or `--market
//! live` -- and read the same dataset, so a route seen in the viewer can be profiled here by its
//! order id. Offline, on the same fixture and gas price, it is the same solve; live it is a
//! different block, and so a different market.
//!
//! ```text
//! ./scripts/profile.sh --config water_fill_d3 --order 2073
//! ./scripts/profile.sh --config water_fill_d3 --orders 200 --repeats 3
//! ./scripts/profile.sh --config water_fill_d3 --orders 200 --no-record   # timings only
//! ```
//!
//! # Reading the flamegraph
//!
//! Three threads by default. The main thread mostly waits on a channel; a tokio thread carries the
//! feed and the derived-data computations; the one that matters is the single solver worker, where
//! everything under `find_best_route` is the algorithm.
//!
//! `--jobs N` runs N orders at a time on N workers, which finishes a run faster at the cost of a
//! flamegraph spread over N solving threads and timings measured under load.
//!
//! Setup is not free — building the solver replays the recording, builds the graph and runs the
//! derived computations before the first order. Those frames sit under `Solver::from_recording`.
//! `--repeats` pushes them down to a rounding error without changing what is measured.
//!
//! # Caching that shapes what you see
//!
//! Derived data — spot prices, pool depths, token gas prices — is computed when the market
//! changes, not per solve. The recording stops feeding after the replay, so it is computed once
//! and every order reads the same values. In production that work recurs every block, so its
//! absence here flatters the profile.

mod common;

use std::{path::PathBuf, time::Instant};

use clap::Parser;
use futures::stream::StreamExt;

use common::{
    available_configs, block_components, build_market, build_solver, exclude_requested_protocols,
    format_micros, load_bench_config, load_blocked_tokens, print_protocol_breakdown,
    protocol_breakdown, resolved_gas_price_gwei, symbol_table, timings_of, token_label,
    trades::{load_trade_orders, recorded_tokens, TradeOrder},
    LiveFlags, MarketSource,
};
use fynd_core::{types::QuoteStatus, QuoteOptions, QuoteRequest, Solver};

#[derive(Parser, Debug)]
#[command(
    about = "Run one algorithm over chosen orders, single-threaded, for profiling",
    long_about = "Runs a single configuration on one thread and prints timings. Writes nothing --  \
                  use algorithm_bench for reports. Wrap it in a profiler with ./scripts/profile.sh."
)]
struct Args {
    /// Config to run, named after a file in `benches/configs/`.
    #[arg(long)]
    config: String,

    /// One order, by id or by its leading index — `2073` and `2073_00000000_ae7ab965` both work.
    /// Takes precedence over `--orders`.
    #[arg(long)]
    order: Option<String>,

    /// Orders to run, from the top of the dataset. Ignored when `--order` is given.
    #[arg(long, default_value_t = 100)]
    orders: usize,

    /// Passes over the chosen orders. More passes, same work — the way to make a short run
    /// produce enough samples to read.
    #[arg(long, default_value_t = 1)]
    repeats: usize,

    /// Orders in flight at once, on that many solver workers. One by default, so the flamegraph
    /// has a single solving thread to read. Raise it to get through a run faster, and read the
    /// timings as wall clock under load rather than as per-solve cost.
    #[arg(long, default_value_t = 1)]
    jobs: usize,

    /// Per-solve budget in milliseconds.
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,

    /// Gas price in gwei, fractions allowed. Without it a live run prices at whatever the chain
    /// is charging, and an offline run at the default.
    #[arg(long, value_parser = common::parse_gas_price_gwei)]
    gas_price_gwei: Option<f64>,

    /// Market flags: `--market`, and the Tycho settings a live capture needs.
    #[command(flatten)]
    live: LiveFlags,

    /// Protocol systems to drop from the market before solving, comma separated, e.g.
    /// `vm:balancer_v2,uniswap_v4_hooks`. Names must match the market's own exactly; a name that
    /// matches nothing stops the run. Applies to a fixture and a live capture alike.
    #[arg(long, value_delimiter = ',')]
    exclude_protocols: Option<Vec<String>>,

    /// Trade dataset path.
    #[arg(long)]
    trades: Option<PathBuf>,

    /// Print each solve as it finishes rather than only the summary.
    #[arg(long)]
    verbose: bool,

    /// Emit the solver's own logs. `RUST_LOG` picks the filter, e.g.
    /// `RUST_LOG=fynd_core::algorithm=trace`; without it `fynd_core=debug`. Pair with
    /// `--no-record`: logging shows up in the flamegraph as its own cost.
    #[arg(long)]
    logs: bool,
}

/// Slowest solves listed at the end, so the next run can aim at one with `--order`.
const SLOWEST_SHOWN: usize = 10;

/// One solve, timed by the caller so queueing and dispatch are counted.
struct Solve {
    order_id: String,
    elapsed_us: u128,
    solved: bool,
    swaps: usize,
}

async fn solve(solver: &Solver, order: &TradeOrder) -> Solve {
    let request = QuoteRequest::new(vec![order.to_order()], QuoteOptions::default());
    let start = Instant::now();
    let quote = solver.quote(request).await;
    let elapsed_us = start.elapsed().as_micros();

    let (solved, swaps) = match &quote {
        Ok(quote) => {
            let order_quote = &quote.orders()[0];
            if order_quote.status() == QuoteStatus::Success {
                (
                    true,
                    order_quote
                        .route()
                        .map(|route| route.swaps().len())
                        .unwrap_or(0),
                )
            } else {
                (false, 0)
            }
        }
        Err(_) => (false, 0),
    };

    Solve { order_id: order.id.clone(), elapsed_us, solved, swaps }
}

/// Orders to run: one named order, or the first `order_count` of the dataset.
fn select_orders(
    orders: Vec<TradeOrder>,
    wanted: Option<&str>,
    order_count: usize,
) -> Vec<TradeOrder> {
    let Some(wanted) = wanted else {
        return orders
            .into_iter()
            .take(order_count)
            .collect();
    };
    // `2073` matches `2073_00000000_ae7ab965`; the full id matches itself
    let picked: Vec<TradeOrder> = orders
        .into_iter()
        .filter(|order| {
            order.id == wanted ||
                order
                    .id
                    .starts_with(&format!("{wanted}_"))
        })
        .collect();
    assert!(!picked.is_empty(), "no order matching '{wanted}' in the dataset");
    picked
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    common::init_logging(args.logs);

    let config = load_bench_config(&args.config)
        .unwrap_or_else(|reason| panic!("{reason}. Available: {}", available_configs().join(", ")));

    let mut market = match build_market(args.live.clone()).await {
        Ok(market) => market,
        Err(reason) => {
            eprintln!("error: {reason}");
            std::process::exit(1);
        }
    };
    let gas_price_gwei = resolved_gas_price_gwei(args.gas_price_gwei, &market);
    let excluded_protocols = args
        .exclude_protocols
        .clone()
        .unwrap_or_default();
    let excluded_components = exclude_requested_protocols(&mut market, &excluded_protocols);
    let market_protocols = protocol_breakdown(&market.updates);
    let blocked = load_blocked_tokens();
    let blocked_components = block_components(&mut market.updates, &blocked.addresses);
    let known_tokens = recorded_tokens(&market.updates);

    let trades = args
        .trades
        .unwrap_or_else(common::default_trades_path);
    // The whole dataset is loaded so `--order` can find any id, then narrowed.
    let (all, summary) =
        load_trade_orders(&trades, &known_tokens, 0).unwrap_or_else(|error| panic!("{error}"));
    let orders = select_orders(all, args.order.as_deref(), args.orders);
    let repeats = args.repeats.max(1);
    let jobs = args.jobs.max(1);
    // More workers than cores costs setup time and memory without adding throughput.
    let workers = jobs.min(num_cpus::get());

    let symbols = symbol_table();
    let pair_of = |order: &TradeOrder| {
        format!(
            "{}->{}",
            token_label(&order.token_in, &symbols),
            token_label(&order.token_out, &symbols)
        )
    };

    println!("\n=== profile: {} ===", config.label);
    println!("  algorithm          {} @ {} hops", config.algorithm, config.max_hops);
    println!("  orders             {} x {repeats} pass(es)", orders.len());
    if orders.len() <= 5 {
        for order in &orders {
            println!("                     {} ({})", order.id, pair_of(order));
        }
    }
    println!("  dataset            {} of {} eligible", summary.kept, summary.eligible);
    println!("  jobs / workers     {jobs} / {workers}");
    println!("  timeout            {}ms", args.timeout_ms);
    match &market.source {
        MarketSource::Offline { chain_name, .. } => {
            println!("  market             offline fixture ({chain_name})")
        }
        MarketSource::Live { chain_name, block, components, states, .. } => println!(
            "  market             live {chain_name} block {block} \
             ({components} components, {states} states)"
        ),
    }
    println!("  gas price          {gas_price_gwei} gwei");
    print_protocol_breakdown(&market_protocols);
    if !excluded_protocols.is_empty() {
        println!(
            "\n  excluded           {} ({} pools dropped)",
            excluded_protocols.join(", "),
            excluded_components
        );
    }
    if !blocked.symbols.is_empty() {
        println!(
            "  blocked            {} ({blocked_components} pools)",
            blocked.symbols.join(", ")
        );
    }
    println!("\nbuilding solver ...");

    let setup = Instant::now();
    let solver = build_solver(&config, &market, workers, args.timeout_ms, gas_price_gwei)
        .await
        .unwrap_or_else(|reason| panic!("could not build {}: {reason}", config.label));
    let setup_ms = setup.elapsed().as_millis();
    println!("ready in {setup_ms}ms, solving\n");

    let mut solves: Vec<Solve> = Vec::with_capacity(orders.len() * repeats);
    let solving = Instant::now();
    for pass in 0..repeats {
        let mut results = futures::stream::iter(orders.iter())
            .map(|order| solve(&solver, order))
            .buffer_unordered(jobs);
        while let Some(result) = results.next().await {
            if args.verbose {
                println!(
                    "  {:<28} {:>10}  {}",
                    result.order_id,
                    format_micros(result.elapsed_us),
                    if result.solved {
                        format!("{} swaps", result.swaps)
                    } else {
                        "no route".into()
                    }
                );
            }
            solves.push(result);
        }
        if repeats > 1 {
            println!("  pass {}/{repeats}", pass + 1);
        }
    }
    let solving_ms = solving.elapsed().as_millis();

    let mut times: Vec<u128> = solves
        .iter()
        .map(|solve| solve.elapsed_us)
        .collect();
    let timings = timings_of(&mut times);
    let solved = solves
        .iter()
        .filter(|solve| solve.solved)
        .count();

    println!("\n=== {} solves ===", solves.len());
    if jobs > 1 {
        println!("  timings are wall clock with {jobs} orders in flight, not per-solve cost");
    }
    println!("  solved       {solved}/{}", solves.len());
    println!("  setup        {setup_ms}ms");
    println!("  solving      {solving_ms}ms");
    println!("  mean         {}", format_micros(solving_ms * 1000 / solves.len().max(1) as u128));
    println!(
        "  p50 / p95    {} / {}",
        format_micros(timings.p50_us),
        format_micros(timings.p95_us)
    );
    println!("  slowest      {}", format_micros(timings.slowest_us));

    if solves.len() > 1 {
        let mut slowest: Vec<&Solve> = solves.iter().collect();
        slowest.sort_by_key(|solve| std::cmp::Reverse(solve.elapsed_us));
        println!("\n  slowest solves — rerun one with --order <id>");
        for solve in slowest.iter().take(SLOWEST_SHOWN) {
            println!(
                "    {:>10}  {:<28} {}",
                format_micros(solve.elapsed_us),
                solve.order_id,
                if solve.solved { format!("{} swaps", solve.swaps) } else { "no route".into() }
            );
        }
    }

    if setup_ms > solving_ms {
        println!(
            "\nsetup outweighs solving — raise --repeats or --orders before reading the flamegraph"
        );
    }
    println!();
}
