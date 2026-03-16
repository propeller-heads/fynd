//! Download trades subcommand.
//!
//! Fetches real DEX trades from Dune Analytics and converts them into the
//! benchmark request JSON format (`--requests-file`).

use std::time::Duration;

use clap::Parser;
use serde::Deserialize;

const DUNE_API_BASE: &str = "https://api.dune.com/api/v1";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_POLL_ATTEMPTS: usize = 150;
const DEFAULT_SENDER: &str = "0x0000000000000000000000000000000000000001";

/// Download real DEX trades from Dune Analytics for benchmarking.
///
/// Queries the `dex.trades` table and saves results in the JSON format
/// expected by `--requests-file` in the `load` and `compare` subcommands.
/// Requires the `DUNE_API_KEY` environment variable.
#[derive(Parser, Debug)]
#[command(
    about = "Download real DEX trades from Dune Analytics",
    long_about = "Download real DEX trades from Dune Analytics.\n\n\
        Queries the dex.trades table for recent Ethereum mainnet trades \
        and converts them to the benchmark request JSON format.\n\n\
        Requires the DUNE_API_KEY environment variable."
)]
pub struct Args {
    /// Number of trades to download
    #[arg(short = 'n', long, default_value_t = 1000)]
    pub limit: usize,

    /// Minimum trade size in USD
    #[arg(long, default_value_t = 100.0)]
    pub min_usd: f64,

    /// Lookback window in hours
    #[arg(long, default_value_t = 24)]
    pub hours: u32,

    /// Blockchain to query
    #[arg(long, default_value = "ethereum")]
    pub chain: String,

    /// Output file path
    #[arg(short, long, default_value = "trades_1k_requests.json")]
    pub output: String,
}

#[derive(Deserialize)]
struct ExecuteResponse {
    execution_id: String,
}

