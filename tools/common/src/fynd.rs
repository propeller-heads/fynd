//! [`FyndAggregator`] — wraps [`FyndClient`] so it produces an [`AggregatorQuote`] for a given
//! `(token_in, token_out, amount_in)`, the same shape external aggregators return.

use std::{str::FromStr, sync::Arc, time::Instant};

use alloy::hex;
use async_trait::async_trait;
use bytes::Bytes;
use fynd_client::{
    EncodingOptions, FyndClient, Order, OrderSide, QuoteOptions, QuoteParams, QuoteStatus,
};
use num_bigint::BigUint;

use crate::aggregator::{AggregatorCalldata, AggregatorClient, AggregatorQuote, AggregatorStatus};

/// Some trade datasets use 0xeeee…eeee as a sentinel for native ETH; Fynd expects ZERO_ADDRESS.
const ETH_SENTINEL: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// Wraps [`FyndClient`] so it participates in the same quote loop as external aggregators.
///
/// Kept separate from any solver `FyndClient` used for health-checks and block-waiting so those
/// concerns don't bleed into the aggregator abstraction.
pub struct FyndAggregator {
    client: Arc<FyndClient>,
    timeout_ms: u64,
    encoding_slippage: f64,
}

impl FyndAggregator {
    /// `encoding_slippage` is the fractional slippage (e.g. `0.005` for 50 bps) passed to
    /// [`EncodingOptions`] when a `wallet` is supplied so calldata is produced.
    pub fn new(client: Arc<FyndClient>, timeout_ms: u64, encoding_slippage: f64) -> Self {
        Self { client, timeout_ms, encoding_slippage }
    }
}

#[async_trait]
impl AggregatorClient for FyndAggregator {
    fn name(&self) -> &str {
        "fynd"
    }

    async fn quote(
        &self,
        token_in: &str,
        token_out: &str,
        amount: &str,
        wallet: Option<&str>,
    ) -> anyhow::Result<AggregatorQuote> {
        let start = Instant::now();

        let params = make_quote_params(
            token_in,
            token_out,
            amount,
            self.timeout_ms,
            wallet,
            wallet
                .is_some()
                .then(|| EncodingOptions::new(self.encoding_slippage)),
        )?;

        let q = self
            .client
            .quote(params)
            .await
            .map_err(|e| anyhow::anyhow!("Fynd quote failed: {e}"))?;

        let response_time_ms = start.elapsed().as_millis() as u64;
        let success = q.status() == QuoteStatus::Success;

        let protocols = q
            .route()
            .map(|r| {
                r.swaps()
                    .iter()
                    .map(|s| s.protocol().to_string())
                    .collect()
            })
            .unwrap_or_default();

        let route = q.route().map(|r| {
            r.swaps()
                .iter()
                .map(|s| [s.protocol().to_string(), s.component_id().to_string()])
                .collect()
        });

        let calldata = success
            .then(|| q.transaction())
            .flatten()
            .and_then(|tx| {
                if tx.to().len() != 20 {
                    return None;
                }
                Some(AggregatorCalldata {
                    to: format!("0x{}", hex::encode(tx.to())),
                    data: format!("0x{}", hex::encode(tx.data())),
                    value: tx.value().to_string(),
                })
            });

        let nonzero_str = |s: String| (s != "0").then_some(s);

        Ok(AggregatorQuote {
            status: fynd_status_to_agg(q.status()),
            amount_out: success
                .then(|| nonzero_str(q.amount_out().to_string()))
                .flatten(),
            amount_out_net_gas: success
                .then(|| nonzero_str(q.amount_out_net_gas().to_string()))
                .flatten(),
            gas_units: q
                .gas_estimate()
                .to_string()
                .parse::<u64>()
                .ok()
                .filter(|&v| v > 0),
            protocols,
            num_splits: None,
            response_time_ms,
            calldata,
            route,
        })
    }
}

/// Map a Fynd [`QuoteStatus`] onto the shared [`AggregatorStatus`].
pub fn fynd_status_to_agg(status: QuoteStatus) -> AggregatorStatus {
    match status {
        QuoteStatus::Success => AggregatorStatus::Success,
        QuoteStatus::NoRouteFound | QuoteStatus::InsufficientLiquidity => AggregatorStatus::NoRoute,
        QuoteStatus::Timeout | QuoteStatus::NotReady | QuoteStatus::PriceCheckFailed => {
            AggregatorStatus::NoAmount
        }
    }
}

/// Normalise a native-ETH sentinel address to the zero address Fynd expects; pass through
/// everything else unchanged (case-insensitive on the sentinel).
pub fn fynd_addr(addr: &str) -> &str {
    if addr.eq_ignore_ascii_case(ETH_SENTINEL) {
        ZERO_ADDRESS
    } else {
        addr
    }
}

/// Build [`QuoteParams`] for a sell-side order from hex token addresses and a decimal amount.
///
/// `sender_hex` defaults to `0x…01` when absent. When `encoding` is supplied the quote requests
/// on-chain calldata via [`EncodingOptions`].
pub fn make_quote_params(
    token_in: &str,
    token_out: &str,
    amount: &str,
    timeout_ms: u64,
    sender_hex: Option<&str>,
    encoding: Option<EncodingOptions>,
) -> anyhow::Result<QuoteParams> {
    let sender = parse_addr(sender_hex.unwrap_or("0x0000000000000000000000000000000000000001"))?;
    let order = Order::new(
        parse_addr(fynd_addr(token_in))?,
        parse_addr(fynd_addr(token_out))?,
        BigUint::from_str(amount).unwrap_or_default(),
        OrderSide::Sell,
        sender,
        None,
    );
    let opts = QuoteOptions::default().with_timeout_ms(timeout_ms);
    let opts = match encoding {
        Some(enc) => opts.with_encoding_options(enc),
        None => opts,
    };
    Ok(QuoteParams::new(order, opts))
}

/// Decode a 0x-prefixed (or bare) hex address into raw bytes.
pub fn parse_addr(hex_str: &str) -> anyhow::Result<Bytes> {
    let stripped = hex_str
        .strip_prefix("0x")
        .unwrap_or(hex_str);
    hex::decode(stripped)
        .map(Bytes::from)
        .map_err(|_| anyhow::anyhow!("bad address '{hex_str}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fynd_addr_normalises_eth_sentinel() {
        assert_eq!(fynd_addr(ETH_SENTINEL), ZERO_ADDRESS);
        // Case-insensitive on the sentinel.
        assert_eq!(fynd_addr("0xEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE"), ZERO_ADDRESS);
    }

    #[test]
    fn fynd_addr_passes_through_normal_address() {
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        assert_eq!(fynd_addr(usdc), usdc);
    }

    #[test]
    fn parse_addr_accepts_with_and_without_prefix() {
        let with = parse_addr("0x0000000000000000000000000000000000000001").unwrap();
        let without = parse_addr("0000000000000000000000000000000000000001").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.len(), 20);
    }

    #[test]
    fn parse_addr_rejects_non_hex() {
        assert!(parse_addr("0xzzzz").is_err());
    }

    #[test]
    fn make_quote_params_builds_for_valid_inputs() {
        // The sentinel token_in exercises the fynd_addr normalisation path inside the builder.
        assert!(make_quote_params(
            ETH_SENTINEL,
            "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "1000000000000000000",
            5_000,
            None,
            None,
        )
        .is_ok());
    }

    #[test]
    fn make_quote_params_bad_address_errors() {
        assert!(make_quote_params("0xnothex", "0xalso_bad", "1", 1_000, None, None).is_err());
    }
}
