//! Request generation and loading.
//!
//! Provides a default WETH→USDC swap, random generation from an embedded
//! token-pair set (`pairs.json`), and loading from user-supplied JSON files.

use std::str::FromStr;

use alloy::hex;
use bytes::Bytes;
use fynd_client::{Order, OrderSide, QuoteOptions, QuoteParams};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct SwapRequest {
    pub label: String,
    token_in_addr: String,
    token_out_addr: String,
    raw_amount: String,
    sender_addr: String,
    timeout_ms: u64,
}

/// Default 1 WETH → USDC request, used when no `--requests-file` is given.
pub fn default_request(timeout_ms: u64) -> SwapRequest {
    SwapRequest {
        label: "1 WETH -> USDC".to_string(),
        token_in_addr: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
        token_out_addr: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
        raw_amount: "1000000000000000000".to_string(),
        sender_addr: SENDER.to_string(),
        timeout_ms,
    }
}

impl SwapRequest {
    pub fn token_in_addr(&self) -> &str {
        &self.token_in_addr
    }

    pub fn token_out_addr(&self) -> &str {
        &self.token_out_addr
    }

    pub fn to_quote_params(&self) -> QuoteParams {
        let token_in = parse_address(&self.token_in_addr);
        let token_out = parse_address(&self.token_out_addr);
        let sender = parse_address(&self.sender_addr);
        let amount = BigUint::from_str(&self.raw_amount)
            .unwrap_or_else(|e| panic!("bad amount '{}': {e}", self.raw_amount));
        let order = Order::new(token_in, token_out, amount, OrderSide::Sell, sender, None);
        let options = QuoteOptions::default().with_timeout_ms(self.timeout_ms);
        QuoteParams::new(order, options)
    }
}

fn parse_address(hex_str: &str) -> Bytes {
    let stripped = hex_str
        .strip_prefix("0x")
        .unwrap_or(hex_str);
    Bytes::from(hex::decode(stripped).unwrap_or_else(|e| panic!("bad address '{hex_str}': {e}")))
}

const SENDER: &str = "0x0000000000000000000000000000000000000001";

const PAIRS_JSON: &str = include_str!("pairs.json");

/// Small embedded sample of real aggregator trades (50 trades).
/// For the full 10k set, use `download-trades` to fetch from GitHub Releases.
const TRADES_SAMPLE_JSON: &str = include_str!("trades_sample.json");

/// URL of the full 10k aggregator trades dataset hosted on GitHub Releases.
pub const TRADES_DOWNLOAD_URL: &str =
    "https://github.com/propeller-heads/fynd/releases/download/benchmark-data-v1/aggregator_trades_10k.json";

#[derive(Deserialize)]
struct PairsFile {
    tokens: Vec<Token>,
}

#[derive(Deserialize)]
struct Token {
    symbol: String,
    address: String,
}

fn load_pairs_file() -> PairsFile {
    serde_json::from_str(PAIRS_JSON).unwrap_or_else(|e| panic!("failed to parse pairs.json: {e}"))
}

fn symbol_for_address(addr: &str, tokens: &[Token]) -> String {
    tokens
        .iter()
        .find(|t| t.address.eq_ignore_ascii_case(addr))
        .map(|t| t.symbol.clone())
        .unwrap_or_else(|| addr[..10.min(addr.len())].to_string())
}

#[derive(Deserialize)]
struct FileRequest {
    orders: Vec<FileOrder>,
}

#[derive(Deserialize)]
struct FileOrder {
    token_in: String,
    token_out: String,
    amount: String,
    #[serde(default = "default_sender")]
    sender: String,
}

fn default_sender() -> String {
    SENDER.to_string()
}

