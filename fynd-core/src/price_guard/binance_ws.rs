//! Binance WebSocket price provider.
//!
//! Connects to the Binance `bookTicker` WebSocket stream, dynamically discovers trading pairs
//! from [`SharedMarketData`](crate::feed::market_data::SharedMarketData)(crate::feed::market_data::SharedMarketData), and caches real-time
//! bid/ask prices. Price resolution supports direct pairs, reverse pairs, and intermediate
//! routing through common quote assets (USDT, USDC, ETH, BTC).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{SinkExt, StreamExt};
use num_bigint::BigUint;
use reqwest::Client;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};
use tycho_simulation::tycho_common::models::{token::Token, Address};

use super::{
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
    utils::{check_staleness, expected_out_from_price},
};
use crate::feed::market_data::SharedMarketDataRef;

const DEFAULT_WS_URL: &str = "wss://stream.binance.com:9443/ws";
const DEFAULT_EXCHANGE_INFO_URL: &str = "https://api.binance.com/api/v3/exchangeInfo";

/// Quote assets used for intermediate price routing.
const INTERMEDIATE_ASSETS: &[&str] = &["USDT", "USDC", "ETH", "BTC"];

/// USD-pegged stablecoins. Loaded from `stable_usd.json` — shared across providers.
static USD_STABLECOINS: LazyLock<HashSet<String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("stable_usd.json")).expect("stable_usd.json is valid")
});

/// How often to check for new tokens and subscribe to additional streams.
const RESYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Initial reconnect delay, doubles up to `MAX_BACKOFF`.
const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Maps on-chain token symbols to their Binance spot trading names.
///
/// Returns `(binance_symbol, price_scale)` where `price_scale` adjusts the oracle price
/// to a per-token basis. For most tokens this is `1.0`. Curated from the Binance perp
/// universe across three categories:
///
/// 1. **Wrapped native tokens** — on-chain "W"-prefixed wrappers of chain gas tokens. Tokens like
///    WLD/WIF whose names happen to start with W are NOT included.
/// 2. **1000-prefix tokens** — Binance quotes these per 1,000 tokens to avoid tiny decimals.
///    `price_scale` is `0.001` so callers get the per-token price.
/// 3. **Binance-specific**: MATIC was renamed to POL (Polygon rebrand).
fn normalize_symbol(symbol: &str) -> (String, f64) {
    match symbol.to_uppercase().as_str() {
        // Wrapped native tokens
        "WETH" => ("ETH".to_string(), 1.0),
        "WBTC" => ("BTC".to_string(), 1.0),
        "WBNB" => ("BNB".to_string(), 1.0),
        "WAVAX" => ("AVAX".to_string(), 1.0),
        // 1000-prefixed: Binance quotes per 1,000 tokens
        "CHEEMS" => ("1000CHEEMS".to_string(), 0.001),
        "SATS" => ("1000SATS".to_string(), 0.001),
        "CAT" => ("1000CAT".to_string(), 0.001),
        // Binance specific
        "WMATIC" | "MATIC" => ("POL".to_string(), 1.0),
        other => (other.to_string(), 1.0),
    }
}

/// Cached ticker data from the bookTicker stream.
#[derive(Debug, Clone)]
struct TickerData {
    bid: f64,
    ask: f64,
    timestamp_ms: u64,
}

/// Resolved price lookup result.
#[derive(Debug)]
struct PriceLookup {
    price: f64,
    timestamp_ms: u64,
}

/// Shared ticker cache. Key is the uppercase Binance symbol (e.g. "ETHUSDT").
type PriceCache = Arc<RwLock<HashMap<String, TickerData>>>;

/// Cached token metadata resolved from on-chain addresses.
type TokenCache = Arc<RwLock<HashMap<Address, Token>>>;

/// Binance WebSocket price provider.
///
/// Subscribes to `bookTicker` streams for pairs discovered by cross-referencing
/// Binance exchange info with tokens in
/// [`SharedMarketData`](crate::feed::market_data::SharedMarketData). Prices are resolved via direct
/// pair (bid), reverse pair (1/ask), or intermediate routing through USDT/USDC/ETH/BTC.
pub struct BinanceWsProvider {
    price_cache: PriceCache,
    token_cache: TokenCache,
    ws_url: String,
    exchange_info_url: String,
}

impl BinanceWsProvider {
    pub fn new() -> Self {
        Self {
            price_cache: Arc::new(RwLock::new(HashMap::new())),
            token_cache: Arc::new(RwLock::new(HashMap::new())),
            ws_url: DEFAULT_WS_URL.to_string(),
            exchange_info_url: DEFAULT_EXCHANGE_INFO_URL.to_string(),
        }
    }

    /// Overrides the Binance WebSocket and exchange info endpoint URLs.
    pub fn new_with_urls(ws_url: impl Into<String>, exchange_info_url: impl Into<String>) -> Self {
        Self {
            price_cache: Arc::new(RwLock::new(HashMap::new())),
            token_cache: Arc::new(RwLock::new(HashMap::new())),
            ws_url: ws_url.into(),
            exchange_info_url: exchange_info_url.into(),
        }
    }

