use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, RwLock},
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

const WS_URL: &str = "wss://stream.binance.com:9443/ws";
const EXCHANGE_INFO_URL: &str = "https://api.binance.com/api/v3/exchangeInfo";
const RESYNC_INTERVAL: Duration = Duration::from_secs(120);
const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_HISTORY_ENTRIES: usize = 3600;

#[derive(Debug, Clone)]
struct PriceEntry {
    mid_price: f64,
    timestamp_ms: u64,
}

#[derive(Debug, Clone)]
struct SymbolHistory {
    entries: VecDeque<PriceEntry>,
}

impl SymbolHistory {
    fn new() -> Self {
        Self { entries: VecDeque::with_capacity(MAX_HISTORY_ENTRIES) }
    }

    fn push(&mut self, mid_price: f64, timestamp_ms: u64) {
        if self.entries.len() >= MAX_HISTORY_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(PriceEntry { mid_price, timestamp_ms });
    }

    fn latest_mid(&self) -> Option<f64> {
        self.entries.back().map(|e| e.mid_price)
    }

    fn latest_timestamp_ms(&self) -> Option<u64> {
        self.entries.back().map(|e| e.timestamp_ms)
    }

    fn realized_vol_bps(&self, window_ms: u64) -> Option<f64> {
        if self.entries.len() < 2 {
            return None;
        }

        let cutoff = self.entries.back()?.timestamp_ms.saturating_sub(window_ms);
        let returns: Vec<f64> = self
            .entries
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[1].timestamp_ms >= cutoff)
            .filter_map(|w| {
                if w[0].mid_price > 0.0 {
                    Some((w[1].mid_price / w[0].mid_price).ln())
                } else {
                    None
                }
            })
            .collect();

        if returns.len() < 2 {
            return None;
        }

        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance =
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
        Some(variance.sqrt() * 10_000.0)
    }
}

type HistoryCache = Arc<RwLock<HashMap<String, SymbolHistory>>>;

/// Thread-safe handle for querying CEX price data.
///
/// Passed to the Tycho resim task so it can read CEX prices at
/// quote-processing time.
#[derive(Clone)]
pub struct CexDynamicsHandle {
    history: HistoryCache,
}

impl CexDynamicsHandle {
    pub fn mid_price(&self, symbol: &str) -> Option<f64> {
        let cache = self.history.read().ok()?;
        cache.get(symbol)?.latest_mid()
    }

    pub fn mid_price_timestamp_ms(&self, symbol: &str) -> Option<u64> {
        let cache = self.history.read().ok()?;
        cache.get(symbol)?.latest_timestamp_ms()
    }

    /// Rolling realized volatility in bps over a time window.
    pub fn realized_vol_bps(&self, symbol: &str, window_ms: u64) -> Option<f64> {
        let cache = self.history.read().ok()?;
        cache.get(symbol)?.realized_vol_bps(window_ms)
    }

    /// CEX-DEX spread in bps: `(cex_mid - dex_spot) / cex_mid * 10_000`.
    /// Positive means DEX price lags behind CEX (CEX is ahead).
    pub fn cex_dex_spread_bps(&self, symbol: &str, dex_spot_price: f64) -> Option<f64> {
        let cex_mid = self.mid_price(symbol)?;
        if cex_mid <= 0.0 {
            return None;
        }
        Some((cex_mid - dex_spot_price) / cex_mid * 10_000.0)
    }

