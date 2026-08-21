//! exclusive-pool-pnl — LP fee revenue and markout for the Fynd exclusive Ekubo pool.
//!
//! The pool is hardcoded in [`pool`]. See `README.md` for usage and for what the numbers mean.

mod chain;
mod pnl;
mod pool;
mod prices;
mod report;

use std::path::PathBuf;

use alloy::{
    network::Ethereum,
    providers::{Provider as _, ProviderBuilder, RootProvider},
};
use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Seconds of reference prices fetched beyond the last swap, so the longest markout resolves.
const PRICE_TAIL_SECS: u64 = 3_600;

/// Seconds of reference prices fetched before the first swap, so its own minute is covered.
const PRICE_LEAD_SECS: u64 = 600;

#[derive(Parser)]
#[command(name = "exclusive-pool-pnl")]
#[command(about = "LP fee revenue and markout for the Fynd exclusive Ekubo pool")]
struct Cli {
    #[arg(long, env = "RPC_URL")]
    rpc_url: String,

    /// First block to scan. Defaults to the extension's deployment block.
    #[arg(long, default_value_t = pool::DEPLOY_BLOCK)]
    from_block: u64,

    /// Last block to scan. Defaults to the chain head.
    #[arg(long)]
    to_block: Option<u64>,

    /// Block span of a single `eth_getLogs` call. Most nodes cap this at 1 000.
    #[arg(long, default_value_t = 1_000)]
    chunk: u64,

    /// Maximum in-flight RPC requests.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Attempts per RPC request before giving up.
    #[arg(long, default_value_t = 5)]
    retries: u32,

    /// Markout horizons in seconds, applied to every swap.
    #[arg(long, value_delimiter = ',', default_values_t = [0u64, 300, 3_600])]
    markout_secs: Vec<u64>,

    /// Skip the reference-price download and report fees only.
    #[arg(long)]
    no_prices: bool,

    /// Write the full result set here as JSON.
    #[arg(long)]
    json: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    if cli.chunk == 0 || cli.concurrency == 0 {
        bail!("--chunk and --concurrency must be greater than zero");
    }
    if cli.markout_secs.is_empty() {
        bail!("--markout-secs needs at least one horizon");
    }

    let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(
        cli.rpc_url
            .parse()
            .with_context(|| format!("invalid RPC URL: {}", cli.rpc_url))?,
    );
    let to_block = match cli.to_block {
        Some(block) => block,
        None => provider
            .get_block_number()
            .await
            .context("could not read the chain head")?,
    };
    if cli.from_block > to_block {
        bail!("--from-block {} is past --to-block {to_block}", cli.from_block);
    }

    info!("scanning blocks {}..={to_block} for pool {}", cli.from_block, pool::POOL_ID);
    let scan = chain::ScanConfig {
        from_block: cli.from_block,
        to_block,
        chunk: cli.chunk,
        concurrency: cli.concurrency,
        retries: cli.retries,
    };
    let interactions = chain::fetch_interactions(&provider, scan).await?;
    let swap_count = interactions
        .iter()
        .filter(|interaction| interaction.is_swap())
        .count();
    info!("found {} pool interactions, {swap_count} of them swaps", interactions.len());
    if swap_count == 0 {
        println!("no swaps on the pool in blocks {}..={to_block}", cli.from_block);
        return Ok(());
    }

    let prices = if cli.no_prices {
        prices::PriceSeries::default()
    } else {
        fetch_prices(&interactions, &cli.markout_secs).await?
    };
    if !cli.no_prices {
        info!("loaded {} reference candles", prices.len());
    }

    let swaps = pnl::build(&interactions, &prices, &cli.markout_secs);
    let primary = cli
        .markout_secs
        .iter()
        .copied()
        .min()
        .unwrap_or_default();
    report::print_swaps(&swaps, primary);
    report::print_totals(&swaps, &cli.markout_secs);
    if let Some(path) = &cli.json {
        report::write_json(&swaps, &cli.markout_secs, path)?;
        info!("wrote {}", path.display());
    }
    Ok(())
}

/// Downloads reference prices covering every swap and every requested horizon.
async fn fetch_prices(
    interactions: &[chain::Interaction],
    horizons: &[u64],
) -> Result<prices::PriceSeries> {
    let stamps: Vec<u64> = interactions
        .iter()
        .filter(|interaction| interaction.is_swap())
        .map(|interaction| interaction.timestamp)
        .collect();
    let first = stamps
        .iter()
        .min()
        .copied()
        .unwrap_or_default();
    let last = stamps
        .iter()
        .max()
        .copied()
        .unwrap_or_default();
    let tail = horizons
        .iter()
        .copied()
        .max()
        .unwrap_or(PRICE_TAIL_SECS)
        .max(PRICE_TAIL_SECS);
    prices::PriceSeries::fetch(
        pool::REFERENCE_SYMBOL,
        first.saturating_sub(PRICE_LEAD_SECS),
        last + tail,
    )
    .await
}