    /// Resolves a token address to (normalized_symbol, decimals) from the local token cache.
    fn resolve_token(&self, address: &Address) -> Result<(String, u32, f64), PriceProviderError> {
        let cache = self
            .token_cache
            .read()
            .map_err(|e| PriceProviderError::Unavailable(format!("token cache poisoned: {e}")))?;
        let info = cache
            .get(address)
            .ok_or_else(|| PriceProviderError::TokenNotFound { address: address.to_string() })?;
        let (symbol, price_scale) = normalize_symbol(&info.symbol);
        Ok((symbol, info.decimals, price_scale))
    }

    /// Looks up a sell-side price for `base` in terms of `quote`.
    ///
    /// Direct pair: uses bid (what you get when selling).
    /// Reverse pair: uses 1/ask (what you pay when buying the reverse).
    fn lookup_sell_price(
        cache: &HashMap<String, TickerData>,
        base: &str,
        quote: &str,
    ) -> Option<PriceLookup> {
        let direct_symbol = format!("{base}{quote}");
        if let Some(ticker) = cache.get(&direct_symbol) {
            if ticker.bid > 0.0 {
                return Some(PriceLookup { price: ticker.bid, timestamp_ms: ticker.timestamp_ms });
            }
        }

        let reverse_symbol = format!("{quote}{base}");
        if let Some(ticker) = cache.get(&reverse_symbol) {
            if ticker.ask > 0.0 {
                return Some(PriceLookup {
                    price: 1.0 / ticker.ask,
                    timestamp_ms: ticker.timestamp_ms,
                });
            }
        }

        None
    }

    /// Looks up a buy-side price for `base` in terms of `quote`.
    ///
    /// Direct pair: uses ask (what you pay when buying).
    /// Reverse pair: uses 1/bid (what you get when selling the reverse).
    fn lookup_buy_price(
        cache: &HashMap<String, TickerData>,
        base: &str,
        quote: &str,
    ) -> Option<PriceLookup> {
        let direct_symbol = format!("{base}{quote}");
        if let Some(ticker) = cache.get(&direct_symbol) {
            if ticker.ask > 0.0 {
                return Some(PriceLookup { price: ticker.ask, timestamp_ms: ticker.timestamp_ms });
            }
        }

        let reverse_symbol = format!("{quote}{base}");
        if let Some(ticker) = cache.get(&reverse_symbol) {
            if ticker.bid > 0.0 {
                return Some(PriceLookup {
                    price: 1.0 / ticker.bid,
                    timestamp_ms: ticker.timestamp_ms,
                });
            }
        }

        None
    }

    /// Resolves a price between two symbols, trying direct/reverse first,
    /// then routing through intermediate assets.
    ///
    /// For intermediate routing: sells `sym_in` for intermediate (bid),
    /// then buys `sym_out` with intermediate (ask), accounting for spreads.
    fn resolve_price(
        cache: &HashMap<String, TickerData>,
        sym_in: &str,
        sym_out: &str,
    ) -> Option<PriceLookup> {
        if let Some(lookup) = Self::lookup_sell_price(cache, sym_in, sym_out) {
            return Some(lookup);
        }

        for intermediate in INTERMEDIATE_ASSETS {
            if *intermediate == sym_in || *intermediate == sym_out {
                continue;
            }
            let leg_in = Self::lookup_sell_price(cache, sym_in, intermediate);
            let leg_out = Self::lookup_buy_price(cache, sym_out, intermediate);
            if let (Some(sell_price), Some(buy_price)) = (leg_in, leg_out) {
                if buy_price.price > 0.0 {
                    let price = sell_price.price / buy_price.price;
                    let ts = sell_price
                        .timestamp_ms
                        .min(buy_price.timestamp_ms);
                    return Some(PriceLookup { price, timestamp_ms: ts });
                }
            }
        }

        None
    }
}

impl Default for BinanceWsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PriceProvider for BinanceWsProvider {
    fn start(&mut self, market_data: SharedMarketDataRef) -> JoinHandle<()> {
        let worker = BinanceWsWorker {
            price_cache: Arc::clone(&self.price_cache),
            token_cache: Arc::clone(&self.token_cache),
            market_data,
            client: Client::new(),
            ws_url: self.ws_url.clone(),
            exchange_info_url: self.exchange_info_url.clone(),
            subscribed_symbols: HashSet::new(),
            next_subscription_id: 1,
        };
        tokio::spawn(async move { worker.run().await })
    }

    fn get_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Result<ExternalPrice, PriceProviderError> {
        let (sym_in, dec_in, scale_in) = self.resolve_token(token_in)?;
        let (sym_out, dec_out, scale_out) = self.resolve_token(token_out)?;

        let cache = self
            .price_cache
            .read()
            .map_err(|e| PriceProviderError::Unavailable(format!("ticker cache poisoned: {e}")))?;

        let lookup = Self::resolve_price(&cache, &sym_in, &sym_out).ok_or_else(|| {
            PriceProviderError::TokenNotFound { address: format!("{sym_in}/{sym_out}") }
        })?;

        check_staleness(lookup.timestamp_ms)?;

        let price = lookup.price * scale_in / scale_out;
        let expected_out = expected_out_from_price(amount_in, price, dec_in, dec_out);
        Ok(ExternalPrice::new(expected_out, "binance_ws".to_string(), lookup.timestamp_ms))
    }
}

