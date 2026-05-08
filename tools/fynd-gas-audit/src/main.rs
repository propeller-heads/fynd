//! fynd-gas-audit — one-shot tool to compare Fynd's quote-time `gas_estimate`
//! against actual gas usage measured via `eth_estimateGas` on mainnet.
//!
//! See `README.md` for usage.

mod cost;
mod quoter;
mod report;
mod sampler;
mod simulator;
mod types;

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use alloy::{
    network::Ethereum,
    primitives::Address,
    providers::{ProviderBuilder, RootProvider},
};
use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    quoter::{QuoteOutcome, Quoter},
    simulator::{SimOutcome, Simulator},
    types::{Artifacts, AuditRow, RowStatus},
};

#[derive(Parser)]
#[command(name = "fynd-gas-audit")]
struct Cli {
    /// Path to the 10k aggregator-trades dataset JSON.
    /// If missing, the tool downloads it to `out/aggregator_trades_10k.json`.
    #[arg(long, default_value = "tools/fynd-gas-audit/out/aggregator_trades_10k.json")]
    dataset: PathBuf,

    #[arg(long, default_value = "http://localhost:3000")]
    fynd_url: String,

    #[arg(long, env = "RPC_URL", default_value = "https://eth.llamarpc.com")]
    rpc_url: String,

    /// How many trades to sample.
    #[arg(long, default_value_t = 100)]
    n: usize,

    /// Max trades kept per (token_in, token_out) pair.
    #[arg(long, default_value_t = 5)]
    max_per_pair: usize,

    /// Deterministic seed for the stratified shuffle.
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Output directory for artifacts.
    #[arg(long, default_value = "tools/fynd-gas-audit/out")]
    out_dir: PathBuf,

    /// Sender address used for quoting + state overrides. Deterministic across runs.
    #[arg(long, default_value = "0x000000000000000000000000000000000000BEEF")]
    sender: String,
}