#[derive(Deserialize)]
struct StatusResponse {
    state: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ResultsResponse {
    result: ResultData,
}

#[derive(Deserialize)]
struct ResultData {
    rows: Vec<TradeRow>,
}

#[derive(Deserialize)]
struct TradeRow {
    token_in: String,
    token_out: String,
    amount: String,
}

#[derive(serde::Serialize)]
struct RequestEntry {
    orders: Vec<Order>,
}

#[derive(serde::Serialize)]
struct Order {
    id: String,
    token_in: String,
    token_out: String,
    amount: String,
    side: String,
    sender: String,
}

fn build_sql(chain: &str, limit: usize, min_usd: f64, hours: u32) -> String {
    format!(
        "SELECT \
            LOWER(CAST(token_sold_address AS VARCHAR)) AS token_in, \
            LOWER(CAST(token_bought_address AS VARCHAR)) AS token_out, \
            CAST(token_sold_amount_raw AS VARCHAR) AS amount \
        FROM dex.trades \
        WHERE blockchain = '{chain}' \
            AND block_time >= NOW() - INTERVAL '{hours}' HOUR \
            AND amount_usd >= {min_usd} \
            AND token_sold_amount_raw IS NOT NULL \
            AND token_sold_address IS NOT NULL \
            AND token_bought_address IS NOT NULL \
        ORDER BY block_time DESC \
        LIMIT {limit}"
    )
}

async fn execute_sql(
    client: &reqwest::Client,
    api_key: &str,
    sql: &str,
) -> anyhow::Result<String> {
    let resp: ExecuteResponse = client
        .post(format!("{DUNE_API_BASE}/sql/execute"))
        .header("X-Dune-Api-Key", api_key)
        .json(&serde_json::json!({"sql": sql, "performance": "medium"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.execution_id)
}

async fn poll_until_done(
    client: &reqwest::Client,
    api_key: &str,
    execution_id: &str,
) -> anyhow::Result<()> {
    for attempt in 0..MAX_POLL_ATTEMPTS {
        let resp: StatusResponse = client
            .get(format!(
                "{DUNE_API_BASE}/execution/{execution_id}/status"
            ))
            .header("X-Dune-Api-Key", api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        match resp.state.as_str() {
            "QUERY_STATE_COMPLETED" => return Ok(()),
            "QUERY_STATE_FAILED" | "QUERY_STATE_CANCELLED" | "QUERY_STATE_EXPIRED" => {
                let error = resp.error.unwrap_or_else(|| "unknown".to_string());
                return Err(anyhow::anyhow!("Query {}: {error}", resp.state));
            }
            _ => {
                if attempt % 5 == 0 {
                    println!(
                        "  polling... state={} (attempt {})",
                        resp.state,
                        attempt + 1
                    );
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
    Err(anyhow::anyhow!(
        "Timed out waiting for Dune query after {} attempts",
        MAX_POLL_ATTEMPTS
    ))
}

async fn fetch_results(
    client: &reqwest::Client,
    api_key: &str,
    execution_id: &str,
) -> anyhow::Result<Vec<TradeRow>> {
    let resp: ResultsResponse = client
        .get(format!(
            "{DUNE_API_BASE}/execution/{execution_id}/results"
        ))
        .header("X-Dune-Api-Key", api_key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.result.rows)
}

fn trades_to_requests(trades: Vec<TradeRow>) -> Vec<RequestEntry> {
    trades
        .into_iter()
        .map(|t| RequestEntry {
            orders: vec![Order {
                id: String::new(),
                token_in: t.token_in,
                token_out: t.token_out,
                amount: t.amount,
                side: "sell".to_string(),
                sender: DEFAULT_SENDER.to_string(),
            }],
        })
        .collect()
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let api_key = std::env::var("DUNE_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "DUNE_API_KEY not set. Get one at https://dune.com/settings/api"
        )
    })?;

    let sql = build_sql(&args.chain, args.limit, args.min_usd, args.hours);

    println!("Chain: {}", args.chain);
    println!(
        "Window: last {}h, min ${}, limit {}",
        args.hours, args.min_usd, args.limit
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    println!("Submitting query to Dune...");
    let execution_id = execute_sql(&client, &api_key, &sql).await?;
    println!("Execution ID: {execution_id}");

    println!("Waiting for results...");
    poll_until_done(&client, &api_key, &execution_id).await?;

    println!("Downloading results...");
    let trades = fetch_results(&client, &api_key, &execution_id).await?;
    let count = trades.len();

    if count < args.limit {
        eprintln!(
            "WARNING: Only {count} trades found (requested {})",
            args.limit
        );
    }

    let requests = trades_to_requests(trades);
    let json = serde_json::to_string_pretty(&requests)?;
    std::fs::write(&args.output, &json)?;

    println!("Saved {count} trades to {}", args.output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sql_default_params() {
        let sql = build_sql("ethereum", 1000, 100.0, 24);
        assert!(sql.contains("blockchain = 'ethereum'"));
        assert!(sql.contains("INTERVAL '24' HOUR"));
        assert!(sql.contains("amount_usd >= 100"));
        assert!(sql.contains("LIMIT 1000"));
        assert!(sql.contains("token_sold_amount_raw"));
    }

    #[test]
    fn build_sql_custom_params() {
        let sql = build_sql("base", 500, 50.0, 48);
        assert!(sql.contains("blockchain = 'base'"));
        assert!(sql.contains("INTERVAL '48' HOUR"));
        assert!(sql.contains("amount_usd >= 50"));
        assert!(sql.contains("LIMIT 500"));
    }

    #[test]
    fn trades_to_requests_conversion() {
        let trades = vec![
            TradeRow {
                token_in: "0xaaa".to_string(),
                token_out: "0xbbb".to_string(),
                amount: "1000".to_string(),
            },
            TradeRow {
                token_in: "0xccc".to_string(),
                token_out: "0xddd".to_string(),
                amount: "2000".to_string(),
            },
        ];
        let requests = trades_to_requests(trades);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].orders.len(), 1);
        assert_eq!(requests[0].orders[0].token_in, "0xaaa");
        assert_eq!(requests[0].orders[0].side, "sell");
        assert_eq!(requests[0].orders[0].sender, DEFAULT_SENDER);
        assert_eq!(requests[1].orders[0].amount, "2000");
    }

    #[test]
    fn trades_to_requests_empty() {
        let requests = trades_to_requests(vec![]);
        assert!(requests.is_empty());
    }
}