/// Load all request templates from a JSON file (one `SwapRequest` per entry).
pub fn load_request_templates(
    path: &str,
    timeout_ms: u64,
) -> Result<Vec<SwapRequest>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let templates: Vec<FileRequest> = serde_json::from_str(&content)?;
    if templates.is_empty() {
        return Err("requests file contains no requests".into());
    }

    let file = load_pairs_file();
    let requests: Vec<SwapRequest> = templates
        .iter()
        .enumerate()
        .map(|(i, tmpl)| {
            let order = tmpl
                .orders
                .first()
                .ok_or_else(|| -> Box<dyn std::error::Error> {
                    format!("request template at index {i} has no orders").into()
                })?;
            let in_sym = symbol_for_address(&order.token_in, &file.tokens);
            let out_sym = symbol_for_address(&order.token_out, &file.tokens);

            Ok::<_, Box<dyn std::error::Error>>(SwapRequest {
                label: format!("{} {in_sym} -> {out_sym}", order.amount),
                token_in_addr: order.token_in.clone(),
                token_out_addr: order.token_out.clone(),
                raw_amount: order.amount.clone(),
                sender_addr: order.sender.clone(),
                timeout_ms,
            })
        })
        .collect::<Result<_, _>>()?;

    Ok(requests)
}

/// Load templates from a JSON file and randomly sample `n` requests from them.
pub fn load_requests_from_file(
    path: &str,
    n: usize,
    timeout_ms: u64,
) -> Result<Vec<SwapRequest>, Box<dyn std::error::Error>> {
    let templates = load_request_templates(path, timeout_ms)?;

    let requests = (0..n)
        .map(|_| templates[fastrand::usize(..templates.len())].clone())
        .collect();

    Ok(requests)
}

/// Load the embedded 50-trade aggregator sample.
pub fn load_embedded_trades(
    n: usize,
    timeout_ms: u64,
) -> Result<Vec<SwapRequest>, Box<dyn std::error::Error>> {
    let templates: Vec<FileRequest> = serde_json::from_str(TRADES_SAMPLE_JSON)?;
    let file = load_pairs_file();
    let all: Vec<SwapRequest> = templates
        .iter()
        .enumerate()
        .map(|(i, tmpl)| {
            let order = tmpl
                .orders
                .first()
                .ok_or_else(|| -> Box<dyn std::error::Error> {
                    format!("embedded trade at index {i} has no orders").into()
                })?;
            let in_sym = symbol_for_address(&order.token_in, &file.tokens);
            let out_sym = symbol_for_address(&order.token_out, &file.tokens);
            Ok::<_, Box<dyn std::error::Error>>(SwapRequest {
                label: format!("{} {in_sym} -> {out_sym}", order.amount),
                token_in_addr: order.token_in.clone(),
                token_out_addr: order.token_out.clone(),
                raw_amount: order.amount.clone(),
                sender_addr: order.sender.clone(),
                timeout_ms,
            })
        })
        .collect::<Result<_, _>>()?;

    let requests = (0..n)
        .map(|_| all[fastrand::usize(..all.len())].clone())
        .collect();
    Ok(requests)
}