const TRADES_DOWNLOAD_URL: &str =
    "https://github.com/propeller-heads/fynd/releases/download/benchmark-data-v1/aggregator_trades_10k.json";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.out_dir)
        .with_context(|| format!("creating out dir {}", cli.out_dir.display()))?;

    let sender: Address = cli
        .sender
        .parse()
        .with_context(|| format!("invalid sender: {}", cli.sender))?;

    // 1. Ensure dataset exists.
    if !cli.dataset.exists() {
        info!("dataset not found at {}; downloading", cli.dataset.display());
        if let Some(parent) = cli.dataset.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = reqwest::get(TRADES_DOWNLOAD_URL)
            .await?
            .bytes()
            .await?;
        std::fs::write(&cli.dataset, &bytes)?;
    }

    // 2. Sample trades.
    let all_trades = sampler::parse_dataset(&cli.dataset)?;
    info!("loaded {} trades from dataset", all_trades.len());
    let sampled = sampler::stratified_sample(&all_trades, cli.n, cli.max_per_pair, cli.seed)?;
    info!("sampled {} trades (cap {}/pair, seed {})", sampled.len(), cli.max_per_pair, cli.seed);
    let trades_json_path = cli.out_dir.join("trades.json");
    std::fs::write(&trades_json_path, serde_json::to_string_pretty(&sampled)?)?;

    // 3. Connect to Fynd and mainnet RPC.
    let quoter = Quoter::connect(&cli.fynd_url, &cli.rpc_url, sender).await?;
    quoter
        .wait_healthy(&cli.fynd_url)
        .await?;
    info!("fynd healthy at {}", cli.fynd_url);

    let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(
        cli.rpc_url
            .parse()
            .with_context(|| format!("invalid RPC URL: {}", cli.rpc_url))?,
    );
    let simulator = Simulator::new(provider, sender);

    // 4. One-shot gas price snapshot.
    let gas_price_wei = simulator.gas_price().await?;
    info!(
        "gas price snapshot: {} wei ({:.2} gwei)",
        gas_price_wei,
        gas_price_wei
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0) /
            1e9
    );

    // 5. Quote + simulate each trade.
    let mut rows: Vec<AuditRow> = Vec::with_capacity(sampled.len());
    for (i, trade) in sampled.iter().enumerate() {
        info!("[{}/{}] {} -> {}", i + 1, sampled.len(), trade.token_in, trade.token_out);

        let mut num_swaps: Option<usize> = None;
        let mut protocols: Option<String> = None;
        let (status, gas_estimate, actual_gas, error_reason) = match quoter.quote(trade).await {
            QuoteOutcome::NoRoute(reason) => (RowStatus::NoQuote, None, None, Some(reason)),
            QuoteOutcome::NoEncoding => {
                (RowStatus::NoEncoding, None, None, Some("no calldata in quote".into()))
            }
            QuoteOutcome::Error(e) => {
                warn!("quote error: {e}");
                (RowStatus::NoQuote, None, None, Some(e))
            }
            QuoteOutcome::Ok(quote) => {
                let estimate = quote.gas_estimate().clone();
                if let Some(route) = quote.route() {
                    num_swaps = Some(route.swaps().len());
                    protocols = Some(
                        route
                            .swaps()
                            .iter()
                            .map(|s| s.protocol().to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                match simulator
                    .simulate(quoter.client(), &quote)
                    .await
                {
                    SimOutcome::Ok { actual_gas } => {
                        (RowStatus::Success, Some(estimate), Some(actual_gas), None)
                    }
                    SimOutcome::Reverted { reason } => {
                        warn!("simulation reverted: {reason}");
                        (RowStatus::SimulationReverted, Some(estimate), None, Some(reason))
                    }
                }
            }
        };

        let (error_gas, error_wei, error_eth) = match (&gas_estimate, actual_gas) {
            (Some(est), Some(actual)) => match cost::compute(est, actual, &gas_price_wei) {
                Ok(b) => (Some(b.error_gas), Some(b.error_wei), Some(b.error_eth)),
                Err(e) => {
                    warn!("cost overflow: {e}");
                    (None, None, None)
                }
            },
            _ => (None, None, None),
        };

        rows.push(AuditRow {
            token_in: trade.token_in.clone(),
            token_out: trade.token_out.clone(),
            amount_in: trade.amount_in.clone(),
            gas_estimate,
            actual_gas,
            gas_price_wei: gas_price_wei.clone(),
            error_gas,
            error_wei,
            error_eth,
            status,
            error_reason,
            num_swaps,
            protocols,
        });
    }

    // 6. Write artifacts.
    let artifacts = Artifacts {
        trades_json: trades_json_path.display().to_string(),
        results_csv: cli
            .out_dir
            .join("results.csv")
            .display()
            .to_string(),
        report_md: cli
            .out_dir
            .join("report.md")
            .display()
            .to_string(),
    };

    report::write_csv(Path::new(&artifacts.results_csv), &rows)?;

    let eth_price_usd = fetch_eth_price_via_fynd(&quoter)
        .await
        .ok();
    let summary = report::summarize(&rows);
    report::write_markdown(
        Path::new(&artifacts.report_md),
        &gas_price_wei,
        eth_price_usd,
        &summary,
        &rows,
    )?;

    info!("wrote {}", artifacts.trades_json);
    info!("wrote {}", artifacts.results_csv);
    info!("wrote {}", artifacts.report_md);
    Ok(())
}

/// Fetch an ETH price via a single 1-WETH→USDC Fynd quote. The result is
/// reported in the header of `report.md` for reader context only; it is not
/// used in any aggregate math (all totals stay in ETH).
async fn fetch_eth_price_via_fynd(quoter: &Quoter) -> anyhow::Result<f64> {
    let trade = crate::types::AuditTrade {
        token_in: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
        token_out: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
        amount_in: "1000000000000000000".into(),
        sender: "0x000000000000000000000000000000000000beef".into(),
    };
    match quoter.quote(&trade).await {
        QuoteOutcome::Ok(q) => {
            // USDC is 6 decimals; divide amount_out by 1e6 for dollars.
            let out = q.amount_out().to_string();
            let out_f = f64::from_str(&out).unwrap_or(0.0);
            Ok(out_f / 1e6)
        }
        _ => anyhow::bail!("could not fetch ETH price from Fynd"),
    }
}
