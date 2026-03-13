// Shared by both benchmark and compare examples; each uses a different subset.
#![allow(dead_code)]

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

/// Default WETH → USDC request used by the benchmark when no file is provided.
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

struct Token {
    symbol: &'static str,
    address: &'static str,
    decimals: u8,
}

struct Pair {
    token_in: &'static str,
    token_out: &'static str,
    amounts: &'static [f64],
}

const SENDER: &str = "0x0000000000000000000000000000000000000001";

const TOKENS: &[Token] = &[
    Token { symbol: "WETH", address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", decimals: 18 },
    Token { symbol: "USDC", address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", decimals: 6 },
    Token { symbol: "DAI", address: "0x6B175474E89094C44Da98b954EedeAC495271d0F", decimals: 18 },
    Token { symbol: "USDT", address: "0xdAC17F958D2ee523a2206206994597C13D831ec7", decimals: 6 },
    Token { symbol: "WBTC", address: "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599", decimals: 8 },
    Token { symbol: "PEPE", address: "0x6982508145454Ce325dDbE47a25d4ec3d2311933", decimals: 18 },
    Token { symbol: "BNB", address: "0xB8c77482e45F1F44dE1745F52C74426C631bDD52", decimals: 18 },
    Token { symbol: "LINK", address: "0x514910771AF9Ca656af840dff83E8264EcF986CA", decimals: 18 },
    Token { symbol: "SHIB", address: "0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE", decimals: 18 },
    Token { symbol: "UNI", address: "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984", decimals: 18 },
    Token { symbol: "AAVE", address: "0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9", decimals: 18 },
    Token { symbol: "GNO", address: "0x6810e776880C02933D47DB1b9fc05908e5386b96", decimals: 18 },
];

// Pairs and sell amounts (in human units, each >= ~$1M)
// Approximate prices: ETH~$2K, BTC~$65K, LINK~$15, UNI~$7,
// AAVE~$200, GNO~$300, BNB~$600, PEPE~$0.000008, SHIB~$0.00001
const PAIRS: &[Pair] = &[
    Pair { token_in: "WETH", token_out: "USDC", amounts: &[500.0, 1000.0, 2500.0, 5000.0] },
    Pair { token_in: "USDC", token_out: "WETH", amounts: &[1e6, 2.5e6, 5e6, 10e6] },
    Pair { token_in: "WETH", token_out: "DAI", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "DAI", token_out: "WETH", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "USDC", token_out: "DAI", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "DAI", token_out: "USDC", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "WETH", token_out: "USDT", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "USDT", token_out: "WETH", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "WETH", token_out: "WBTC", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "WBTC", token_out: "WETH", amounts: &[15.0, 25.0, 50.0, 100.0] },
    Pair { token_in: "USDC", token_out: "USDT", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "USDT", token_out: "USDC", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "WETH", token_out: "PEPE", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "PEPE", token_out: "WETH", amounts: &[125e9, 500e9, 1e12] },
    Pair { token_in: "WETH", token_out: "LINK", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "LINK", token_out: "WETH", amounts: &[70_000.0, 150_000.0, 350_000.0] },
    Pair { token_in: "WETH", token_out: "SHIB", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "SHIB", token_out: "WETH", amounts: &[100e9, 500e9, 1e12] },
    Pair { token_in: "WETH", token_out: "UNI", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "UNI", token_out: "WETH", amounts: &[150_000.0, 350_000.0, 700_000.0] },
    Pair { token_in: "WETH", token_out: "AAVE", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "AAVE", token_out: "WETH", amounts: &[5_000.0, 10_000.0, 25_000.0] },
    Pair { token_in: "WETH", token_out: "GNO", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "GNO", token_out: "WETH", amounts: &[3_500.0, 7_000.0, 15_000.0] },
    Pair { token_in: "WETH", token_out: "BNB", amounts: &[500.0, 1000.0, 2500.0] },
    Pair { token_in: "BNB", token_out: "WETH", amounts: &[1_700.0, 3_500.0, 7_000.0] },
    Pair { token_in: "USDC", token_out: "LINK", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "USDC", token_out: "UNI", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "USDC", token_out: "AAVE", amounts: &[1e6, 2.5e6, 5e6] },
    Pair { token_in: "LINK", token_out: "USDC", amounts: &[70_000.0, 150_000.0, 350_000.0] },
    Pair { token_in: "PEPE", token_out: "USDC", amounts: &[125e9, 500e9, 1e12] },
    // Lesser-known to lesser-known (multi-hop, harder routes)
    Pair { token_in: "LINK", token_out: "UNI", amounts: &[70_000.0, 150_000.0, 350_000.0] },
    Pair { token_in: "UNI", token_out: "LINK", amounts: &[150_000.0, 350_000.0, 700_000.0] },
    Pair { token_in: "AAVE", token_out: "GNO", amounts: &[5_000.0, 10_000.0, 25_000.0] },
    Pair { token_in: "GNO", token_out: "AAVE", amounts: &[3_500.0, 7_000.0, 15_000.0] },
    Pair { token_in: "PEPE", token_out: "SHIB", amounts: &[125e9, 500e9, 1e12] },
    Pair { token_in: "SHIB", token_out: "PEPE", amounts: &[100e9, 500e9, 1e12] },
    Pair { token_in: "LINK", token_out: "AAVE", amounts: &[70_000.0, 150_000.0, 350_000.0] },
    Pair { token_in: "AAVE", token_out: "LINK", amounts: &[5_000.0, 10_000.0, 25_000.0] },
    Pair { token_in: "UNI", token_out: "GNO", amounts: &[150_000.0, 350_000.0, 700_000.0] },
    Pair { token_in: "GNO", token_out: "UNI", amounts: &[3_500.0, 7_000.0, 15_000.0] },
    Pair { token_in: "PEPE", token_out: "LINK", amounts: &[125e9, 500e9, 1e12] },
    Pair { token_in: "BNB", token_out: "AAVE", amounts: &[1_700.0, 3_500.0, 7_000.0] },
    Pair { token_in: "SHIB", token_out: "UNI", amounts: &[100e9, 500e9, 1e12] },
];

fn find_token(symbol: &str) -> &'static Token {
    TOKENS
        .iter()
        .find(|t| t.symbol == symbol)
        .unwrap_or_else(|| panic!("unknown token: {symbol}"))
}

fn human_to_raw(amount: f64, decimals: u8) -> String {
    let raw = amount * 10f64.powi(i32::from(decimals));
    format!("{:.0}", raw)
}

fn symbol_for_address(addr: &str) -> &str {
    TOKENS
        .iter()
        .find(|t| t.address.eq_ignore_ascii_case(addr))
        .map(|t| t.symbol)
        .unwrap_or(&addr[..10.min(addr.len())])
}

pub fn generate_requests(n: usize, timeout_ms: u64) -> Vec<SwapRequest> {
    (0..n)
        .map(|_| {
            let pair = &PAIRS[fastrand::usize(..PAIRS.len())];
            let amount = pair.amounts[fastrand::usize(..pair.amounts.len())];
            let token_in = find_token(pair.token_in);
            let token_out = find_token(pair.token_out);
            let raw = human_to_raw(amount, token_in.decimals);

            SwapRequest {
                label: format!("{amount} {} -> {}", token_in.symbol, token_out.symbol),
                token_in_addr: token_in.address.to_string(),
                token_out_addr: token_out.address.to_string(),
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

/// Load all request templates from a JSON file.
///
/// Returns one `SwapRequest` per entry in the file. Use this when you need the
/// full template pool (e.g. benchmark randomly samples from them during the run).
pub fn load_request_templates(
    path: &str,
    timeout_ms: u64,
) -> Result<Vec<SwapRequest>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let templates: Vec<FileRequest> = serde_json::from_str(&content)?;
    if templates.is_empty() {
        return Err("requests file contains no requests".into());
    }

    let requests = templates
        .iter()
        .map(|tmpl| {
            let order = &tmpl.orders[0];
            let in_sym = symbol_for_address(&order.token_in);
            let out_sym = symbol_for_address(&order.token_out);

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

/// Load requests from a JSON file and randomly sample `n` from them.
///
/// Use this when you need a fixed number of pre-generated requests
/// (e.g. compare sends the same N requests to both solvers).
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
