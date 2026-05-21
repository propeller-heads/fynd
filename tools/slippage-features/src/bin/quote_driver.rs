//! Quote driver binary: replays benchmark trades through Fynd's HTTP API
//! on a configurable schedule for continuous slippage data collection.

use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "quote-driver")]
struct Args {
    /// Path to trades JSON file (from fynd-benchmark download-trades)
    #[arg(long)]
    trades_file: PathBuf,

    /// Fynd API URL
    #[arg(long, default_value = "http://localhost:3000")]
    fynd_url: String,

    /// Sender address for quotes
    #[arg(long, default_value = "0x0000000000000000000000000000000000000001")]
    sender: String,

    /// Interval between quote rounds in seconds
    #[arg(long, default_value_t = 12)]
    interval_secs: u64,

    /// Max trades per round (0 = all)
    #[arg(long, default_value_t = 0)]
    batch_size: usize,
}

/// A single trade entry matching the benchmark dataset format.
#[derive(Deserialize)]
struct TradeFile {
    orders: Vec<TradeOrder>,
}

/// A single order within a trade entry.
#[derive(Deserialize)]
struct TradeOrder {
    token_in: String,
    token_out: String,
    amount: String,
    #[serde(default)]
    sender: Option<String>,
}

fn load_trades(path: &std::path::Path) -> anyhow::Result<Vec<TradeFile>> {
    let content = std::fs::read_to_string(path)?;
    let trades: Vec<TradeFile> = serde_json::from_str(&content)?;
    if trades.is_empty() {
        anyhow::bail!("trades file is empty: {}", path.display());
    }
    Ok(trades)
}

struct RoundStats {
    sent: u64,
    success: u64,
    failed: u64,
}

async fn send_quote(
    client: &reqwest::Client,
    fynd_url: &str,
    trade: &TradeFile,
    sender: &str,
) -> bool {
    let Some(order) = trade.orders.first() else {
        warn!("trade has no orders, skipping");
        return false;
    };

    let effective_sender = order
        .sender
        .as_deref()
        .unwrap_or(sender);

    let body = serde_json::json!({
        "orders": [{
            "token_in": order.token_in,
            "token_out": order.token_out,
            "amount": order.amount,
            "side": "sell",
            "sender": effective_sender,
        }]
    });

    let url = format!("{fynd_url}/v1/quote");
    match client
        .post(&url)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                true
            } else {
                warn!(
                    status = %resp.status(),
                    token_in = %order.token_in,
                    token_out = %order.token_out,
                    "quote request failed"
                );
                false
            }
        }
        Err(e) => {
            warn!(error = %e, "quote request error");
            false
        }
    }
}

async fn run_round(client: &reqwest::Client, trades: &[TradeFile], args: &Args) -> RoundStats {
    let batch: &[TradeFile] = if args.batch_size > 0 && args.batch_size < trades.len() {
        &trades[..args.batch_size]
    } else {
        trades
    };

    let mut stats = RoundStats { sent: 0, success: 0, failed: 0 };

    for trade in batch {
        stats.sent += 1;
        if send_quote(client, &args.fynd_url, trade, &args.sender).await {
            stats.success += 1;
        } else {
            stats.failed += 1;
        }
    }

    stats
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!(
        trades_file = %args.trades_file.display(),
        fynd_url = %args.fynd_url,
        interval_secs = args.interval_secs,
        batch_size = args.batch_size,
        "starting quote driver"
    );

    let trades = load_trades(&args.trades_file)?;
    info!(trades = trades.len(), "loaded trades");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut total_sent: u64 = 0;
    let mut total_success: u64 = 0;
    let mut round_count: u64 = 0;

    loop {
        round_count += 1;
        let round_stats = run_round(&client, &trades, &args).await;

        total_sent += round_stats.sent;
        total_success += round_stats.success;

        let success_rate =
            if total_sent > 0 { (total_success as f64 / total_sent as f64) * 100.0 } else { 0.0 };

        info!(
            round = round_count,
            sent = round_stats.sent,
            success = round_stats.success,
            failed = round_stats.failed,
            total_sent = total_sent,
            total_success = total_success,
            success_rate_pct = format!("{success_rate:.1}"),
            "round complete"
        );

        tokio::time::sleep(std::time::Duration::from_secs(args.interval_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parsing_defaults() {
        let args = Args::parse_from(["quote-driver", "--trades-file", "/tmp/trades.json"]);
        assert_eq!(args.trades_file, PathBuf::from("/tmp/trades.json"));
        assert_eq!(args.fynd_url, "http://localhost:3000");
        assert_eq!(args.sender, "0x0000000000000000000000000000000000000001");
        assert_eq!(args.interval_secs, 12);
        assert_eq!(args.batch_size, 0);
    }

    #[test]
    fn cli_parsing_all_args() {
        let args = Args::parse_from([
            "quote-driver",
            "--trades-file",
            "/data/trades.json",
            "--fynd-url",
            "http://remote:8080",
            "--sender",
            "0xdeadbeef",
            "--interval-secs",
            "30",
            "--batch-size",
            "100",
        ]);
        assert_eq!(args.fynd_url, "http://remote:8080");
        assert_eq!(args.sender, "0xdeadbeef");
        assert_eq!(args.interval_secs, 30);
        assert_eq!(args.batch_size, 100);
    }

    #[test]
    fn load_trades_valid_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("trades.json");
        let content = serde_json::json!([{
            "orders": [{
                "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "amount": "1000000000000000000"
            }]
        }]);
        std::fs::write(&path, content.to_string()).expect("write trades file");

        let trades = load_trades(&path).expect("load trades");
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].orders.len(), 1);
        assert_eq!(trades[0].orders[0].amount, "1000000000000000000");
    }

    #[test]
    fn load_trades_empty_array_fails() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "[]").expect("write empty file");

        let result = load_trades(&path);
        assert!(result.is_err());
    }

    #[test]
    fn load_trades_nonexistent_file_fails() {
        let result = load_trades(std::path::Path::new("/nonexistent/trades.json"));
        assert!(result.is_err());
    }

    #[test]
    fn load_trades_with_full_benchmark_format() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("trades.json");
        let content = serde_json::json!([
            {
                "orders": [{
                    "id": "",
                    "token_in": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                    "token_out": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    "amount": "179820000000000000",
                    "side": "sell",
                    "sender": "0x0000000000000000000000000000000000000001",
                    "receiver": null
                }],
                "options": {
                    "timeout_ms": 5000,
                    "min_responses": null,
                    "max_gas": null
                }
            }
        ]);
        std::fs::write(&path, content.to_string()).expect("write trades");

        let trades = load_trades(&path).expect("load trades");
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].orders[0].token_in, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    }

    #[test]
    fn sender_defaults_when_absent() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("nosender.json");
        let content = serde_json::json!([{
            "orders": [{
                "token_in": "0xaaa",
                "token_out": "0xbbb",
                "amount": "100"
            }]
        }]);
        std::fs::write(&path, content.to_string()).expect("write");

        let trades = load_trades(&path).expect("load");
        assert!(trades[0].orders[0].sender.is_none());
    }
}
