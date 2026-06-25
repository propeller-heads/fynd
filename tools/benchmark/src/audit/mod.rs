//! `audit` subcommand — compare Fynd quotes against external DEX aggregators.
//!
//! Samples the top-N token pairs from the trade dataset, fires quotes at Fynd and
//! each configured aggregator in parallel across multiple blocks, and writes a
//! per-trade comparison table plus a console summary.

mod args;
mod blocks;
mod execute;
mod output;
mod summary;

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::providers::ProviderBuilder;
use anyhow::{Context, Result};
pub use args::Args;
use fynd_client::{FyndClient, FyndClientBuilder, RetryConfig};
use fynd_tools_common::{
    aggregator::AggregatorClient, fynd::FyndAggregator, swap_simulation::EthCallRunner,
};
use governor::DefaultDirectRateLimiter;
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::{info, warn};

use crate::{
    aggregator::{KyberswapClient, NordsternClient, ZeroExClient},
    audit::{
        execute::{execute_trade, make_rate_limiter, QuoteTask, RetryCfg, TradeConfig},
        output::{AuditConfig, AuditOutput, TradeResult},
    },
    pair_selector::{select_top_pairs, PairSpec},
    requests::{load_all_embedded_templates, load_all_templates_from_file},
};

pub async fn run(args: Args) -> Result<()> {
    let pairs = load_pairs(&args)?;
    let fynd = build_fynd_client(&args)?;
    let eth_call_slippage = args.eth_call_slippage_bps as f64 / 10_000.0;
    let eth_call_runner = build_eth_call_runner(&args, eth_call_slippage)?;
    let participants = build_participants(&args, fynd.clone(), eth_call_slippage)?;
    health_check(&fynd).await?;

    // Divide the full trade list into per-block chunks. Each trade fires exactly once on its
    // assigned block, keeping per-block request counts low so we stay within rate limits.
    let all_specs: Vec<(usize, usize)> = (0..pairs.len())
        .flat_map(|pi| (0..pairs[pi].amounts.len()).map(move |ai| (pi, ai)))
        .collect();
    let total_trades = all_specs.len();
    let n_blocks = args.blocks.min(total_trades).max(1);
    let chunk_size = total_trades.div_ceil(n_blocks);
    let chunks: Vec<&[(usize, usize)]> = all_specs.chunks(chunk_size).collect();
    let actual_blocks = chunks.len();

    info!(
        "Spreading {} trades across {} blocks (~{} trades/block, ~{} agg requests/block).",
        total_trades,
        actual_blocks,
        chunk_size,
        chunk_size * (participants.len() - 1),
    );

    // Per-aggregator rate limiters (Fynd is never paced). Keyed by `AggregatorClient::name`.
    let mut rate_limiters: HashMap<String, Arc<DefaultDirectRateLimiter>> = HashMap::new();
    for (name, rps) in [
        ("nordstern", args.nordstern_rps),
        ("kyberswap", args.kyberswap_rps),
        ("0x", args.zerox_rps),
    ] {
        if let Some(limiter) = make_rate_limiter(rps)? {
            rate_limiters.insert(name.to_string(), limiter);
        }
    }
    let rate_limiters = Arc::new(rate_limiters);
    let retry = RetryCfg {
        max_retries: args.aggregator_max_retries,
        base_ms: args.aggregator_retry_base_ms,
    };

    let runner = BlockRunner {
        pairs: &pairs,
        participants: &participants,
        fynd: &fynd,
        eth_call_runner: eth_call_runner.as_ref(),
        rate_limiters: &rate_limiters,
        retry,
        args: &args,
    };

    let mut all_results: Vec<TradeResult> = Vec::with_capacity(total_trades);
    for (block_idx, chunk) in chunks.iter().enumerate() {
        let block_results = runner
            .run_block(block_idx, chunk, actual_blocks)
            .await?;
        all_results.extend(block_results);
    }

    let output = build_output(&args, all_results, actual_blocks, chunk_size);
    write_output(&args, &output)?;
    summary::print_summary(&output.results);
    Ok(())
}

