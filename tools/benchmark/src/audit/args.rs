//! CLI arguments for the `audit` subcommand.

use clap::Parser;

/// Compare Fynd quote performance against external DEX aggregators across multiple blocks.
#[derive(Parser, Debug)]
#[command(
    about = "Audit Fynd quote quality against external aggregators",
    long_about = "Audit Fynd quote quality against external aggregators.\n\n\
        Samples the top-N token pairs from the trade dataset, fires quotes at both Fynd and \
        the configured aggregator(s) for each (pair, amount) combination once per block, and \
        writes a per-trade JSON table plus a per-pair console summary.\n\n\
        Requires a healthy Fynd solver on --fynd-url."
)]
pub struct Args {
    /// Fynd solver base URL.
    #[arg(long, env = "FYND_URL", default_value = "http://localhost:3000")]
    pub fynd_url: String,

    /// Nordstern Finance base URL.
    #[arg(long, default_value = "https://api.nordstern.finance")]
    pub nordstern_url: String,

    /// KyberSwap Aggregator API base URL.
    #[arg(long, default_value = "https://aggregator-api.kyberswap.com")]
    pub kyberswap_url: String,

    /// KyberSwap chain slug (must match the URL path segment, e.g. "ethereum", "base").
    #[arg(long, default_value = "ethereum")]
    pub kyberswap_chain: String,

    /// 0x Swap API v2 base URL.
    #[arg(long, default_value = "https://api.0x.org")]
    pub zerox_url: String,

    /// 0x API key (env: ZRX_API_KEY).
    #[arg(long, env = "ZRX_API_KEY", default_value = "")]
    pub zerox_api_key: String,

    /// EVM chain ID passed to aggregator APIs (1 = Ethereum mainnet).
    #[arg(long, default_value_t = 1)]
    pub chain_id: u64,

    /// Number of blocks to spread trades across. The full trade list is divided into this
    /// many chunks; each chunk fires on its own block. Larger values slow the run down but
    /// keep per-block request counts low for rate-limited aggregator APIs.
    #[arg(long, default_value_t = 10)]
    pub blocks: usize,

    /// Top-N token pairs ranked by trade frequency in the dataset.
    #[arg(long, default_value_t = 25)]
    pub top_pairs: usize,

    /// Number of representative amounts per pair (evenly-spaced percentiles).
    #[arg(long, default_value_t = 10)]
    pub amounts_per_pair: usize,

    /// Path to the 10k aggregator trade dataset (run `download-trades` to fetch it).
    /// Falls back to the embedded 50-trade sample when omitted.
    #[arg(long)]
    pub trade_data: Option<String>,

    /// Per-quote timeout in milliseconds for Fynd.
    #[arg(long, default_value_t = 10_000)]
    pub timeout_ms: u64,

    /// Maximum parallel in-flight requests (controls load on aggregator APIs).
    #[arg(long, default_value_t = 5)]
    pub concurrency: usize,

    /// Sample every Nth block. 1 = every block (~12 s apart), 5 = every ~60 s, etc.
    /// Use a higher value for long runs to reduce aggregator API load.
    #[arg(long, default_value_t = 1)]
    pub block_stride: usize,

    /// Per-aggregator request rate cap (requests/sec). Each aggregator is paced by its own
    /// token bucket so a slow API (e.g. KyberSwap) doesn't drop samples while fast ones run
    /// freely. Set to 0 to disable pacing for that aggregator. Fynd is never paced.
    #[arg(long, default_value_t = 20.0)]
    pub nordstern_rps: f64,

    /// Per-aggregator request rate cap for KyberSwap (requests/sec). KyberSwap's public API
    /// rate-limits aggressively; keep this low to avoid 429s. 0 disables pacing.
    #[arg(long, default_value_t = 3.0)]
    pub kyberswap_rps: f64,

    /// Per-aggregator request rate cap for 0x (requests/sec). 0 disables pacing.
    #[arg(long, default_value_t = 5.0)]
    pub zerox_rps: f64,

    /// Max retries per aggregator request on a rate-limit (429) or 5xx response, with
    /// exponential backoff. Recovers samples that would otherwise be dropped.
    #[arg(long, default_value_t = 3)]
    pub aggregator_max_retries: u32,

    /// Base backoff in milliseconds for aggregator retries (doubled each attempt, plus jitter).
    #[arg(long, default_value_t = 250)]
    pub aggregator_retry_base_ms: u64,

    /// Path to write the full per-trade JSON results.
    #[arg(long, default_value = "audit_results.json")]
    pub output: String,

    /// Ethereum RPC URL for eth_call validation of Fynd quotes.
    ///
    /// When set, each successful Fynd quote is re-executed via `eth_call` at the
    /// quote block using state overrides to inject token balance and allowance.
    /// The on-chain result is compared against Fynd's quoted `amount_out` and
    /// stored as `eth_call_amount_out` / `eth_call_diff_bps` in the JSON output.
    /// Omit to skip validation entirely.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,

    /// Slippage tolerance passed to `EncodingOptions` when `--rpc-url` is set (basis points).
    /// Controls the `min_amount_out` encoded in the router calldata.
    #[arg(long, default_value_t = 50)]
    pub eth_call_slippage_bps: u32,

    /// Minimum approximate USD value for trade amounts sampled from the dataset.
    ///
    /// Trades below this threshold are excluded before percentile sampling, preventing
    /// sub-dust amounts from appearing as representative test cases. Prices are
    /// approximate and hardcoded for well-known tokens; unknown tokens are unaffected.
    /// Set to 0 to disable filtering (default).
    #[arg(long, default_value_t = 0.0)]
    pub min_amount_usd: f64,

    /// Upward adjustment applied to Fynd's eth_call amount-out (basis points).
    ///
    /// Scales the on-chain result up before storing `eth_call_amount_out` and recomputing
    /// `eth_call_diff_bps`. Use this to simulate zero-fee output: e.g. 10 removes an
    /// approximate 1 bps taker fee from the on-chain comparison.
    #[arg(long, default_value_t = 0)]
    pub eth_call_baseline_fee_bps: u32,
}
