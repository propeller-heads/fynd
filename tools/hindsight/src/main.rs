mod decoder;
mod resolve;
mod telemetry;
mod usd;
mod verify;

use std::time::Instant;

use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    decoder::{DecodedTrade, Decoder, Registry},
    resolve::monitor::MonitorArgs,
    verify::allium::AlliumClient,
};

#[derive(Parser)]
#[command(name = "hindsight", about = "Decode solver swaps from on-chain data")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode solver trades from a block or range.
    Decode(DecodeArgs),
    /// Decode and compare against Allium's `aggregator_trades` ground truth.
    Verify(VerifyArgs),
    /// Live monitor: drive an in-process solver block-by-block, re-solving each block's settled
    /// trades at top-of-block (N-1) and back-of-block (N).
    Monitor(MonitorArgs),
}

/// Chain selection shared by every subcommand: which chain to operate on and how to reach it.
/// Chain-specific configuration beyond the RPC endpoint and address book belongs here too.
#[derive(Args)]
pub(crate) struct ChainArgs {
    /// Chain to operate on — selects the decoder's address book
    #[arg(long = "chain", value_name = "CHAIN", default_value = "ethereum")]
    pub name: String,

    /// Chain RPC URL
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: String,

    /// Decoder address-book TOML (defaults to the chain's built-in book)
    #[arg(long, env = "HINDSIGHT_REGISTRY")]
    pub registry: Option<std::path::PathBuf>,
}

/// Block selection shared by the decode and verify subcommands.
#[derive(Args)]
struct BlockArgs {
    /// Block number to process (latest if omitted)
    #[arg(long)]
    block: Option<u64>,

    /// Range of blocks to process (e.g. 21000000-21000010)
    #[arg(long, conflicts_with = "block")]
    range: Option<String>,
}

#[derive(Args)]
struct DecodeArgs {
    #[command(flatten)]
    chain: ChainArgs,

    #[command(flatten)]
    blocks: BlockArgs,

    /// Output as JSON instead of human-readable
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct VerifyArgs {
    #[command(flatten)]
    chain: ChainArgs,

    #[command(flatten)]
    blocks: BlockArgs,

    /// Allium API key
    #[arg(long, env = "ALLIUM_API_KEY")]
    allium_key: String,

    /// Allium saved query ID, parameterized by `block_number`. A saved query ID is scoped to the
    /// workspace that created it, so register this SQL in your own Allium workspace and pass its
    /// ID:
    ///
    ///   SELECT project, protocol, `token_sold_address`, `token_sold_symbol`, `token_sold_amount`,
    ///          `usd_sold_amount`, `token_bought_address`, `token_bought_symbol`,
    /// `token_bought_amount`,          `usd_bought_amount`, `sender_address`, `to_address`,
    /// `transaction_hash`,          `transaction_fees_usd`, `block_number`, `block_timestamp`,
    /// `log_index`   FROM `ethereum.dex.aggregator_trades`
    ///   WHERE `block_number` = {{`block_number`}}
    ///   ORDER BY `log_index`
    #[arg(long, env = "ALLIUM_QUERY_ID")]
    allium_query_id: String,

    /// Max allowed amount difference vs Allium, in basis points
    #[arg(long, default_value_t = 50.0)]
    tolerance_bps: f64,

    /// Output as JSON instead of human-readable
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ANSI colors only on a real terminal: in a pod they end up as escape sequences inside
    // Loki, where they break plain name=value field extraction.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_env("RUST_LOG").add_directive(
                "hindsight=info"
                    .parse()
                    .expect("valid static directive"),
            ),
        )
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    match Cli::parse().command {
        Command::Decode(args) => run_decode(args).await,
        Command::Verify(args) => run_verify(args).await,
        Command::Monitor(args) => resolve::monitor::run(args).await,
    }
}

#[expect(clippy::print_stdout)]
async fn run_decode(args: DecodeArgs) -> anyhow::Result<()> {
    let provider = provider_from(&args.chain.rpc_url)?;
    let blocks = resolve_blocks(&provider, args.blocks.block, args.blocks.range.as_deref()).await?;
    let registry = Registry::load(&args.chain.name, args.chain.registry.as_deref())?;
    let mut decoder = Decoder::new(provider, registry);

    let mut all_trades = Vec::new();
    for block_number in &blocks {
        info!(block = block_number, "decoding solver trades");
        let start = Instant::now();
        let trades = match decoder
            .decode_block(*block_number)
            .await
        {
            Ok(trades) => trades,
            Err(error) => {
                warn!(block = block_number, %error, "failed to decode block; skipping");
                continue;
            }
        };
        let elapsed_ms = start.elapsed().as_millis();

        if trades.is_empty() {
            info!(block = block_number, elapsed_ms, "no solver trades found");
        } else {
            info!(block = block_number, count = trades.len(), elapsed_ms, "decoded trades");
        }
        all_trades.extend(trades);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&all_trades)?);
    } else {
        print_trades(&all_trades);
    }
    Ok(())
}