/// Background worker that manages the Binance WebSocket connection lifecycle.
struct BinanceWsWorker {
    price_cache: PriceCache,
    token_cache: TokenCache,
    market_data: SharedMarketDataRef,
    client: Client,
    ws_url: String,
    exchange_info_url: String,
    subscribed_symbols: HashSet<String>,
    next_subscription_id: u64,
}

impl BinanceWsWorker {
    async fn run(mut self) {
        let mut backoff = INITIAL_BACKOFF;

        loop {
            self.refresh_token_cache().await;

            match self.connect_and_stream().await {
                Ok(()) => {
                    info!("Binance WebSocket disconnected normally, reconnecting");
                    backoff = INITIAL_BACKOFF;
                }
                Err(e) => {
                    warn!(error = %e, backoff_secs = backoff.as_secs(), "Binance WebSocket error");
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Establishes a WebSocket connection, subscribes to bookTicker streams,
    /// and processes messages until disconnection.
    async fn connect_and_stream(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let binance_symbols = self.fetch_binance_symbols().await?;
        let token_symbols = self.get_token_symbols();
        let pairs = discover_pairs(&token_symbols, &binance_symbols);

        if pairs.is_empty() {
            return Err("no valid Binance pairs discovered".into());
        }

        let (ws_stream, _) = connect_async(&self.ws_url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Subscribe to bookTicker streams for all discovered pairs.
        self.subscribed_symbols = pairs.iter().cloned().collect();
        let streams: Vec<String> = pairs
            .iter()
            .map(|s| format!("{}@bookTicker", s.to_lowercase()))
            .collect();

        let sub_id = self.next_subscription_id;
        self.next_subscription_id += 1;
        let subscribe_msg = serde_json::json!({
            "method": "SUBSCRIBE",
            "params": streams,
            "id": sub_id
        });
        write
            .send(Message::Text(subscribe_msg.to_string().into()))
            .await?;

        info!(pair_count = pairs.len(), id = sub_id, "subscribed to Binance bookTicker streams");

        let mut resync_interval = tokio::time::interval(RESYNC_INTERVAL);
        resync_interval.tick().await; // consume the first immediate tick

        loop {
            tokio::select! {
                msg = read.next() => {
                    let Some(msg) = msg else {
                        return Ok(());
                    };
                    self.handle_message(&msg?);
                }
                _ = resync_interval.tick() => {
                    self.discover_new_pairs(&mut write).await;
                }
            }
        }
    }

    /// Routes a WebSocket message by type: text frames are parsed as bookTicker
    /// updates, close frames are logged, and everything else is ignored.
    fn handle_message(&self, msg: &Message) {
        match msg {
            Message::Text(text) => self.update_price_cache(text),
            Message::Close(frame) => {
                let reason = frame
                    .as_ref()
                    .map(|f| format!("code={}, reason={}", f.code, f.reason))
                    .unwrap_or_else(|| "no reason".to_string());
                debug!(reason, "Binance WebSocket close frame received");
            }
            _ => {}
        }
    }

    /// Parses a bookTicker JSON message and upserts the bid/ask into the ticker cache.
    /// Silently drops messages that fail to parse or have non-positive prices.
    fn update_price_cache(&self, text: &str) {
        let Ok(ticker) = serde_json::from_str::<BookTickerMsg>(text) else {
            return;
        };

        let Some(symbol) = &ticker.s else {
            return;
        };

        let bid = ticker.b.parse::<f64>().unwrap_or(0.0);
        let ask = ticker.a.parse::<f64>().unwrap_or(0.0);
        if bid <= 0.0 || ask <= 0.0 {
            return;
        }

        let timestamp_ms = ticker.event_time.unwrap_or_else(now_ms);

        let Ok(mut cache) = self.price_cache.write() else {
            warn!("ticker cache lock poisoned, dropping update");
            return;
        };
        cache.insert(symbol.clone(), TickerData { bid, ask, timestamp_ms });

        // When a ticker quotes against USDT or USDC, inject synthetic entries for
        // all USD stablecoins so unlisted stablecoins (DAI, GHO, …) get priced
        // as if they were USDT/USDC.
        inject_stablecoin_tickers(&mut cache, symbol, bid, ask, timestamp_ms);
    }

    /// Fetches valid TRADING symbols from Binance exchange info.
    async fn fetch_binance_symbols(
        &self,
    ) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        let resp: ExchangeInfoResponse = self
            .client
            .get(&self.exchange_info_url)
            .send()
            .await?
            .json()
            .await?;
        let symbols: HashSet<String> = resp
            .symbols
            .into_iter()
            .filter(|s| s.status == "TRADING")
            .map(|s| s.symbol)
            .collect();
        debug!(count = symbols.len(), "fetched Binance exchange symbols");
        Ok(symbols)
    }

    /// Gets all unique normalized token symbols from the token cache.
    fn get_token_symbols(&self) -> HashSet<String> {
        let Ok(cache) = self.token_cache.read() else {
            return HashSet::new();
        };
        cache
            .values()
            .map(|info| normalize_symbol(&info.symbol).0)
            .collect()
    }

    /// Snapshots the token registry from SharedMarketData into the local token cache.
    /// Skips the clone when the registry size hasn't changed, since tokens are only
    /// ever added — never removed or replaced.
    async fn refresh_token_cache(&self) {
        let current_len = self
            .token_cache
            .read()
            .map(|c| c.len())
            .unwrap_or(0);

        let new_cache: HashMap<Address, Token> = {
            let data = self.market_data.read().await;
            let registry = data.token_registry_ref();
            if registry.len() == current_len {
                return;
            }
            registry
                .iter()
                .map(|(address, token)| (address.clone(), token.clone()))
                .collect()
        };

        let mut cache = match self.token_cache.write() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "token cache lock poisoned");
                return;
            }
        };

        *cache = new_cache;
    }

    /// Re-reads tokens from SharedMarketData and subscribes to any new Binance
    /// pairs that appeared since the last sync.
    async fn discover_new_pairs<S>(&mut self, write: &mut S)
    where
        S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        self.refresh_token_cache().await;
        if let Err(e) = self.resync_subscriptions(write).await {
            warn!(error = %e, "failed to resync Binance subscriptions");
        }
    }