/// Download the full 10k aggregator trade dataset from GitHub Releases.
pub async fn download_trades(output_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("Downloading aggregator trades from {TRADES_DOWNLOAD_URL}...");
    let client = reqwest::Client::new();
    let resp = client
        .get(TRADES_DOWNLOAD_URL)
        .header("Accept", "application/octet-stream")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!(
            "download failed: HTTP {} ({})",
            resp.status(),
            resp.status()
                .canonical_reason()
                .unwrap_or("unknown")
        )
        .into());
    }
    let bytes = resp.bytes().await?;
    std::fs::write(output_path, &bytes)?;
    println!("Saved {} bytes to {output_path}", bytes.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_with_prefix() {
        let addr = parse_address("0x0000000000000000000000000000000000000001");
        assert_eq!(addr.len(), 20);
        assert_eq!(addr[19], 1);
    }

    #[test]
    fn parse_address_without_prefix() {
        let with = parse_address("0x0000000000000000000000000000000000000001");
        let without = parse_address("0000000000000000000000000000000000000001");
        assert_eq!(with, without);
    }

    #[test]
    fn parse_address_zero() {
        let addr = parse_address("0x0000000000000000000000000000000000000000");
        assert_eq!(addr.len(), 20);
        assert!(addr.iter().all(|&b| b == 0));
    }

    #[test]
    fn default_request_fields() {
        let req = default_request(5000);
        assert!(req.label.contains("WETH"));
        assert!(req.label.contains("USDC"));
        assert_eq!(req.raw_amount, "1000000000000000000");
        assert_eq!(req.timeout_ms, 5000);
        assert!(req.token_in_addr.starts_with("0x"));
        assert!(req.token_out_addr.starts_with("0x"));
    }

    #[test]
    fn symbol_for_known_address() {
        let tokens = vec![Token {
            symbol: "WETH".to_string(),
            address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
        }];
        assert_eq!(
            symbol_for_address("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", &tokens),
            "WETH"
        );
    }

    #[test]
    fn symbol_for_unknown_address() {
        let tokens: Vec<Token> = vec![];
        let result = symbol_for_address("0xdeadbeef01", &tokens);
        assert_eq!(result, "0xdeadbeef");
    }

    #[test]
    fn symbol_for_address_case_insensitive() {
        let tokens = vec![Token {
            symbol: "WETH".to_string(),
            address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
        }];
        assert_eq!(
            symbol_for_address("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", &tokens),
            "WETH"
        );
    }

    #[test]
    fn load_pairs_file_parses() {
        let file = load_pairs_file();
        assert!(!file.tokens.is_empty());
    }

    #[test]
    fn load_embedded_trades_returns_correct_count() {
        let reqs = load_embedded_trades(10, 5000).unwrap();
        assert_eq!(reqs.len(), 10);
        for r in &reqs {
            assert_eq!(r.timeout_ms, 5000);
            assert!(r.token_in_addr.starts_with("0x"));
            assert!(r.token_out_addr.starts_with("0x"));
        }
    }

    #[test]
    fn load_embedded_trades_seeded_determinism() {
        fastrand::seed(99);
        let a: Vec<String> = load_embedded_trades(5, 1000)
            .unwrap()
            .iter()
            .map(|r| r.label.clone())
            .collect();
        fastrand::seed(99);
        let b: Vec<String> = load_embedded_trades(5, 1000)
            .unwrap()
            .iter()
            .map(|r| r.label.clone())
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn load_request_templates_valid_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_templates_valid.json");
        let content = serde_json::json!([{
            "orders": [{
                "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "amount": "1000000000000000000"
            }]
        }]);
        std::fs::write(&path, content.to_string()).unwrap();
        let result = load_request_templates(path.to_str().unwrap(), 5000);
        std::fs::remove_file(&path).ok();
        let templates = result.unwrap();
        assert_eq!(templates.len(), 1);
    }

    #[test]
    fn load_request_templates_empty_array() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_templates_empty.json");
        std::fs::write(&path, "[]").unwrap();
        let result = load_request_templates(path.to_str().unwrap(), 5000);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no requests"));
    }

    #[test]
    fn load_request_templates_empty_orders() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_templates_empty_orders.json");
        let content = serde_json::json!([{ "orders": [] }]);
        std::fs::write(&path, content.to_string()).unwrap();
        let result = load_request_templates(path.to_str().unwrap(), 5000);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no orders"));
    }

    #[test]
    fn load_request_templates_nonexistent() {
        assert!(load_request_templates("/nonexistent/path.json", 5000).is_err());
    }

    #[test]
    fn load_requests_from_file_samples_with_replacement() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_templates_sample.json");
        let content = serde_json::json!([{
            "orders": [{
                "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "amount": "1000000000000000000"
            }]
        }]);
        std::fs::write(&path, content.to_string()).unwrap();
        let result = load_requests_from_file(path.to_str().unwrap(), 10, 5000);
        std::fs::remove_file(&path).ok();
        assert_eq!(result.unwrap().len(), 10);
    }
}
