//! `routes` subcommand: dump full solved routes for selected trades, per algorithm.
//!
//! Replays the frozen market snapshot (like `quality`) but writes each algorithm's route for a set
//! of trade indices in the route-visualization normalized schema, ready for the `fynd` route-viz
//! renderer. Used to show, side by side, how different algorithms route the same order.

use std::{path::PathBuf, str::FromStr};

use anyhow::{Context, Result};
use fynd_core::{offline, AlgorithmConfig, Order, OrderSide};
use num_bigint::BigUint;
use num_traits::Zero;
use serde::Deserialize;
use tycho_simulation::tycho_core::Bytes;

/// Mainnet WETH, the default gas token used to price gas during derived-data computation.
const DEFAULT_GAS_TOKEN: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

/// Dump solved routes for selected trades to the route-visualization normalized schema.
#[derive(clap::Parser, Debug)]
pub struct Args {
    /// Path to a market snapshot JSON.
    #[arg(short, long, default_value = "market_snapshot.json")]
    pub snapshot: PathBuf,

    /// Path to the trades dataset JSON.
    #[arg(short, long, default_value = "aggregator_trades_10k.json")]
    pub requests_file: PathBuf,

    /// Comma-separated zero-based trade indices (into the unshuffled all-sell-orders list).
    #[arg(short, long)]
    pub indices: String,

    /// Comma-separated algorithm names to dump.
    #[arg(short, long, default_value = "most_liquid,bellman_ford,split,split_max")]
    pub algorithms: String,

    /// Maximum hops to search.
    #[arg(long, default_value_t = 3)]
    pub max_hops: usize,

    /// Per-solve timeout in milliseconds.
    #[arg(long, default_value_t = 5000)]
    pub timeout_ms: u64,

    /// Gas token address used during derived-data computation.
    #[arg(long, default_value = DEFAULT_GAS_TOKEN)]
    pub gas_token: String,

    /// Directory to write `<algo>__idx<N>.json` route files into.
    #[arg(short, long, default_value = "routes_out")]
    pub out_dir: PathBuf,
}

#[derive(Deserialize)]
struct TradeEntry {
    orders: Vec<TradeOrder>,
}

#[derive(Deserialize)]
struct TradeOrder {
    token_in: String,
    token_out: String,
    amount: String,
    side: String,
}

pub async fn run(args: Args) -> Result<()> {
    let indices: Vec<usize> = args
        .indices
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<usize>()
                .context("parsing --indices")
        })
        .collect::<Result<_>>()?;
    anyhow::ensure!(!indices.is_empty(), "no trade indices given");

    let algorithms: Vec<String> = args
        .algorithms
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    println!("Loading snapshot: {}", args.snapshot.display());
    let snapshot = offline::load_snapshot(&args.snapshot)
        .with_context(|| format!("loading snapshot {}", args.snapshot.display()))?;

    let all_orders = load_orders(&args.requests_file)?;
    println!("Loaded {} sell orders", all_orders.len());
    let subset: Vec<Order> = indices
        .iter()
        .map(|&i| {
            all_orders
                .get(i)
                .cloned()
                .with_context(|| format!("index {i} out of range ({})", all_orders.len()))
        })
        .collect::<Result<_>>()?;

    let gas_token = Bytes::from_str(&args.gas_token).context("parsing --gas-token")?;
    println!("Computing derived data...");
    let (market, derived) = offline::prepare(snapshot, gas_token, args.max_hops, 0.01)
        .await
        .context("preparing derived data")?;

    let config = AlgorithmConfig::new(
        1,
        args.max_hops,
        std::time::Duration::from_millis(args.timeout_ms),
        None,
    )?;

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("creating {}", args.out_dir.display()))?;

    for algo in &algorithms {
        println!("Solving {algo} over {} selected trades...", subset.len());
        let routes =
            offline::run_algorithm_routes(&market, &derived, algo, config.clone(), &subset, algo)
                .await
                .with_context(|| format!("running {algo}"))?;
        for (pos, &idx) in indices.iter().enumerate() {
            match &routes[pos] {
                Some(route) => {
                    let path = args
                        .out_dir
                        .join(format!("{algo}__idx{idx}.json"));
                    let json = serde_json::to_vec_pretty(route)?;
                    std::fs::write(&path, json)?;
                    println!("  idx {idx}: {} swaps -> {}", route.swaps.len(), path.display());
                }
                None => println!("  idx {idx}: no route for {algo}"),
            }
        }
    }

    println!("\nWrote route files to {}", args.out_dir.display());
    Ok(())
}

fn load_orders(path: &PathBuf) -> Result<Vec<Order>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading trades file {}", path.display()))?;
    let entries: Vec<TradeEntry> = serde_json::from_slice(&bytes).context("parsing trades JSON")?;

    let mut orders = Vec::new();
    for entry in entries {
        let Some(trade) = entry.orders.into_iter().next() else {
            continue;
        };
        if trade.side != "sell" {
            continue;
        }
        let (Ok(token_in), Ok(token_out), Ok(amount)) = (
            Bytes::from_str(&trade.token_in),
            Bytes::from_str(&trade.token_out),
            BigUint::from_str(&trade.amount),
        ) else {
            continue;
        };
        if token_in == token_out || amount.is_zero() {
            continue;
        }
        let sender = Bytes::from([0x01u8; 20].as_slice());
        orders.push(Order::new(token_in, token_out, amount, OrderSide::Sell, sender));
    }
    Ok(orders)
}
