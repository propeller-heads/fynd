//! Historical reference prices for the markout benchmark.
//!
//! `fynd-core`'s `price_guard` providers are deliberately not used here: they stream or poll the
//! *current* price to validate a live quote, and keep no history. A markout needs the price at a
//! past timestamp, so this module pulls one-minute klines from Binance's public REST endpoint
//! instead. No API key is required.

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::debug;

/// Binance returns at most this many klines per request.
const MAX_KLINES_PER_REQUEST: u64 = 1_000;

const SECONDS_PER_MINUTE: u64 = 60;

/// One-minute closing prices, ordered by time, quoted in token1 per token0.
#[derive(Debug, Clone, Default)]
pub struct PriceSeries {
    /// `(minute open time in seconds, close price)`, ascending.
    candles: Vec<(u64, f64)>,
}

impl PriceSeries {
    /// Downloads one-minute closes covering `[from, to]` inclusive, in Unix seconds.
    pub async fn fetch(symbol: &str, from: u64, to: u64) -> Result<Self> {
        let client = reqwest::Client::new();
        let mut candles: Vec<(u64, f64)> = Vec::new();
        let mut cursor = from;
        while cursor <= to {
            let url = format!(
                "https://api.binance.com/api/v3/klines?symbol={symbol}&interval=1m\
                 &startTime={}&limit={MAX_KLINES_PER_REQUEST}",
                cursor * 1_000
            );
            let batch: Vec<Value> = client
                .get(&url)
                .send()
                .await
                .context("binance klines request failed")?
                .error_for_status()
                .context("binance klines returned an error status")?
                .json()
                .await
                .context("binance klines response was not JSON")?;
            if batch.is_empty() {
                break;
            }
            for row in &batch {
                let open_ms = row[0]
                    .as_u64()
                    .context("kline open time was not a number")?;
                let close = row[4]
                    .as_str()
                    .context("kline close was not a string")?
                    .parse::<f64>()
                    .context("kline close was not a number")?;
                candles.push((open_ms / 1_000, close));
            }
            let last_open = candles
                .last()
                .map(|(open, _)| *open)
                .unwrap_or(cursor);
            cursor = last_open + SECONDS_PER_MINUTE;
        }
        candles.sort_unstable_by_key(|(open, _)| *open);
        candles.dedup_by_key(|(open, _)| *open);
        debug!("fetched {} reference candles for {symbol}", candles.len());
        Ok(Self { candles })
    }

    /// Returns the close of the minute containing `timestamp`.
    ///
    /// Returns `None` when `timestamp` falls outside the downloaded window on either side. Running
    /// off the end matters most: a markout horizon that has not elapsed yet must drop out of the
    /// report rather than silently reuse the newest price.
    pub fn at(&self, timestamp: u64) -> Option<f64> {
        let index = self
            .candles
            .partition_point(|(open, _)| *open <= timestamp);
        if index == 0 {
            return None;
        }
        let (open, close) = self.candles[index - 1];
        (timestamp < open + SECONDS_PER_MINUTE).then_some(close)
    }

    /// Number of candles held.
    pub fn len(&self) -> usize {
        self.candles.len()
    }

    /// Builds a series directly from `(minute open, close)` pairs. Test helper.
    #[cfg(test)]
    pub fn from_candles(candles: Vec<(u64, f64)>) -> Self {
        Self { candles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series() -> PriceSeries {
        PriceSeries::from_candles(vec![(60, 100.0), (120, 101.0), (180, 102.0)])
    }

    #[test]
    fn returns_the_close_of_the_containing_minute() {
        assert_eq!(series().at(150), Some(101.0));
    }

    #[test]
    fn returns_the_close_on_an_exact_minute_boundary() {
        assert_eq!(series().at(120), Some(101.0));
    }

    #[test]
    fn returns_the_close_at_the_end_of_the_last_minute() {
        assert_eq!(series().at(239), Some(102.0));
    }

    #[test]
    fn returns_none_past_the_end_of_the_series() {
        // A horizon that has not elapsed yet must drop out rather than reuse the newest close.
        assert_eq!(series().at(240), None);
    }

    #[test]
    fn returns_none_before_the_series_starts() {
        assert_eq!(series().at(59), None);
    }

    #[test]
    fn returns_none_when_empty() {
        assert_eq!(PriceSeries::default().at(120), None);
    }
}