/// Load the trade dataset and derive the top token pairs (with representative amounts).
fn load_pairs(args: &Args) -> Result<Vec<PairSpec>> {
    let trades = match &args.trade_data {
        Some(path) => load_all_templates_from_file(path)
            .map_err(|e| anyhow::anyhow!("failed to load trade data from {path}: {e}"))?,
        None => {
            warn!(
                "No --trade-data provided; using embedded 50-trade sample. \
                 Run `download-trades` for the full 10k dataset."
            );
            load_all_embedded_templates()
        }
    };

    let pairs =
        select_top_pairs(&trades, args.top_pairs, args.amounts_per_pair, args.min_amount_usd);
    if pairs.is_empty() {
        anyhow::bail!("no valid pairs found in trade dataset");
    }

    info!("Derived {} pairs from trade dataset ({} trades).", pairs.len(), trades.len());
    for p in &pairs {
        info!(
            "  {} ({} amounts: {}..{})",
            p.label,
            p.amounts.len(),
            p.amounts
                .first()
                .unwrap_or(&String::new()),
            p.amounts
                .last()
                .unwrap_or(&String::new())
        );
    }
    Ok(pairs)
}

/// Build the quote-only `FyndClient` used for quotes, health-checks, and block-waiting.
fn build_fynd_client(args: &Args) -> Result<Arc<FyndClient>> {
    let client = FyndClientBuilder::new(&args.fynd_url)
        .with_timeout(Duration::from_millis(args.timeout_ms))
        .with_retry(RetryConfig::new(1, Duration::from_millis(0), Duration::from_millis(0)))
        .build_quote_only()
        .map_err(|e| anyhow::anyhow!("failed to build Fynd client: {e}"))?;
    Ok(Arc::new(client))
}

/// Build the `eth_call` runner when `--rpc-url` is provided.
fn build_eth_call_runner(args: &Args, slippage: f64) -> Result<Option<Arc<EthCallRunner>>> {
    let Some(rpc_url) = &args.rpc_url else {
        return Ok(None);
    };
    let url = rpc_url
        .parse::<reqwest::Url>()
        .with_context(|| format!("invalid --rpc-url: {rpc_url}"))?;
    let provider = ProviderBuilder::default().connect_http(url);
    let runner = Arc::new(EthCallRunner::new(provider));
    info!(
        "eth_call validation enabled (rpc: {rpc_url}, sender: {}, slippage: {:.2}%)",
        runner.sender_hex(),
        slippage * 100.0
    );
    Ok(Some(runner))
}

/// Assemble the participant list: Fynd first (the baseline), then external aggregators.
fn build_participants(
    args: &Args,
    fynd: Arc<FyndClient>,
    slippage: f64,
) -> Result<Vec<Arc<dyn AggregatorClient>>> {
    let mut participants: Vec<Arc<dyn AggregatorClient>> =
        vec![Arc::new(FyndAggregator::new(fynd, args.timeout_ms, slippage))];
    participants.push(Arc::new(NordsternClient::new(&args.nordstern_url, args.chain_id)?));
    participants.push(Arc::new(KyberswapClient::new(&args.kyberswap_url, &args.kyberswap_chain)?));
    if !args.zerox_api_key.is_empty() {
        participants.push(Arc::new(ZeroExClient::new(
            &args.zerox_url,
            args.chain_id,
            &args.zerox_api_key,
        )?));
    }
    Ok(participants)
}

async fn health_check(fynd: &FyndClient) -> Result<()> {
    let health = fynd
        .health()
        .await
        .context("Fynd health check failed — is the solver running?")?;
    if !health.healthy() {
        anyhow::bail!(
            "Fynd solver is not healthy (last_update={}ms, pools={})",
            health.last_update_ms(),
            health.num_solver_pools()
        );
    }
    info!(
        "Fynd healthy — {} solver pools, last update {}ms ago.",
        health.num_solver_pools(),
        health.last_update_ms()
    );
    Ok(())
}

