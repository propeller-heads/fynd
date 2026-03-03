use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use num_bigint::BigUint;
use reqwest::Client;
use tokio::{sync::RwLock, task::JoinHandle};
use tracing::{debug, error, info, warn};
use tycho_simulation::tycho_common::models::Address;

use super::{
    common::{check_staleness, compute_expected_out, normalize_symbol, resolve_token},
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
};
use crate::feed::market_data::SharedMarketData;

const WS_URL: &str = "wss://stream.binance.com:9443/ws";
const EXCHANGE_INFO_URL: &str = "https://api.binance.com/api/v3/exchangeInfo";
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const RESYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Quote currencies to pair each token symbol against when building subscriptions.
const QUOTE_CURRENCIES: &[&str] = &["USDT", "USDC", "ETH", "BTC"];

/// Common quote currencies used to find intermediate paths in the cache.
const INTERMEDIATE_QUOTES: &[&str] = QUOTE_CURRENCIES;

/// Cached book ticker entry from Binance.
#[derive(Debug, Clone)]
struct TickerData {
    bid: f64,
    ask: f64,
    timestamp_ms: u64,
}

/// Shared price cache. Key is the Binance symbol (e.g. "ETHUSDC").
type PriceCache = Arc<RwLock<HashMap<String, TickerData>>>;

struct PriceLookup {
    price: f64,
    timestamp_ms: u64,
}

/// Binance WebSocket price provider.
///
/// Dynamically subscribes to bookTicker streams for all tokens in [`SharedMarketData`]
/// that have a valid Binance trading pair. New tokens are picked up every 60 seconds.
pub struct BinanceWsProvider {
    cache: PriceCache,
    /// Token registry for resolving on-chain addresses to exchange symbols and decimals.
    market_data: Option<Arc<RwLock<SharedMarketData>>>,
}

impl BinanceWsProvider {
    pub fn new() -> Self {
        Self { cache: Arc::new(RwLock::new(HashMap::new())), market_data: None }
    }

    /// Attempts to find a price between two symbols in the cache.
    ///
    /// Tries: direct pair, reverse pair, then routing through intermediates.
    fn lookup_price(
        cache: &HashMap<String, TickerData>,
        sym_in: &str,
        sym_out: &str,
        now_ms: u64,
    ) -> Result<PriceLookup, PriceProviderError> {
        // Direct pair: sym_in is base, sym_out is quote → selling base for quote
        let direct = format!("{}{}", sym_in, sym_out);
        if let Some(ticker) = cache.get(&direct) {
            check_staleness(ticker.timestamp_ms, now_ms)?;
            return Ok(PriceLookup { price: ticker.bid, timestamp_ms: ticker.timestamp_ms });
        }

        // Reverse pair: sym_out is base, sym_in is quote → buying base with quote
        let reverse = format!("{}{}", sym_out, sym_in);
        if let Some(ticker) = cache.get(&reverse) {
            check_staleness(ticker.timestamp_ms, now_ms)?;
            if ticker.ask == 0.0 {
                return Err(PriceProviderError::Unavailable("zero ask price".into()));
            }
            return Ok(PriceLookup { price: 1.0 / ticker.ask, timestamp_ms: ticker.timestamp_ms });
        }

        // Intermediate routing: sym_in → intermediate → sym_out
        for &intermediate in INTERMEDIATE_QUOTES {
            if intermediate == sym_in || intermediate == sym_out {
                continue;
            }

            let price_in = Self::lookup_direct_or_reverse(cache, sym_in, intermediate);
            let price_out = Self::lookup_direct_or_reverse(cache, intermediate, sym_out);

            if let (Some((p_in, ts_in)), Some((p_out, ts_out))) = (price_in, price_out) {
                let oldest_ts = ts_in.min(ts_out);
                check_staleness(oldest_ts, now_ms)?;
                return Ok(PriceLookup { price: p_in * p_out, timestamp_ms: oldest_ts });
            }
        }

        Err(PriceProviderError::PriceNotFound {
            token_in: sym_in.to_string(),
            token_out: sym_out.to_string(),
        })
    }

    /// Looks up a direct or reverse pair, returning the price for selling `base` to get `quote`.
    fn lookup_direct_or_reverse(
        cache: &HashMap<String, TickerData>,
        base: &str,
        quote: &str,
    ) -> Option<(f64, u64)> {
        let direct = format!("{}{}", base, quote);
        if let Some(t) = cache.get(&direct) {
            return Some((t.bid, t.timestamp_ms));
        }
        let reverse = format!("{}{}", quote, base);
        if let Some(t) = cache.get(&reverse) {
            if t.ask > 0.0 {
                return Some((1.0 / t.ask, t.timestamp_ms));
            }
        }
        None
    }
}