#[expect(clippy::print_stdout)]
async fn run_verify(args: VerifyArgs) -> anyhow::Result<()> {
    let provider = provider_from(&args.chain.rpc_url)?;
    let blocks = resolve_blocks(&provider, args.blocks.block, args.blocks.range.as_deref()).await?;
    let allium = AlliumClient::new(args.allium_key, args.allium_query_id);
    let registry = Registry::load(&args.chain.name, args.chain.registry.as_deref())?;
    let mut decoder = Decoder::new(provider, registry);

    info!(blocks = blocks.len(), "verifying decoded trades against Allium");
    let start = Instant::now();
    let report = verify::run(&mut decoder, &allium, &blocks, args.tolerance_bps).await?;
    info!(elapsed_ms = start.elapsed().as_millis(), "verification complete");

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        report.print();
    }
    Ok(())
}

pub(crate) fn provider_from(rpc_url: &str) -> anyhow::Result<impl Provider> {
    let url: reqwest::Url = rpc_url
        .parse()
        .with_context(|| format!("invalid RPC URL: {rpc_url}"))?;
    Ok(ProviderBuilder::new().connect_http(url))
}

pub(crate) async fn resolve_blocks<P: Provider>(
    provider: &P,
    block: Option<u64>,
    range: Option<&str>,
) -> anyhow::Result<Vec<u64>> {
    if let Some(range) = range {
        parse_range(range)
    } else if let Some(block) = block {
        Ok(vec![block])
    } else {
        let latest = provider
            .get_block_number()
            .await
            .context("failed to fetch latest block number")?;
        Ok(vec![latest])
    }
}

#[expect(clippy::print_stdout)]
fn print_trades(trades: &[DecodedTrade]) {
    if trades.is_empty() {
        println!("No solver trades found.");
        return;
    }

    println!("\n{} solver trade(s) found:\n", trades.len());
    for trade in trades {
        println!("  tx:         {}", trade.tx_hash);
        println!("  tx_index:   {}", trade.tx_index);
        println!("  block:      {}", trade.block_number);
        println!("  venue:      {}", trade.venue);
        println!("  solver:     {}", trade.solver);
        println!("  sender:     {}", trade.sender);
        println!("  token_in:   {}", trade.token_in);
        println!("  amount_in:  {}", trade.amount_in);
        println!("  token_out:  {}", trade.token_out);
        println!("  amount_out: {}", trade.amount_out);
        if let Some(sandwich) = &trade.sandwich {
            println!(
                "  sandwich:   front={} back={} attacker={}",
                sandwich.front_tx, sandwich.back_tx, sandwich.attacker
            );
        }
        println!();
    }
}

const MAX_RANGE_BLOCKS: u64 = 1000;

fn parse_range(range: &str) -> anyhow::Result<Vec<u64>> {
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid range format: expected START-END, got '{range}'");
    }
    let start: u64 = parts[0]
        .parse()
        .with_context(|| format!("invalid start block: {}", parts[0]))?;
    let end: u64 = parts[1]
        .parse()
        .with_context(|| format!("invalid end block: {}", parts[1]))?;
    if end < start {
        anyhow::bail!("end block ({end}) must be >= start block ({start})");
    }
    if end - start > MAX_RANGE_BLOCKS {
        anyhow::bail!("range too large: {} blocks (max {MAX_RANGE_BLOCKS})", end - start);
    }
    Ok((start..=end).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_range_valid() {
        let blocks = parse_range("100-105").unwrap();
        assert_eq!(blocks, vec![100, 101, 102, 103, 104, 105]);
    }

    #[test]
    fn test_parse_range_single_block() {
        let blocks = parse_range("100-100").unwrap();
        assert_eq!(blocks, vec![100]);
    }

    #[test]
    fn test_parse_range_invalid_format() {
        assert!(parse_range("100").is_err());
        assert!(parse_range("100-200-300").is_err());
    }

    #[test]
    fn test_parse_range_reversed() {
        assert!(parse_range("200-100").is_err());
    }

    #[test]
    fn test_parse_range_too_large() {
        assert!(parse_range("0-1001").is_err());
    }
}
