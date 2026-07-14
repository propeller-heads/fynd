//! `generate-requests` subcommand.
//!
//! Generates a per-chain synthetic swap-request dataset for capacity load testing. Token pairs are
//! sampled weighted by pool count (mimicking real traffic skew) and amounts are log-spaced around
//! one whole token, so the dataset spans the amount range a solver sees in production. The output
//! is a JSON array loadable via `--requests-file`.
//!
//! Token and pool-count data comes from the same Tycho query the `derive-connector-tokens` command
//! uses, exposed as `fynd_rpc::protocols::fetch_token_pool_stats`.

use std::{collections::HashSet, path::PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use fynd_rpc::{
    config::defaults::default_tycho_url,
    parse_chain,
    protocols::{fetch_token_pool_stats, resolve_protocols, TokenPoolStats},
};
use num_bigint::BigUint;
use rand::{
    distr::{weighted::WeightedIndex, Distribution},
    rngs::StdRng,
    Rng, SeedableRng,
};
use serde::Serialize;
use tracing::info;

/// Placeholder sender used for every generated order (matches the aggregator-trade datasets).
const SENDER: &str = "0x0000000000000000000000000000000000000001";

/// Orders of magnitude spanned around one whole token, in each direction. A spread of 2 covers a
/// ~4-order-of-magnitude range (0.01 to 100 whole tokens).
const AMOUNT_LOG_SPREAD: f64 = 2.0;

/// Generate a synthetic per-chain request dataset for capacity testing.
#[derive(Parser, Debug)]
#[command(
    about = "Generate a synthetic per-chain request dataset for capacity testing",
    long_about = "Generate a synthetic per-chain request dataset for capacity testing.\n\n\
        Fetches token and pool-count data from Tycho, samples token pairs weighted by pool count, \
        and writes a JSON array of requests loadable via --requests-file.\n\n\
        Accepted --chain values: ethereum, base, unichain, bsc, arbitrum, polygon (case-insensitive). \
        zksync also parses but has no default Tycho URL, so pass --tycho-url for it."
)]
pub struct Args {
    /// Target chain: ethereum, base, unichain, bsc, arbitrum, or polygon.
    #[arg(long)]
    pub chain: String,

    /// Tycho URL. Defaults to the Fynd endpoint for the selected chain.
    #[arg(long)]
    pub tycho_url: Option<String>,

    /// Tycho API key (defaults to the TYCHO_API_KEY environment variable).
    #[arg(long, env = "TYCHO_API_KEY")]
    pub tycho_api_key: Option<String>,

    /// Disable TLS for the Tycho connection.
    #[arg(long)]
    pub disable_tls: bool,

    /// Protocol systems to query (comma-separated). Supports the `all_onchain` and
    /// `native_onchain` expansion tokens.
    #[arg(long, value_delimiter = ',', default_value = "all_onchain")]
    pub protocols: Vec<String>,

    /// Number of most-connected tokens to sample pairs from.
    #[arg(long, default_value_t = 50)]
    pub top_n_tokens: usize,

    /// Number of requests to generate.
    #[arg(long, default_value_t = 2000)]
    pub num_requests: usize,

    /// RNG seed. The output is deterministic for a fixed seed and token set.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Per-request quote timeout written into each request's options.
    #[arg(long, default_value_t = 5000)]
    pub timeout_ms: u64,