    /// Resolve token symbols to a Binance pair symbol.
    /// Tries BASEUSDT, BASEUSD, etc.
    pub fn resolve_pair_symbol(
        &self,
        token_in_symbol: &str,
        token_out_symbol: &str,
    ) -> Option<String> {
        let cache = self.history.read().ok()?;
        let norm_in = normalize_symbol(token_in_symbol);
        let norm_out = normalize_symbol(token_out_symbol);

        for candidate in [
            format!("{}{}", norm_in, norm_out),
            format!("{}USDT", norm_in),
            format!("{}USDC", norm_in),
        ] {
            if cache.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

fn normalize_symbol(sym: &str) -> String {
    match sym.to_uppercase().as_str() {
        "WETH" => "ETH".to_string(),
        "WBTC" => "BTC".to_string(),
        "WBNB" => "BNB".to_string(),
        "WAVAX" => "AVAX".to_string(),
        "WMATIC" | "MATIC" => "POL".to_string(),
        other => other.to_string(),
    }
}

/// Starts the CEX dynamics background task. Returns a handle for reading
/// prices and a `JoinHandle` for the background task.
pub fn start_cex_dynamics() -> (CexDynamicsHandle, tokio::task::JoinHandle<()>) {
    let history: HistoryCache = Arc::new(RwLock::new(HashMap::new()));
    let handle = CexDynamicsHandle { history: Arc::clone(&history) };

    let join = tokio::spawn(async move {
        run_binance_ws(history).await;
    });

    (handle, join)
}

async fn run_binance_ws(history: HistoryCache) {
    let client = reqwest::Client::new();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        match connect_and_stream(&client, &history).await {
            Ok(()) => {
                info!("CEX dynamics WS disconnected, reconnecting");
                backoff = INITIAL_BACKOFF;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    backoff_secs = backoff.as_secs(),
                    "CEX dynamics WS error"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_and_stream(
    client: &reqwest::Client,
    history: &HistoryCache,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let binance_symbols = fetch_binance_symbols(client).await?;
    let pairs: Vec<String> = binance_symbols
        .iter()
        .filter(|s| {
            s.ends_with("USDT") || s.ends_with("USDC") || s.ends_with("ETH") || s.ends_with("BTC")
        })
        .cloned()
        .collect();

    if pairs.is_empty() {
        return Err("no Binance pairs found".into());
    }

    let (ws_stream, _) = connect_async(WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let streams: Vec<String> = pairs
        .iter()
        .map(|s| format!("{}@bookTicker", s.to_lowercase()))
        .collect();

    let subscribe_msg = serde_json::json!({
        "method": "SUBSCRIBE",
        "params": streams,
        "id": 1
    });
    write
        .send(Message::Text(subscribe_msg.to_string().into()))
        .await?;

    info!(pair_count = pairs.len(), "CEX dynamics: subscribed to bookTicker streams");

    let mut resync = tokio::time::interval(RESYNC_INTERVAL);
    resync.tick().await;

    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(msg) = msg else { return Ok(()); };
                if let Message::Text(text) = msg? {
                    handle_ticker(&text, history);
                }
            }
            _ = resync.tick() => {
                debug!("CEX dynamics: resync tick (no-op for now)");
            }
        }
    }
}

fn handle_ticker(text: &str, history: &HistoryCache) {
    #[derive(serde::Deserialize)]
    struct BookTicker {
        s: Option<String>,
        b: Option<String>,
        a: Option<String>,
        #[serde(alias = "E")]
        event_time: Option<u64>,
    }

    let Ok(ticker) = serde_json::from_str::<BookTicker>(text) else {
        return;
    };
    let Some(symbol) = ticker.s else { return };
    let bid = ticker.b.and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let ask = ticker.a.and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    if bid <= 0.0 || ask <= 0.0 {
        return;
    }

    let mid = (bid + ask) / 2.0;
    let ts = ticker.event_time.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });

    let Ok(mut cache) = history.write() else {
        return;
    };
    cache
        .entry(symbol)
        .or_insert_with(SymbolHistory::new)
        .push(mid, ts);
}

async fn fetch_binance_symbols(
    client: &reqwest::Client,
) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
    #[derive(serde::Deserialize)]
    struct ExchangeInfo {
        symbols: Vec<SymbolInfo>,
    }
    #[derive(serde::Deserialize)]
    struct SymbolInfo {
        symbol: String,
        status: String,
    }

    let resp: ExchangeInfo = client.get(EXCHANGE_INFO_URL).send().await?.json().await?;
    let symbols: HashSet<String> = resp
        .symbols
        .into_iter()
        .filter(|s| s.status == "TRADING")
        .map(|s| s.symbol)
        .collect();
    debug!(count = symbols.len(), "CEX dynamics: fetched Binance symbols");
    Ok(symbols)
}

pub const FIVE_MIN_MS: u64 = 5 * 60 * 1000;
pub const FIFTEEN_MIN_MS: u64 = 15 * 60 * 1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realized_vol_empty_history() {
        let h = SymbolHistory::new();
        assert!(h.realized_vol_bps(FIVE_MIN_MS).is_none());
    }

    #[test]
    fn realized_vol_single_entry() {
        let mut h = SymbolHistory::new();
        h.push(100.0, 1000);
        assert!(h.realized_vol_bps(FIVE_MIN_MS).is_none());
    }

    #[test]
    fn realized_vol_stable_price() {
        let mut h = SymbolHistory::new();
        for i in 0..100 {
            h.push(2000.0, i * 1000);
        }
        let vol = h.realized_vol_bps(FIVE_MIN_MS).expect("has vol");
        assert!(vol.abs() < 0.01, "stable price should have ~0 vol, got {vol}");
    }

    #[test]
    fn realized_vol_volatile_price() {
        let mut h = SymbolHistory::new();
        for i in 0..100 {
            let price = if i % 2 == 0 { 2000.0 } else { 2010.0 };
            h.push(price, i * 1000);
        }
        let vol = h.realized_vol_bps(FIVE_MIN_MS).expect("has vol");
        assert!(vol > 0.0, "alternating price should have positive vol");
    }

    #[test]
    fn history_caps_at_max() {
        let mut h = SymbolHistory::new();
        for i in 0..(MAX_HISTORY_ENTRIES + 100) {
            h.push(2000.0 + i as f64, i as u64 * 1000);
        }
        assert_eq!(h.entries.len(), MAX_HISTORY_ENTRIES);
    }

    #[test]
    fn normalize_wrapped_tokens() {
        assert_eq!(normalize_symbol("WETH"), "ETH");
        assert_eq!(normalize_symbol("WBTC"), "BTC");
        assert_eq!(normalize_symbol("UNI"), "UNI");
    }

    #[test]
    fn cex_dex_spread_positive_when_cex_ahead() {
        let history: HistoryCache = Arc::new(RwLock::new(HashMap::new()));
        {
            let mut cache = history.write().unwrap();
            let mut h = SymbolHistory::new();
            h.push(2010.0, 1000);
            cache.insert("ETHUSDT".to_string(), h);
        }
        let handle = CexDynamicsHandle { history };
        let spread = handle.cex_dex_spread_bps("ETHUSDT", 2000.0).unwrap();
        assert!(spread > 0.0, "CEX ahead of DEX should give positive spread");
        let expected = (2010.0 - 2000.0) / 2010.0 * 10_000.0;
        assert!((spread - expected).abs() < 0.1);
    }
}