fn build_output(
    args: &Args,
    results: Vec<TradeResult>,
    actual_blocks: usize,
    chunk_size: usize,
) -> AuditOutput {
    AuditOutput {
        config: AuditConfig {
            fynd_url: args.fynd_url.clone(),
            nordstern_url: args.nordstern_url.clone(),
            chain_id: args.chain_id,
            top_pairs: args.top_pairs,
            amounts_per_pair: args.amounts_per_pair,
            blocks_used: actual_blocks,
            trades_per_block: chunk_size,
            block_stride: args.block_stride,
            total_trades: results.len(),
        },
        results,
    }
}

fn write_output(args: &Args, output: &AuditOutput) -> Result<()> {
    let json = serde_json::to_string_pretty(output)?;
    std::fs::write(&args.output, &json)?;
    info!("Full results saved to: {}", args.output);
    Ok(())
}

/// Shared context for running each block's batch of quotes.
struct BlockRunner<'a> {
    pairs: &'a [PairSpec],
    participants: &'a [Arc<dyn AggregatorClient>],
    fynd: &'a FyndClient,
    eth_call_runner: Option<&'a Arc<EthCallRunner>>,
    rate_limiters: &'a Arc<HashMap<String, Arc<DefaultDirectRateLimiter>>>,
    retry: RetryCfg,
    args: &'a Args,
}

impl BlockRunner<'_> {
    /// Wait for the target block, then fire every (pair, amount) in `chunk` concurrently and
    /// collect the per-trade results.
    async fn run_block(
        &self,
        block_idx: usize,
        chunk: &[(usize, usize)],
        actual_blocks: usize,
    ) -> Result<Vec<TradeResult>> {
        if block_idx == 0 {
            info!("Waiting for first block…");
            blocks::wait_for_fresh_block(self.fynd).await?;
        } else {
            info!("Waiting for block {} of {}…", block_idx + 1, actual_blocks);
            blocks::wait_for_next_sample(self.fynd, self.args.block_stride).await?;
        }

        // Pin every quote and eth_call in this chunk to the same block state.
        let quote_block = match self.eth_call_runner {
            Some(runner) => runner.latest_block_hash().await,
            None => None,
        };

        info!("  Block ready — firing {} quote pairs…", chunk.len());

        let sem = Arc::new(Semaphore::new(self.args.concurrency));
        let mut jset: JoinSet<Result<TradeResult>> = JoinSet::new();

        for &(pair_idx, amount_idx) in chunk.iter() {
            let pair = self.pairs[pair_idx].clone();
            let amount = pair.amounts[amount_idx].clone();
            let participants = self.participants.to_vec();
            let sem = sem.clone();
            let block_sample = block_idx + 1;
            let rate_limiters = self.rate_limiters.clone();
            let retry = self.retry;
            let baseline_fee_bps = self.args.eth_call_baseline_fee_bps;
            let runner = self.eth_call_runner.cloned();

            jset.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .expect("semaphore acquire");
                execute_trade(
                    participants,
                    QuoteTask { block_sample, pair, amount, amount_percentile_idx: amount_idx },
                    TradeConfig {
                        rate_limiters,
                        retry,
                        eth_call_runner: runner,
                        quote_block,
                        baseline_fee_bps,
                    },
                )
                .await
            });
        }

        let mut results = Vec::new();
        while let Some(result) = jset.join_next().await {
            match result {
                Ok(Ok(trade)) => results.push(trade),
                Ok(Err(e)) => warn!("trade task error: {e}"),
                Err(e) => warn!("task panicked: {e}"),
            }
        }
        info!("  Collected {}/{} results.", results.len(), chunk.len());
        Ok(results)
    }
}