    /// Output JSON file path.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Serialize)]
struct GeneratedRequest {
    orders: Vec<GeneratedOrder>,
    options: RequestOptions,
}

#[derive(Serialize)]
struct GeneratedOrder {
    id: String,
    token_in: String,
    token_out: String,
    amount: String,
    side: String,
    sender: String,
    receiver: Option<String>,
}

#[derive(Serialize)]
struct RequestOptions {
    timeout_ms: u64,
    min_responses: Option<u64>,
    max_gas: Option<u64>,
}

/// Converts a log-spaced magnitude into a raw integer amount for a token with `decimals`.
///
/// The amount is centered at one whole token (`10^decimals` raw units) and shifted by `log_offset`
/// orders of magnitude. The value is split into a seven-significant-digit leading part and a power
/// of ten so the result stays an exact integer even for large `decimals`.
fn raw_amount(decimals: u32, log_offset: f64) -> BigUint {
    let magnitude = f64::from(decimals) + log_offset;
    let whole = magnitude.floor();
    let frac = magnitude - whole;
    // 10^frac is in [1, 10); scaling by 1e6 yields a 7-digit leading value in [1_000_000,
    // 10_000_000].
    let leading = (10f64.powf(frac) * 1e6).round() as u64;
    let whole = whole as i64;
    let ten = BigUint::from(10u32);

    if whole >= 6 {
        BigUint::from(leading) * ten.pow((whole - 6) as u32)
    } else if whole >= 0 {
        let scaled = BigUint::from(leading) / ten.pow((6 - whole) as u32);
        scaled.max(BigUint::from(1u32))
    } else {
        // Sub-unit magnitudes floor to the smallest representable amount.
        BigUint::from(1u32)
    }
}

/// Samples `num_requests` orders, drawing both tokens weighted by pool count and excluding
/// self-pairs. Deterministic for a fixed `seed` and token slice.
fn sample_requests(
    tokens: &[TokenPoolStats],
    num_requests: usize,
    seed: u64,
    timeout_ms: u64,
) -> Result<Vec<GeneratedRequest>> {
    if tokens.len() < 2 {
        bail!("need at least 2 tokens to sample distinct pairs, got {}", tokens.len());
    }

    let weights: Vec<u64> = tokens
        .iter()
        .map(|t| t.pool_count as u64)
        .collect();
    let distribution = WeightedIndex::new(&weights)
        .context("failed to build weighted token distribution (are all pool counts zero?)")?;
    let mut rng = StdRng::seed_from_u64(seed);

    let mut requests = Vec::with_capacity(num_requests);
    for _ in 0..num_requests {
        let in_idx = distribution.sample(&mut rng);
        let mut out_idx = distribution.sample(&mut rng);
        // Reject self-pairs; after a few draws fall back to a neighbour so a token that dominates
        // the weight distribution cannot loop forever.
        let mut attempts = 0;
        while out_idx == in_idx && attempts < 16 {
            out_idx = distribution.sample(&mut rng);
            attempts += 1;
        }
        if out_idx == in_idx {
            out_idx = (in_idx + 1) % tokens.len();
        }

        let token_in = &tokens[in_idx];
        let token_out = &tokens[out_idx];
        let log_offset = rng.random_range(-AMOUNT_LOG_SPREAD..=AMOUNT_LOG_SPREAD);
        let amount = raw_amount(token_in.decimals, log_offset).to_string();

        requests.push(GeneratedRequest {
            orders: vec![GeneratedOrder {
                id: String::new(),
                token_in: token_in.address.clone(),
                token_out: token_out.address.clone(),
                amount,
                side: "sell".to_string(),
                sender: SENDER.to_string(),
                receiver: None,
            }],
            options: RequestOptions { timeout_ms, min_responses: None, max_gas: None },
        });
    }

    Ok(requests)
}

fn print_summary(args: &Args, tokens: &[TokenPoolStats], requests: &[GeneratedRequest]) {
    let unique_pairs: HashSet<(&str, &str)> = requests
        .iter()
        .filter_map(|r| r.orders.first())
        .map(|o| (o.token_in.as_str(), o.token_out.as_str()))
        .collect();

    println!("\n=== generate-requests summary ===");
    println!("Chain:            {}", args.chain);
    println!("Tokens sampled:   {}", tokens.len());
    println!("Requests written: {}", requests.len());
    println!("Unique pairs:     {}", unique_pairs.len());
    println!("Output:           {}", args.output.display());
    if let Some(order) = requests
        .first()
        .and_then(|r| r.orders.first())
    {
        println!("Sample order:     {} of {} -> {}", order.amount, order.token_in, order.token_out);
    }
}

pub async fn run(args: Args) -> Result<()> {
    let chain =
        parse_chain(&args.chain).with_context(|| format!("invalid --chain '{}'", args.chain))?;

    let tycho_url = match args.tycho_url.clone() {
        Some(url) => url,
        None => default_tycho_url(&args.chain)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .to_string(),
    };

    let protocols = resolve_protocols(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &args.protocols,
    )
    .await?;
    info!("Resolved {} protocol system(s)", protocols.len());

    let mut tokens = fetch_token_pool_stats(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &protocols,
    )
    .await?;
    tokens.truncate(args.top_n_tokens);
    info!("Sampling {} request(s) from top {} token(s)", args.num_requests, tokens.len());

    let requests = sample_requests(&tokens, args.num_requests, args.seed, args.timeout_ms)?;
    let json = serde_json::to_string_pretty(&requests)?;
    std::fs::write(&args.output, json)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    print_summary(&args, &tokens, &requests);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::load_request_templates;

    fn fixture_tokens() -> Vec<TokenPoolStats> {
        vec![
            TokenPoolStats {
                address: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string(),
                symbol: "WETH".to_string(),
                decimals: 18,
                pool_count: 500,
            },
            TokenPoolStats {
                address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
                symbol: "USDC".to_string(),
                decimals: 6,
                pool_count: 300,
            },
            TokenPoolStats {
                address: "0xdac17f958d2ee523a2206206994597c13d831ec7".to_string(),
                symbol: "USDT".to_string(),
                decimals: 6,
                pool_count: 200,
            },
            TokenPoolStats {
                address: "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599".to_string(),
                symbol: "WBTC".to_string(),
                decimals: 8,
                pool_count: 50,
            },
        ]
    }

    fn amounts(requests: &[GeneratedRequest]) -> Vec<String> {
        requests
            .iter()
            .map(|r| r.orders[0].amount.clone())
            .collect()
    }

    #[test]
    fn raw_amount_centers_on_one_whole_token() {
        assert_eq!(raw_amount(18, 0.0).to_string(), "1000000000000000000");
        assert_eq!(raw_amount(6, 0.0).to_string(), "1000000");
    }

    #[test]
    fn raw_amount_spans_orders_of_magnitude() {
        // 0.01 and 100 whole USDC (6 decimals) at the spread bounds.
        assert_eq!(raw_amount(6, -2.0).to_string(), "10000");
        assert_eq!(raw_amount(6, 2.0).to_string(), "100000000");
    }

    #[test]
    fn raw_amount_low_decimals_floor_to_one() {
        assert_eq!(raw_amount(2, -2.0).to_string(), "1");
    }

    #[test]
    fn sample_requests_rejects_self_pairs() {
        let tokens = fixture_tokens();
        let requests = sample_requests(&tokens, 500, 7, 5000).unwrap();
        assert_eq!(requests.len(), 500);
        for request in &requests {
            let order = &request.orders[0];
            assert_ne!(order.token_in, order.token_out);
        }
    }

    #[test]
    fn sample_requests_is_seed_deterministic() {
        let tokens = fixture_tokens();
        let first = sample_requests(&tokens, 100, 42, 5000).unwrap();
        let second = sample_requests(&tokens, 100, 42, 5000).unwrap();
        assert_eq!(amounts(&first), amounts(&second));
        let different = sample_requests(&tokens, 100, 43, 5000).unwrap();
        assert_ne!(amounts(&first), amounts(&different));
    }

    #[test]
    fn sample_requests_needs_two_tokens() {
        let tokens = vec![fixture_tokens()[0].clone()];
        assert!(sample_requests(&tokens, 10, 42, 5000).is_err());
    }

    #[test]
    fn generated_dataset_round_trips_through_loader() {
        let tokens = fixture_tokens();
        let requests = sample_requests(&tokens, 250, 42, 5000).unwrap();
        let json = serde_json::to_string_pretty(&requests).unwrap();

        let path = std::env::temp_dir().join("generate_requests_round_trip.json");
        std::fs::write(&path, json).unwrap();
        let loaded = load_request_templates(path.to_str().unwrap(), 5000);
        std::fs::remove_file(&path).ok();

        let loaded = loaded.unwrap();
        assert_eq!(loaded.len(), 250);
        for request in &loaded {
            assert_ne!(request.token_in_addr(), request.token_out_addr());
            assert!(request
                .token_in_addr()
                .starts_with("0x"));
            assert!(!request.raw_amount().is_empty());
        }
    }
}
