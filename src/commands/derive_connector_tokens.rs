use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result};
use clap::Args;
use fynd_core::{
    feed::market_data::MarketData,
    solver::{FyndBuilder, SolverBuildError},
};
use fynd_rpc::builder::parse_chain;
use tracing::info;
use tycho_simulation::tycho_common::models::Address;

/// Derives recommended connector tokens from live Tycho market data.
///
/// Connects to Tycho, waits for the initial market snapshot, then ranks every
/// token by how many pools it appears in. The most-connected tokens are the best
/// candidates for intermediate ("connector") hops in multi-hop routes.
///
/// Outputs a ranked list and a ready-to-paste TOML snippet.
#[derive(Args, PartialEq, Debug)]
pub struct DeriveConnectorTokensArgs {
    /// Target chain (e.g. Ethereum)
    #[arg(short, long, default_value = "Ethereum")]
    pub chain: String,

    /// Tycho URL. Defaults to the Fynd endpoint for the selected chain.
    #[arg(long, env)]
    pub tycho_url: Option<String>,

    /// Tycho API key.
    #[arg(long, env)]
    pub tycho_api_key: Option<String>,

    /// Disable TLS for Tycho connection.
    #[arg(long)]
    pub disable_tls: bool,

    /// Node RPC URL for the target chain. Defaults to a public endpoint if not set.
    #[arg(long, env)]
    pub rpc_url: Option<String>,

    /// Protocols to index (comma-separated). Defaults to all on-chain protocols.
    #[arg(short, long, value_delimiter = ',', value_name = "PROTO1,PROTO2")]
    pub protocols: Vec<String>,

    /// Minimum TVL threshold in native token. Pools below this threshold are excluded.
    #[arg(long, default_value_t = 10.0)]
    pub min_tvl: f64,

    /// Number of top connector tokens to suggest.
    #[arg(long, default_value_t = 10)]
    pub top_n: usize,

    /// Minimum number of pools a token must appear in to be included.
    #[arg(long, default_value_t = 2)]
    pub min_pool_count: usize,

    /// Output format: "toml", "json", or "text".
    #[arg(long, default_value = "toml")]
    pub output: String,

    /// How long to wait for the initial Tycho snapshot (seconds).
    #[arg(long, default_value_t = 120)]
    pub wait_secs: u64,
}

pub async fn run(args: DeriveConnectorTokensArgs) -> Result<()> {
    let chain = parse_chain(&args.chain).context("invalid chain")?;
    let tycho_url = crate::resolve_tycho_url(&args.chain, args.tycho_url.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rpc_url = crate::resolve_rpc_url(args.rpc_url.as_deref());

    let protocols = crate::resolve_protocols(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &args.protocols,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    info!(?protocols, "starting market data feed with {} protocol(s)", protocols.len());

    let mut builder = FyndBuilder::new(chain, tycho_url, rpc_url, protocols, args.min_tvl)
        .algorithm("most_liquid");
    if let Some(key) = &args.tycho_api_key {
        builder = builder.tycho_api_key(key.clone());
    }
    if args.disable_tls {
        builder = builder.tycho_use_tls(false);
    }

    let solver = builder
        .build()
        .map_err(|e: SolverBuildError| anyhow::anyhow!("{e}"))?;
    let market_data = solver.market_data();

    // We only need component topology and token symbols — market data is sufficient.
    // wait_until_ready also waits for derived computations (spot prices, pool depths, etc.)
    // which is expensive and unnecessary here.
    info!("Waiting for Tycho initial snapshot (up to {}s)…", args.wait_secs);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.wait_secs);
    loop {
        if market_data
            .read()
            .await
            .last_updated()
            .is_some()
        {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out after {}s waiting for initial Tycho snapshot",
            args.wait_secs
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let scores = score_tokens(&market_data).await;
    let mut ranked: Vec<(Address, TokenScore)> = scores.into_iter().collect();
    ranked.sort_by_key(|(_, s)| std::cmp::Reverse(s.pool_count));
    let total = ranked.len();

    let candidates: Vec<&(Address, TokenScore)> = ranked
        .iter()
        .filter(|(_, s)| s.pool_count >= args.min_pool_count)
        .take(args.top_n)
        .collect();

    solver.shutdown();

    match args.output.as_str() {
        "toml" => print_toml(&candidates, &args.chain, total),
        "json" => print_json(&candidates, total)?,
        _ => print_text(&candidates, total),
    }

    Ok(())
}

struct TokenScore {
    symbol: String,
    pool_count: usize,
}

async fn score_tokens(market_data: &MarketData) -> HashMap<Address, TokenScore> {
    let guard = market_data.read().await;
    let topology = guard.component_topology();

    // Count pool appearances per token.
    let mut pool_count: HashMap<Address, usize> = HashMap::new();
    for tokens in topology.values() {
        for addr in tokens {
            *pool_count
                .entry(addr.clone())
                .or_insert(0) += 1;
        }
    }

    pool_count
        .into_iter()
        .map(|(addr, count)| {
            let symbol = guard
                .get_token(&addr)
                .map(|t| t.symbol.clone())
                .unwrap_or_else(|| "?".to_string());
            (addr, TokenScore { symbol, pool_count: count })
        })
        .collect()
}

fn print_toml(candidates: &[&(Address, TokenScore)], chain: &str, total: usize) {
    use chrono::Utc;
    let date = Utc::now().format("%Y-%m-%d");
    println!("# Derived connector tokens for {chain} ({date})");
    println!("# Score = pool_count. Top {} of {} tokens.", candidates.len(), total);
    println!("connector_tokens = [");
    for (addr, score) in candidates {
        println!(
            "    \"0x{}\",  # {}  — {} pools",
            hex::encode(addr.as_ref()),
            score.symbol,
            score.pool_count,
        );
    }
    println!("]");
}

fn print_json(candidates: &[&(Address, TokenScore)], _total: usize) -> Result<()> {
    let entries: Vec<serde_json::Value> = candidates
        .iter()
        .map(|(addr, score)| {
            serde_json::json!({
                "address": format!("0x{}", hex::encode(addr.as_ref())),
                "symbol": score.symbol,
                "pool_count": score.pool_count,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}

fn print_text(candidates: &[&(Address, TokenScore)], _total: usize) {
    println!("{:<5} {:<10} {:>6}  Address", "Rank", "Symbol", "Pools");
    println!("{:-<60}", "");
    for (i, (addr, score)) in candidates.iter().enumerate() {
        println!(
            "{:<5} {:<10} {:>6}  0x{}",
            i + 1,
            score.symbol,
            score.pool_count,
            hex::encode(addr.as_ref()),
        );
    }
}