#[async_trait]
impl PriceProvider for BinanceWsProvider {
    fn start(&mut self, market_data: Arc<RwLock<SharedMarketData>>) -> JoinHandle<()> {
        self.market_data = Some(Arc::clone(&market_data));
        let worker =
            BinanceWsWorker { cache: Arc::clone(&self.cache), market_data, client: Client::new() };
        tokio::spawn(async move { worker.run().await })
    }

    async fn get_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Result<ExternalPrice, PriceProviderError> {
        let market_data = self
            .market_data
            .as_ref()
            .ok_or_else(|| PriceProviderError::Unavailable("provider not started".into()))?;
        let (sym_in, dec_in) = resolve_token(market_data, token_in).await?;
        let (sym_out, dec_out) = resolve_token(market_data, token_out).await?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let cache = self.cache.read().await;
        let price_lookup = Self::lookup_price(&cache, &sym_in, &sym_out, now_ms)?;

        let expected_out = compute_expected_out(amount_in, price_lookup.price, dec_in, dec_out);

        Ok(ExternalPrice::new(expected_out, "binance_ws".to_string(), price_lookup.timestamp_ms))
    }
}

/// Background task that connects to Binance WebSocket and populates the price cache.
///
/// On each connection, fetches the Binance exchange info to discover which trading
/// pairs exist, then cross-references with [`SharedMarketData`] to subscribe only to
/// relevant bookTicker streams. Periodically re-checks for new tokens and subscribes
/// to additional streams on the live connection.
struct BinanceWsWorker {
    cache: PriceCache,
    market_data: Arc<RwLock<SharedMarketData>>,
    client: Client,
}

impl BinanceWsWorker {
    /// Runs the WebSocket loop. Reconnects with exponential backoff on failure.
    async fn run(&self) {
        let mut current_delay = RECONNECT_DELAY;

        loop {
            info!(url = WS_URL, "connecting to Binance WebSocket");

            let valid_symbols = match self.fetch_valid_symbols().await {
                Ok(s) => {
                    debug!(count = s.len(), "fetched Binance exchange info");
                    s
                }
                Err(e) => {
                    warn!(error = %e, "failed to fetch Binance exchange info, will retry");
                    tokio::time::sleep(current_delay).await;
                    current_delay = (current_delay * 2).min(MAX_RECONNECT_DELAY);
                    continue;
                }
            };

            match tokio_tungstenite::connect_async(WS_URL).await {
                Ok((ws_stream, _)) => {
                    info!("Binance WebSocket connected");
                    current_delay = RECONNECT_DELAY;

                    let (mut write, mut read) = ws_stream.split();

                    let streams = self.build_streams(&valid_symbols).await;
                    let mut subscribed: HashSet<String> = streams.iter().cloned().collect();

                    if streams.is_empty() {
                        warn!("no Binance streams to subscribe to");
                    } else {
                        info!(count = streams.len(), "subscribing to Binance bookTicker streams");
                        if let Err(e) = Self::subscribe(&mut write, &streams, 1).await {
                            warn!(error = %e, "failed to subscribe");
                            continue;
                        }
                    }

                    let mut sub_id: u64 = 2;
                    let mut resync = tokio::time::interval(RESYNC_INTERVAL);
                    resync.tick().await; // consume the immediate first tick

                    loop {
                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                        self.handle_message(&text).await;
                                    }
                                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                                        warn!("Binance WebSocket closed by server");
                                        break;
                                    }
                                    Some(Ok(_)) => {}
                                    Some(Err(e)) => {
                                        warn!(error = %e, "Binance WebSocket read error");
                                        break;
                                    }
                                    None => break,
                                }
                            }
                            _ = resync.tick() => {
                                let new_streams = self.build_streams(&valid_symbols).await;
                                let additions: Vec<String> = new_streams
                                    .into_iter()
                                    .filter(|s| !subscribed.contains(s))
                                    .collect();
                                if !additions.is_empty() {
                                    info!(
                                        count = additions.len(),
                                        "subscribing to new Binance streams"
                                    );
                                    if let Err(e) =
                                        Self::subscribe(&mut write, &additions, sub_id).await
                                    {
                                        warn!(error = %e, "failed to subscribe to new streams");
                                    } else {
                                        subscribed.extend(additions);
                                        sub_id += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "failed to connect to Binance WebSocket");
                }
            }

            warn!(delay_secs = current_delay.as_secs(), "reconnecting to Binance WebSocket");
            tokio::time::sleep(current_delay).await;
            current_delay = (current_delay * 2).min(MAX_RECONNECT_DELAY);
        }
    }

    /// Fetches Binance exchange info and returns the set of actively-trading symbols.
    async fn fetch_valid_symbols(&self) -> Result<HashSet<String>, reqwest::Error> {
        let resp: ExchangeInfoResponse = self
            .client
            .get(EXCHANGE_INFO_URL)
            .send()
            .await?
            .json()
            .await?;

        let symbols = resp
            .symbols
            .into_iter()
            .filter(|s| s.status == "TRADING")
            .map(|s| s.symbol)
            .collect();

        Ok(symbols)
    }

    /// Reads all token symbols from market data and builds bookTicker stream names
    /// for pairs that exist on Binance.
    async fn build_streams(&self, valid_symbols: &HashSet<String>) -> Vec<String> {
        let market_data = self.market_data.read().await;
        let token_symbols: HashSet<String> = market_data
            .token_registry_ref()
            .values()
            .map(|t| normalize_symbol(&t.symbol).to_uppercase())
            .collect();
        drop(market_data);

        let mut streams = Vec::new();
        for symbol in &token_symbols {
            for &quote in QUOTE_CURRENCIES {
                if symbol == quote {
                    continue;
                }
                let pair = format!("{}{}", symbol, quote);
                if valid_symbols.contains(&pair) {
                    streams.push(format!("{}@bookTicker", pair.to_lowercase()));
                }
            }
        }

        streams.sort();
        streams.dedup();
        streams
    }

    async fn subscribe<W>(
        write: &mut W,
        streams: &[String],
        id: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: futures::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
        W::Error: std::error::Error + Send + Sync + 'static,
    {
        let sub_msg = serde_json::json!({
            "method": "SUBSCRIBE",
            "params": streams,
            "id": id
        });
        futures::SinkExt::send(
            write,
            tokio_tungstenite::tungstenite::Message::Text(sub_msg.to_string().into()),
        )
        .await?;
        Ok(())
    }

    async fn handle_message(&self, text: &str) {
        let parsed: Result<BookTickerMsg, _> = serde_json::from_str(text);
        match parsed {
            Ok(msg) => {
                let bid: f64 = match msg.b.parse() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let ask: f64 = match msg.a.parse() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let timestamp_ms = msg.event_time.unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                });

                let mut cache = self.cache.write().await;
                cache.insert(msg.s, TickerData { bid, ask, timestamp_ms });
            }
            Err(_) => {
                debug!(msg = text, "ignoring non-ticker message");
            }
        }
    }
}