    /// Checks for new token pairs and subscribes to additional streams.
    async fn resync_subscriptions<S>(
        &mut self,
        write: &mut S,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        let binance_symbols = self.fetch_binance_symbols().await?;
        let token_symbols = self.get_token_symbols();
        let all_pairs = discover_pairs(&token_symbols, &binance_symbols);

        let new_pairs: Vec<String> = all_pairs
            .into_iter()
            .filter(|p| !self.subscribed_symbols.contains(p))
            .collect();

        if new_pairs.is_empty() {
            return Ok(());
        }

        let streams: Vec<String> = new_pairs
            .iter()
            .map(|s| format!("{}@bookTicker", s.to_lowercase()))
            .collect();

        let sub_id = self.next_subscription_id;
        self.next_subscription_id += 1;
        let subscribe_msg = serde_json::json!({
            "method": "SUBSCRIBE",
            "params": streams,
            "id": sub_id
        });
        write
            .send(Message::Text(subscribe_msg.to_string().into()))
            .await?;

        info!(new_pairs = new_pairs.len(), id = sub_id, "subscribed to additional Binance streams");
        self.subscribed_symbols
            .extend(new_pairs);
        Ok(())
    }
}

/// Discovers candidate Binance pairs by cross-referencing token symbols with available pairs.
///
/// For each token symbol, generates candidate pairs with every intermediate asset
/// and checks if Binance lists them (in either order). Also generates pairs between
/// all token symbols directly.
fn discover_pairs(
    token_symbols: &HashSet<String>,
    binance_symbols: &HashSet<String>,
) -> Vec<String> {
    let mut pairs = HashSet::new();

    for symbol in token_symbols {
        for quote in INTERMEDIATE_ASSETS {
            let direct = format!("{symbol}{quote}");
            if binance_symbols.contains(&direct) {
                pairs.insert(direct);
            }
            let reverse = format!("{quote}{symbol}");
            if binance_symbols.contains(&reverse) {
                pairs.insert(reverse);
            }
        }
    }

    // Also try direct pairs between tracked tokens.
    let symbols: Vec<&String> = token_symbols.iter().collect();
    for (i, a) in symbols.iter().enumerate() {
        for b in &symbols[i + 1..] {
            let pair_ab = format!("{a}{b}");
            if binance_symbols.contains(&pair_ab) {
                pairs.insert(pair_ab);
            }
            let pair_ba = format!("{b}{a}");
            if binance_symbols.contains(&pair_ba) {
                pairs.insert(pair_ba);
            }
        }
    }

    pairs.into_iter().collect()
}

