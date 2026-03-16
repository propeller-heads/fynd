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

#[derive(Deserialize)]
struct PairsFile {
    tokens: Vec<Token>,
    pairs: Vec<Pair>,
}

#[derive(Deserialize)]
struct Token {
    symbol: String,
    address: String,
    decimals: u8,
}

#[derive(Deserialize)]
struct Pair {
    token_in: String,
    token_out: String,
    amounts: Vec<f64>,
}

fn load_pairs_file() -> PairsFile {
    serde_json::from_str(PAIRS_JSON).unwrap_or_else(|e| panic!("failed to parse pairs.json: {e}"))
}

fn human_to_raw(amount: f64, decimals: u8) -> String {
    let raw = amount * 10f64.powi(i32::from(decimals));
    format!("{:.0}", raw)
}

fn symbol_for_address(addr: &str, tokens: &[Token]) -> String {
    tokens
        .iter()
        .find(|t| t.address.eq_ignore_ascii_case(addr))
        .map(|t| t.symbol.clone())
        .unwrap_or_else(|| addr[..10.min(addr.len())].to_string())
}

/// Build `n` random requests by sampling pairs and amounts from `pairs.json`.
pub fn generate_requests(n: usize, timeout_ms: u64) -> Vec<SwapRequest> {
    let file = load_pairs_file();
    (0..n)
        .map(|_| {
            let pair = &file.pairs[fastrand::usize(..file.pairs.len())];
            let amount = pair.amounts[fastrand::usize(..pair.amounts.len())];
            let token_in = file
                .tokens
                .iter()
                .find(|t| t.symbol == pair.token_in)
                .unwrap_or_else(|| panic!("unknown token: {}", pair.token_in));
            let token_out = file
                .tokens
                .iter()
                .find(|t| t.symbol == pair.token_out)
                .unwrap_or_else(|| panic!("unknown token: {}", pair.token_out));
            let raw = human_to_raw(amount, token_in.decimals);

            SwapRequest {
                label: format!("{amount} {} -> {}", token_in.symbol, token_out.symbol),
                token_in_addr: token_in.address.clone(),
                token_out_addr: token_out.address.clone(),
                raw_amount: raw,
                sender_addr: SENDER.to_string(),
                timeout_ms,
            }
        })
        .collect()
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
    let requests = templates
        .iter()
        .map(|tmpl| {
            let order = &tmpl.orders[0];
            let in_sym = symbol_for_address(&order.token_in, &file.tokens);
            let out_sym = symbol_for_address(&order.token_out, &file.tokens);

            SwapRequest {
                label: format!("{} {in_sym} -> {out_sym}", order.amount),
                token_in_addr: order.token_in.clone(),
                token_out_addr: order.token_out.clone(),
                raw_amount: order.amount.clone(),
                sender_addr: order.sender.clone(),
                timeout_ms,
            }
        })
        .collect();

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