/// Binance bookTicker WebSocket message (only the fields we need).
#[derive(serde::Deserialize)]
struct BookTickerMsg {
    /// Symbol (e.g. "ETHUSDC")
    s: String,
    /// Best bid price (string)
    b: String,
    /// Best ask price (string)
    a: String,
    /// Event time in milliseconds
    #[serde(rename = "E")]
    event_time: Option<u64>,
}

/// Binance exchange info response (only the fields we need).
#[derive(serde::Deserialize)]
struct ExchangeInfoResponse {
    symbols: Vec<SymbolInfo>,
}

/// Per-symbol info from Binance exchange info.
#[derive(serde::Deserialize)]
struct SymbolInfo {
    symbol: String,
    status: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::RwLock;
    use tycho_simulation::tycho_core::models::{token::Token, Chain};

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    #[test]
    fn test_lookup_price_direct() {
        let mut cache = HashMap::new();
        cache.insert(
            "ETHUSDC".to_string(),
            TickerData { bid: 2000.0, ask: 2001.0, timestamp_ms: 1000 },
        );

        let result = BinanceWsProvider::lookup_price(&cache, "ETH", "USDC", 1000);
        let lookup = result.unwrap();
        assert_eq!(lookup.price, 2000.0);
    }

    #[test]
    fn test_lookup_price_reverse() {
        let mut cache = HashMap::new();
        cache.insert(
            "ETHUSDC".to_string(),
            TickerData { bid: 2000.0, ask: 2001.0, timestamp_ms: 1000 },
        );

        let result = BinanceWsProvider::lookup_price(&cache, "USDC", "ETH", 1000);
        let lookup = result.unwrap();
        let expected = 1.0 / 2001.0;
        assert!((lookup.price - expected).abs() < 1e-10);
    }

    #[test]
    fn test_lookup_price_intermediate() {
        let mut cache = HashMap::new();
        cache.insert(
            "LINKUSDT".to_string(),
            TickerData { bid: 15.0, ask: 15.1, timestamp_ms: 1000 },
        );
        cache.insert(
            "AAVEUSDT".to_string(),
            TickerData { bid: 200.0, ask: 201.0, timestamp_ms: 1000 },
        );

        let result = BinanceWsProvider::lookup_price(&cache, "LINK", "AAVE", 1000);
        let lookup = result.unwrap();
        let expected = 15.0 * (1.0 / 201.0);
        assert!(
            (lookup.price - expected).abs() < 1e-10,
            "got {}, expected {}",
            lookup.price,
            expected
        );
    }

