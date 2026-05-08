//! Fynd quoter: wraps `FyndClient` and fetches one quote per audit trade.

use std::{str::FromStr, time::Duration};

use alloy::primitives::Address;
use bytes::Bytes;
use fynd_client::{
    EncodingOptions, FyndClient, FyndClientBuilder, Order, OrderSide, Quote, QuoteOptions,
    QuoteParams, QuoteStatus,
};
use num_bigint::BigUint;

use crate::types::AuditTrade;

/// Outcome of a single quote request.
#[derive(Debug)]
pub enum QuoteOutcome {
    Ok(Box<Quote>),
    NoRoute(String),
    NoEncoding,
    Error(String),
}

pub struct Quoter {
    client: FyndClient,
    slippage: f64,
    timeout_ms: u64,
}

impl Quoter {
    pub async fn connect(fynd_url: &str, rpc_url: &str, sender: Address) -> anyhow::Result<Self> {
        let client = FyndClientBuilder::new(fynd_url, rpc_url)
            .with_sender(sender)
            .build()
            .await?;
        Ok(Self { client, slippage: 0.005, timeout_ms: 5_000 })
    }

    /// Borrow the underlying `FyndClient` (used by the simulator for dry-run gas
    /// estimation via `execute_swap`).
    pub fn client(&self) -> &FyndClient {
        &self.client
    }

    /// Wait up to 30s for the solver to report healthy, so audit runs begin
    /// with a warm solver.
    pub async fn wait_healthy(&self, fynd_url: &str) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await;
            match self.client.health().await {
                Ok(h) if h.healthy() => return Ok(()),
                Ok(_) | Err(_) if tokio::time::Instant::now() >= deadline => {
                    anyhow::bail!("solver at {fynd_url} not healthy after 30s");
                }
                _ => continue,
            }
        }
    }

    pub async fn quote(&self, trade: &AuditTrade) -> QuoteOutcome {
        let order = match build_order(trade) {
            Ok(o) => o,
            Err(e) => return QuoteOutcome::Error(e),
        };
        let opts = QuoteOptions::default()
            .with_timeout_ms(self.timeout_ms)
            .with_encoding_options(EncodingOptions::new(self.slippage));

        match self
            .client
            .quote(QuoteParams::new(order, opts))
            .await
        {
            Ok(quote) => match quote.status() {
                QuoteStatus::Success => {
                    if quote.transaction().is_some() {
                        QuoteOutcome::Ok(Box::new(quote))
                    } else {
                        QuoteOutcome::NoEncoding
                    }
                }
                _ => QuoteOutcome::NoRoute(format!("status={:?}", quote.status())),
            },
            Err(e) => QuoteOutcome::Error(e.to_string()),
        }
    }
}

fn build_order(trade: &AuditTrade) -> Result<Order, String> {
    let token_in = parse_hex_bytes(&trade.token_in).map_err(|e| format!("bad token_in: {e}"))?;
    let token_out = parse_hex_bytes(&trade.token_out).map_err(|e| format!("bad token_out: {e}"))?;
    let sender = parse_hex_bytes(&trade.sender).map_err(|e| format!("bad sender: {e}"))?;
    let amount = BigUint::from_str(&trade.amount_in).map_err(|e| format!("bad amount: {e}"))?;
    Ok(Order::new(token_in, token_out, amount, OrderSide::Sell, sender, None))
}

fn parse_hex_bytes(s: &str) -> anyhow::Result<Bytes> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let raw = alloy::primitives::hex::decode(stripped)
        .map_err(|e| anyhow::anyhow!("invalid hex '{s}': {e}"))?;
    Ok(Bytes::from(raw))
}