/// For a ticker like `ETHUSDT`, injects synthetic entries `ETHDAI`, `ETHGHO`, etc. for every
/// USD stablecoin in `stable_usd.json` that doesn't already have a real Binance pair cached.
/// Similarly handles `USDTETH`-style pairs (quote asset is a stablecoin).
fn inject_stablecoin_tickers(
    cache: &mut HashMap<String, TickerData>,
    symbol: &str,
    bid: f64,
    ask: f64,
    timestamp_ms: u64,
) {
    for quote in ["USDT", "USDC"] {
        if let Some(base) = symbol.strip_suffix(quote) {
            for stable in USD_STABLECOINS.iter() {
                if stable == quote {
                    continue;
                }
                let synthetic = format!("{base}{stable}");
                cache
                    .entry(synthetic)
                    .or_insert(TickerData { bid, ask, timestamp_ms });
            }
            return;
        }
        if let Some(base) = symbol.strip_prefix(quote) {
            for stable in USD_STABLECOINS.iter() {
                if stable == quote {
                    continue;
                }
                let synthetic = format!("{stable}{base}");
                cache
                    .entry(synthetic)
                    .or_insert(TickerData { bid, ask, timestamp_ms });
            }
            return;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// -- Binance API/WS response types --

/// BookTicker WebSocket message.
#[derive(serde::Deserialize)]
struct BookTickerMsg {
    /// Symbol (e.g. "ETHUSDT").
    s: Option<String>,
    /// Best bid price.
    b: String,
    /// Best ask price.
    a: String,
    /// Event time in milliseconds.
    #[serde(rename = "E")]
    event_time: Option<u64>,
}

/// Response from GET /api/v3/exchangeInfo.
#[derive(serde::Deserialize)]
struct ExchangeInfoResponse {
    symbols: Vec<SymbolInfo>,
}

/// Per-symbol metadata from exchange info.
#[derive(serde::Deserialize)]
struct SymbolInfo {
    symbol: String,
    status: String,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tokio::sync::RwLock;
    use tycho_simulation::evm::tycho_models::Chain;

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    fn make_token(address: Address, symbol: &str, decimals: u32) -> Token {
        Token {
            address,
            symbol: symbol.to_string(),
            decimals,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn weth_address() -> Address {
        "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .expect("valid address")
    }

    fn usdc_address() -> Address {
        "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .expect("valid address")
    }

    fn link_address() -> Address {
        "514910771AF9Ca656af840dff83E8264EcF986CA"
            .parse()
            .expect("valid address")
    }

    fn usdt_address() -> Address {
        "dAC17F958D2ee523a2206206994597C13D831ec7"
            .parse()
            .expect("valid address")
    }

    fn cheems_address() -> Address {
        "0x41b1f9dcd5923c9542b6957b9b72169595acbc5c"
            .parse()
            .expect("valid address")
    }

    /// Returns a provider pre-seeded with ETH, BTC, LINK tickers and
    /// WETH, USDC, LINK token metadata.
    fn seeded_provider() -> BinanceWsProvider {
        let provider = BinanceWsProvider::default();
        let now = now_ms();

        {
            let mut cache = provider
                .price_cache
                .write()
                .expect("lock");
            cache.insert(
                "ETHUSDT".to_string(),
                TickerData { bid: 2000.0, ask: 2000.5, timestamp_ms: now },
            );
            cache.insert(
                "ETHUSDC".to_string(),
                TickerData { bid: 2000.0, ask: 2000.5, timestamp_ms: now },
            );
            cache.insert(
                "BTCUSDT".to_string(),
                TickerData { bid: 60000.0, ask: 60010.0, timestamp_ms: now },
            );
            cache.insert(
                "LINKUSDT".to_string(),
                TickerData { bid: 15.0, ask: 15.01, timestamp_ms: now },
            );
            cache.insert(
                "ETHBTC".to_string(),
                TickerData { bid: 0.0333, ask: 0.0334, timestamp_ms: now },
            );
            cache.insert(
                "1000CHEEMSUSDC".to_string(),
                TickerData { bid: 0.000433, ask: 0.000434, timestamp_ms: now },
            );
        }

        {
            let mut cache = provider
                .token_cache
                .write()
                .expect("lock");
            let weth = make_token(weth_address(), "WETH", 18);
            cache.insert(weth.address.clone(), weth);
            let usdc = make_token(usdc_address(), "USDC", 6);
            cache.insert(usdc.address.clone(), usdc);
            let link = make_token(link_address(), "LINK", 18);
            cache.insert(link.address.clone(), link);
            let cheems = make_token(cheems_address(), "CHEEMS", 18);
            cache.insert(cheems.address.clone(), cheems);
        }

        provider
    }

    /// Creates a `BinanceWsWorker` that writes to the given ticker cache.
    fn make_worker(price_cache: &PriceCache) -> BinanceWsWorker {
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        BinanceWsWorker {
            price_cache: Arc::clone(price_cache),
            token_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            market_data,
            client: Client::new(),
            ws_url: DEFAULT_WS_URL.to_string(),
            exchange_info_url: DEFAULT_EXCHANGE_INFO_URL.to_string(),
            subscribed_symbols: HashSet::new(),
            next_subscription_id: 1,
        }
    }

    // -- Price resolution tests --

    #[test]
    fn test_direct_pair_price() {
        let provider = seeded_provider();
        let one_eth = BigUint::from(10u64).pow(18);

        let result = provider
            .get_expected_out(&weth_address(), &usdc_address(), &one_eth)
            .expect("should get price");

        // 1 ETH at bid 2000 USDC → 2_000_000_000 (6 decimals)
        assert_eq!(*result.expected_amount_out(), BigUint::from(2_000_000_000u64));
        assert_eq!(result.source(), "binance_ws");
    }

    #[test]
    fn test_price_via_intermediate_usdt() {
        let provider = seeded_provider();
        let ten_link = BigUint::from(10u64) * BigUint::from(10u64).pow(18);

        let result = provider
            .get_expected_out(&link_address(), &weth_address(), &ten_link)
            .expect("should get price");

        // 10 LINK → ETH via USDT:
        //   sell LINK: bid=15.0, buy ETH: ask=2000.5
        //   10 * (15.0 / 2000.5) ≈ 0.07498 ETH
        let expected_price = 15.0 / 2000.5;
        let expected_raw = (10.0 * expected_price * 1e18) as u128;
        let expected = BigUint::from(expected_raw);
        let diff = if *result.expected_amount_out() > expected {
            result.expected_amount_out() - &expected
        } else {
            &expected - result.expected_amount_out()
        };
        let tolerance = &expected / BigUint::from(100u64); // 1%
        assert!(diff < tolerance, "result={}, expected ~{expected}", result.expected_amount_out());
    }

    #[test]
    fn test_reverse_pair_price() {
        let provider = BinanceWsProvider::default();
        let now = now_ms();

        {
            let mut cache = provider
                .price_cache
                .write()
                .expect("lock");
            cache.insert(
                "BTCETH".to_string(),
                TickerData { bid: 30.0, ask: 30.01, timestamp_ms: now },
            );
        }

        let cache = provider
            .price_cache
            .read()
            .expect("lock");
        // ETH→BTC via reverse BTCETH: sell-side uses 1/ask
        let lookup = BinanceWsProvider::lookup_sell_price(&cache, "ETH", "BTC");
        assert!(lookup.is_some());
        let lookup = lookup.expect("should have price");
        let expected = 1.0 / 30.01;
        assert!((lookup.price - expected).abs() < 1e-10);

        // Buy-side uses 1/bid
        let buy = BinanceWsProvider::lookup_buy_price(&cache, "ETH", "BTC");
        let buy = buy.expect("should have buy price");
        let expected_buy = 1.0 / 30.0;
        assert!((buy.price - expected_buy).abs() < 1e-10);
    }

    #[test]
    fn test_direct_pair_price_1000_token() {
        // Testing for pairs that are quoted in 1000 tokens
        // 1000CHEEMSUSDC price is 0.000433 USDC per 1000 CHEEMS
        // which means the real price is 0.000000433 USDC per 1 CHEEMS (bid)
        // and 1 USDC is ~23041474 CHEEMS (ask)
        let provider = seeded_provider();
        let amount = BigUint::from(10u64).pow(22);

        let result = provider
            .get_expected_out(&cheems_address(), &usdc_address(), &amount)
            .expect("should get price");

        // 10_000 * 10**18 CHEEMS → 4330 USDC (6 decimals)
        assert_eq!(*result.expected_amount_out(), BigUint::from(4330u64));

        // let's test the reverse price
        let amount = BigUint::from(10u64).pow(7);

        let result = provider
            .get_expected_out(&usdc_address(), &cheems_address(), &amount)
            .expect("should get price");

        // 10 * 10**6 USDC  → ~23094680 * 10**18 CHEEMS
        assert_eq!(
            *result.expected_amount_out(),
            BigUint::from_str("23041474_654377879797760000").unwrap()
        );
    }
    #[test]
    fn test_pair_price_1000_token_intermediate_usdc() {
        // Testing for pairs that are quoted in 1000 tokens
        let provider = seeded_provider();
        let amount = BigUint::from(10u64).pow(22);

        let result = provider
            .get_expected_out(&cheems_address(), &weth_address(), &amount)
            .expect("should get price");

        // 10_000 * 10**18 CHEEMS → 4330 USDC -> 2164458880000 ETH
        assert_eq!(*result.expected_amount_out(), BigUint::from(2164458880000u64));

        // let's test the reverse price
        let amount = BigUint::from(10u64).pow(18);

        let result = provider
            .get_expected_out(&weth_address(), &cheems_address(), &amount)
            .expect("should get price");

        // 1 * 10**18 ETH -> 2000 * 10**6 USDC  → ~4608294930 * 10**18 CHEEMS
        assert_eq!(
            *result.expected_amount_out(),
            BigUint::from_str("4608294930_875576062631215104").unwrap()
        );
    }

    #[test]
    fn test_unknown_token_returns_error() {
        let provider = seeded_provider();
        let unknown: Address = "0000000000000000000000000000000000000001"
            .parse()
            .expect("valid");
        let one = BigUint::from(10u64).pow(18);

        let result = provider.get_expected_out(&unknown, &usdc_address(), &one);
        assert!(result.is_err());
        assert!(matches!(result, Err(PriceProviderError::TokenNotFound { .. })));
    }

    #[test]
    fn test_stale_price_returns_error() {
        let provider = BinanceWsProvider::default();
        let stale_ts = 1_000u64;

        {
            let mut cache = provider
                .price_cache
                .write()
                .expect("lock");
            cache.insert(
                "ETHUSDT".to_string(),
                TickerData { bid: 2000.0, ask: 2000.5, timestamp_ms: stale_ts },
            );
        }
        {
            let mut cache = provider
                .token_cache
                .write()
                .expect("lock");
            let weth = make_token(weth_address(), "WETH", 18);
            cache.insert(weth.address.clone(), weth);
            let usdt = make_token(usdt_address(), "USDT", 6);
            cache.insert(usdt.address.clone(), usdt);
        }

        let one_eth = BigUint::from(10u64).pow(18);
        let result = provider.get_expected_out(&weth_address(), &usdt_address(), &one_eth);
        assert!(result.is_err());
        assert!(matches!(result, Err(PriceProviderError::StaleData { .. })));
    }

    // -- handle_message / update_price_cache tests (M4) --

    #[test]
    fn test_update_price_cache_writes_to_cache() {
        let price_cache: PriceCache = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let worker = make_worker(&price_cache);

        let msg = Message::Text(
            r#"{"s":"ETHUSDT","b":"2000.00","a":"2000.50","E":1700000000000}"#.into(),
        );
        worker.handle_message(&msg);

        let cache = price_cache.read().expect("lock");
        let ticker = cache
            .get("ETHUSDT")
            .expect("should be cached");
        assert!((ticker.bid - 2000.0).abs() < f64::EPSILON);
        assert!((ticker.ask - 2000.5).abs() < f64::EPSILON);
        assert_eq!(ticker.timestamp_ms, 1_700_000_000_000);
    }

    #[test]
    fn test_update_price_cache_rejects_zero_bid() {
        let price_cache: PriceCache = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let worker = make_worker(&price_cache);

        let msg = Message::Text(r#"{"s":"ETHUSDT","b":"0","a":"2000.50"}"#.into());
        worker.handle_message(&msg);

        let cache = price_cache.read().expect("lock");
        assert!(cache.get("ETHUSDT").is_none());
    }

    #[test]
    fn test_update_price_cache_rejects_zero_ask() {
        let price_cache: PriceCache = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let worker = make_worker(&price_cache);

        let msg = Message::Text(r#"{"s":"ETHUSDT","b":"2000.00","a":"0"}"#.into());
        worker.handle_message(&msg);

        let cache = price_cache.read().expect("lock");
        assert!(cache.get("ETHUSDT").is_none());
    }

    #[test]
    fn test_update_price_cache_uses_now_ms_when_no_event_time() {
        let price_cache: PriceCache = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let worker = make_worker(&price_cache);

        let before = now_ms();
        let msg = Message::Text(r#"{"s":"ETHUSDT","b":"2000.00","a":"2000.50"}"#.into());
        worker.handle_message(&msg);
        let after = now_ms();

        let cache = price_cache.read().expect("lock");
        let ticker = cache
            .get("ETHUSDT")
            .expect("should be cached");
        assert!(ticker.timestamp_ms >= before && ticker.timestamp_ms <= after);
    }

    #[test]
    fn test_handle_message_ignores_non_text() {
        let price_cache: PriceCache = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let worker = make_worker(&price_cache);

        worker.handle_message(&Message::Ping(vec![].into()));
        worker.handle_message(&Message::Pong(vec![].into()));
        worker.handle_message(&Message::Binary(vec![].into()));

        let cache = price_cache.read().expect("lock");
        assert!(cache.is_empty());
    }

    // -- Deserialization tests --

    #[test]
    fn test_parse_book_ticker_msg() {
        let json = r#"{"e":"bookTicker","u":123,"s":"ETHUSDT","b":"2000.00","B":"10.5","a":"2000.50","A":"5.2","E":1700000000000}"#;
        let msg: BookTickerMsg = serde_json::from_str(json).expect("should parse");
        assert_eq!(msg.s.as_deref(), Some("ETHUSDT"));
        assert_eq!(msg.b, "2000.00");
        assert_eq!(msg.a, "2000.50");
        assert_eq!(msg.event_time, Some(1_700_000_000_000));
    }

    #[test]
    fn test_parse_exchange_info_response() {
        let json = r#"{"symbols":[{"symbol":"ETHUSDT","status":"TRADING"},{"symbol":"ETHBTC","status":"BREAK"}]}"#;
        let resp: ExchangeInfoResponse = serde_json::from_str(json).expect("should parse");
        assert_eq!(resp.symbols.len(), 2);
        assert_eq!(resp.symbols[0].symbol, "ETHUSDT");
        assert_eq!(resp.symbols[0].status, "TRADING");
    }

    // -- Pair discovery tests --

    #[test]
    fn test_discover_pairs_filters_correctly() {
        let tokens: HashSet<String> = ["ETH", "LINK", "AAVE"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let binance: HashSet<String> =
            ["ETHUSDT", "ETHBTC", "LINKUSDT", "LINKETH", "AAVEUSDT", "XYZUSDT"]
                .iter()
                .map(|s| s.to_string())
                .collect();

        let pairs = discover_pairs(&tokens, &binance);
        assert!(pairs.contains(&"ETHUSDT".to_string()));
        assert!(pairs.contains(&"ETHBTC".to_string()));
        assert!(pairs.contains(&"LINKUSDT".to_string()));
        assert!(pairs.contains(&"LINKETH".to_string()));
        assert!(pairs.contains(&"AAVEUSDT".to_string()));
        assert!(!pairs.contains(&"XYZUSDT".to_string()));
    }

    // -- Stablecoin cache injection tests --

    #[test]
    fn test_inject_creates_synthetic_stablecoin_tickers() {
        let mut cache = HashMap::new();
        let now = now_ms();

        // Receiving ETHUSDT should create ETHDAI, ETHGHO, etc.
        inject_stablecoin_tickers(&mut cache, "ETHUSDT", 2000.0, 2000.5, now);

        assert!(cache.contains_key("ETHDAI"));
        assert!(cache.contains_key("ETHGHO"));
        assert!(cache.contains_key("ETHUSDC"));
        // Should not create ETHUSDT — that's the original, already in cache via the caller.
        assert!(!cache.contains_key("ETHUSDT"));
    }

    #[test]
    fn test_inject_does_not_overwrite_real_ticker() {
        let mut cache = HashMap::new();
        let now = now_ms();

        // Pre-populate a real ETHUSDC ticker with a different price.
        cache.insert(
            "ETHUSDC".to_string(),
            TickerData { bid: 1999.0, ask: 1999.5, timestamp_ms: now },
        );

        inject_stablecoin_tickers(&mut cache, "ETHUSDT", 2000.0, 2000.5, now);

        // Real ticker should be preserved.
        let real = cache
            .get("ETHUSDC")
            .expect("should exist");
        assert!((real.bid - 1999.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_inject_handles_quote_prefix_pair() {
        let mut cache = HashMap::new();
        let now = now_ms();

        // USDCETH → strip "USDC" prefix, remainder is "ETH", create "DAITH", "GHOETH", etc.
        inject_stablecoin_tickers(&mut cache, "USDCETH", 0.0005, 0.00051, now);

        assert!(cache.contains_key("DAIETH"));
        assert!(cache.contains_key("GHOETH"));
        assert!(cache.contains_key("USDTETH"));
    }

    #[test]
    fn test_unlisted_stablecoin_resolves_via_cache() {
        let provider = seeded_provider();

        let dai_address: Address = "6B175474E89094C44Da98b954EedeAC495271d0F"
            .parse()
            .expect("valid address");
        {
            let mut cache = provider
                .token_cache
                .write()
                .expect("lock");
            let dai = make_token(dai_address.clone(), "DAI", 18);
            cache.insert(dai.address.clone(), dai);
        }

        // Simulate what update_price_cache does: inject stablecoin tickers from ETHUSDT.
        {
            let mut cache = provider
                .price_cache
                .write()
                .expect("lock");
            inject_stablecoin_tickers(&mut cache, "ETHUSDT", 2000.0, 2000.5, now_ms());
        }

        let one_eth = BigUint::from(10u64).pow(18);
        let result = provider
            .get_expected_out(&weth_address(), &dai_address, &one_eth)
            .expect("should resolve ETH/DAI via synthetic ticker");

        // ETH/DAI uses the ETHUSDT bid (2000.0)
        let expected = BigUint::from(2000u64) * BigUint::from(10u64).pow(18);
        let diff = if *result.expected_amount_out() > expected {
            result.expected_amount_out() - &expected
        } else {
            &expected - result.expected_amount_out()
        };
        let tolerance = &expected / BigUint::from(100u64); // 1%
        assert!(diff < tolerance, "result={}, expected ~{expected}", result.expected_amount_out());
    }

    #[test]
    fn test_ws_message_creates_synthetic_stablecoin_ticker() {
        // End-to-end: a raw ETHUSDT WebSocket message arrives, the worker processes
        // it, and we can price ETH/GHO through the synthetic cache entry.
        let provider = BinanceWsProvider::default();
        let price_cache = Arc::clone(&provider.price_cache);
        let worker = make_worker(&price_cache);

        let gho_address: Address = "40D16FC0246aD3160Ccc09B8D0D3A2cD28aE6C2f"
            .parse()
            .expect("valid address");
        {
            let mut cache = provider
                .token_cache
                .write()
                .expect("lock");
            let weth = make_token(weth_address(), "WETH", 18);
            cache.insert(weth.address.clone(), weth);
            let gho = make_token(gho_address.clone(), "GHO", 18);
            cache.insert(gho.address.clone(), gho);
        }

        // Simulate a bookTicker message arriving from Binance (no event time → uses now).
        let msg = Message::Text(r#"{"s":"ETHUSDT","b":"2000.00","a":"2000.50"}"#.into());
        worker.handle_message(&msg);

        // The cache should now contain a synthetic ETHGHO entry.
        {
            let cache = price_cache.read().expect("lock");
            assert!(cache.contains_key("ETHGHO"), "synthetic ETHGHO ticker missing");
        }

        // Price 1 ETH → GHO (both 18 decimals).
        let one_eth = BigUint::from(10u64).pow(18);
        let result = provider
            .get_expected_out(&weth_address(), &gho_address, &one_eth)
            .expect("should resolve ETH/GHO via synthetic ticker");

        // Sell-side uses bid = 2000.0 → 2000 GHO (18 decimals)
        let expected = BigUint::from(2000u64) * BigUint::from(10u64).pow(18);
        assert_eq!(*result.expected_amount_out(), expected);
        assert_eq!(result.source(), "binance_ws");
    }

    // -- Live integration test --

    #[tokio::test]
    #[ignore] // requires network access
    async fn test_binance_ws_live_weth_usdc() {
        let weth = make_token(weth_address(), "WETH", 18);
        let usdc = make_token(usdc_address(), "USDC", 6);

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([weth, usdc]);
        let market_data = Arc::new(RwLock::new(market_data));

        let mut provider = BinanceWsProvider::default();
        let _handle = provider.start(market_data);

        // Retry: WebSocket connection + initial tickers may take a few seconds.
        let one_eth = BigUint::from(10u64).pow(18);
        let mut price = None;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            match provider.get_expected_out(&weth_address(), &usdc_address(), &one_eth) {
                Ok(p) => {
                    price = Some(p);
                    break;
                }
                Err(e) => debug!(error = %e, "waiting for Binance WS price"),
            }
        }

        let price = price.expect("should get a price from Binance WS within 20s");

        // 1 ETH should be worth between $1,000 and $10,000 USDC (6 decimals)
        let min = BigUint::from(1_000_000_000u64); // $1,000
        let max = BigUint::from(10_000_000_000u64); // $10,000
        let amount = price.expected_amount_out();
        assert!(
            *amount >= min && *amount <= max,
            "expected amount_out in [{min}, {max}], got {amount}"
        );
    }
}