    #[test]
    fn test_lookup_price_not_found() {
        let cache = HashMap::new();
        let result = BinanceWsProvider::lookup_price(&cache, "UNKNOWN", "TOKEN", 1000);
        assert!(matches!(result, Err(PriceProviderError::PriceNotFound { .. })));
    }

    #[test]
    fn test_staleness_detection() {
        let mut cache = HashMap::new();
        cache.insert(
            "ETHUSDC".to_string(),
            TickerData { bid: 2000.0, ask: 2001.0, timestamp_ms: 1000 },
        );

        // 31 seconds later with 30s threshold
        let result = BinanceWsProvider::lookup_price(&cache, "ETH", "USDC", 32_000);
        assert!(matches!(result, Err(PriceProviderError::StaleData { .. })));
    }

    #[tokio::test]
    async fn test_build_streams_filters_by_valid_symbols() {
        let weth = Token {
            address: "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                .parse()
                .unwrap(),
            symbol: "WETH".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };
        let link = Token {
            address: "514910771AF9Ca656af840dff83E8264EcF986CA"
                .parse()
                .unwrap(),
            symbol: "LINK".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };
        let obscure = Token {
            address: "0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            symbol: "OBSCURE".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([weth, link, obscure]);
        let market_data = Arc::new(RwLock::new(market_data));

        let valid_symbols: HashSet<String> = [
            "ETHUSDT", "ETHUSDC", "ETHBTC", "LINKUSDT",
            "LINKETH",
            // OBSCURE pairs intentionally missing
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let worker = BinanceWsWorker {
            cache: Arc::new(RwLock::new(HashMap::new())),
            market_data,
            client: Client::new(),
        };

        let streams = worker
            .build_streams(&valid_symbols)
            .await;

        assert!(streams.contains(&"ethusdt@bookTicker".to_string()));
        assert!(streams.contains(&"ethusdc@bookTicker".to_string()));
        assert!(streams.contains(&"ethbtc@bookTicker".to_string()));
        assert!(streams.contains(&"linkusdt@bookTicker".to_string()));
        assert!(streams.contains(&"linketh@bookTicker".to_string()));

        // OBSCURE has no valid pairs — should not appear
        assert!(!streams
            .iter()
            .any(|s| s.contains("obscure")));
    }

    #[tokio::test]
    async fn test_build_streams_deduplicates_wrapped_tokens() {
        // WETH normalizes to ETH — should not produce duplicate streams
        let weth = Token {
            address: "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                .parse()
                .unwrap(),
            symbol: "WETH".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([weth]);
        let market_data = Arc::new(RwLock::new(market_data));

        let valid_symbols: HashSet<String> = ["ETHUSDT", "ETHUSDC"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let worker = BinanceWsWorker {
            cache: Arc::new(RwLock::new(HashMap::new())),
            market_data,
            client: Client::new(),
        };

        let streams = worker
            .build_streams(&valid_symbols)
            .await;
        assert_eq!(streams.len(), 2);
    }

    #[tokio::test]
    #[ignore] // requires network access
    async fn test_binance_ws_provider_live() {
        let weth_addr: Address = "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let usdc_addr: Address = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();

        let weth = Token {
            address: weth_addr.clone(),
            symbol: "WETH".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };
        let usdc = Token {
            address: usdc_addr.clone(),
            symbol: "USDC".to_string(),
            decimals: 6,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([weth, usdc]);
        let market_data = Arc::new(RwLock::new(market_data));

        let mut provider = BinanceWsProvider::new();
        provider.start(market_data);

        // The WebSocket needs time to connect and receive ticker data.
        // Retry a few times since the subscription and first messages take a moment.
        let one_eth = BigUint::from(10u64).pow(18);
        let mut result = Err(PriceProviderError::Unavailable("not yet".into()));
        for _ in 0..6 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            result = provider
                .get_expected_out(&weth_addr, &usdc_addr, &one_eth)
                .await;
            if result.is_ok() {
                break;
            }
        }

        let price = result.expect("should get a price from Binance WS");
        let amount_out = price.expected_amount_out().clone();

        // 1 ETH should be worth between $100 and $100,000 USDC (6 decimals)
        let min = BigUint::from(100_000_000u64); // 100 USDC
        let max = BigUint::from(100_000_000_000u64); // 100,000 USDC
        assert!(
            amount_out >= min && amount_out <= max,
            "expected amount_out in [{min}, {max}], got {amount_out}"
        );
        println!("Binance WS: 1 WETH = {} USDC (raw)", amount_out);
    }
}
