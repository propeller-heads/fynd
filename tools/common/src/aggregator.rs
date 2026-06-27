//! The quote model shared by Fynd and external DEX aggregators.
//!
//! Every quote source implements [`AggregatorClient`] and returns an [`AggregatorQuote`], so the
//! audit loop and Hindsight can treat Fynd and third-party aggregators uniformly.

use std::fmt;

use async_trait::async_trait;
use serde::Serialize;

/// Quote status returned by a quote source.
///
/// Use this enum rather than comparing status strings directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregatorStatus {
    Success,
    NoAmount,
    NoRoute,
    /// The aggregator returned an HTTP error response (4xx/5xx).
    HttpError {
        code: u16,
        snippet: String,
    },
}

impl fmt::Display for AggregatorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::NoAmount => f.write_str("no_amount"),
            Self::NoRoute => f.write_str("no_route"),
            Self::HttpError { code, snippet } => write!(f, "http_{code}: {snippet}"),
        }
    }
}

impl Serialize for AggregatorStatus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// On-chain transaction calldata returned by a quote source for execution validation.
#[derive(Debug, Clone, Serialize)]
pub struct AggregatorCalldata {
    /// Router / settlement contract address (hex with 0x prefix).
    pub to: String,
    /// ABI-encoded calldata (hex with 0x prefix).
    pub data: String,
    /// Native ETH value to send with the transaction (decimal string; usually `"0"`).
    pub value: String,
}

/// A quote response from a quote source.
#[derive(Debug, Clone, Serialize)]
pub struct AggregatorQuote {
    pub status: AggregatorStatus,
    /// Output amount as a decimal string (token units).
    pub amount_out: Option<String>,
    /// Output amount net of gas costs (token units). Only populated by Fynd; `None` for all
    /// other aggregators. Used as the baseline when computing gas-adjusted bps diffs.
    pub amount_out_net_gas: Option<String>,
    /// Total estimated gas units across all route legs.
    pub gas_units: Option<u64>,
    /// DEX names used in the route (e.g. `["uniswap_v3"]`).
    pub protocols: Vec<String>,
    /// Number of independent parallel sub-routes the aggregator split the order across.
    /// `1` means a single path (no splitting). `None` when the aggregator does not expose
    /// route structure (e.g. 0x `/price`).
    pub num_splits: Option<usize>,
    /// Wall-clock time from request dispatch to response body received.
    pub response_time_ms: u64,
    /// Encoded on-chain transaction. Present only when calldata was requested and the
    /// aggregator has an encoding endpoint. `None` otherwise.
    pub calldata: Option<AggregatorCalldata>,
    /// Pool-level route: `[protocol, component_id]` pairs (Fynd only; `None` for aggregators).
    pub route: Option<Vec<[String; 2]>>,
}

impl AggregatorQuote {
    pub fn is_success(&self) -> bool {
        self.status == AggregatorStatus::Success
    }
}

/// Interface for any quote source (Fynd or an external DEX aggregator).
#[async_trait]
pub trait AggregatorClient: Send + Sync {
    /// Short label used in output (e.g. `"nordstern"`).
    fn name(&self) -> &str;

    /// Request a sell-side quote. Returns `Err` for network or parse failures; routing
    /// outcomes (no route, HTTP error from the aggregator) are encoded in the returned
    /// quote's `status` field.
    ///
    /// When `wallet` is `Some(address)`, the implementation fetches the encoded on-chain
    /// transaction (populating [`AggregatorQuote::calldata`]) and uses `address` as the
    /// sender/recipient so that balance-delta validation reads the correct account.
    /// Pass `None` when calldata is not needed.
    async fn quote(
        &self,
        token_in: &str,
        token_out: &str,
        amount: &str,
        wallet: Option<&str>,
    ) -> anyhow::Result<AggregatorQuote>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote_with_status(status: AggregatorStatus) -> AggregatorQuote {
        AggregatorQuote {
            status,
            amount_out: None,
            amount_out_net_gas: None,
            gas_units: None,
            protocols: vec![],
            num_splits: None,
            response_time_ms: 0,
            calldata: None,
            route: None,
        }
    }

    #[test]
    fn is_success_only_for_success_status() {
        assert!(quote_with_status(AggregatorStatus::Success).is_success());
        assert!(!quote_with_status(AggregatorStatus::NoAmount).is_success());
        assert!(!quote_with_status(AggregatorStatus::NoRoute).is_success());
        assert!(!quote_with_status(AggregatorStatus::HttpError {
            code: 429,
            snippet: "rate limited".to_string()
        })
        .is_success());
    }

    #[test]
    fn status_serialises_to_string() {
        assert_eq!(serde_json::to_string(&AggregatorStatus::Success).unwrap(), r#""success""#);
        assert_eq!(serde_json::to_string(&AggregatorStatus::NoRoute).unwrap(), r#""no_route""#);
        assert_eq!(
            serde_json::to_string(&AggregatorStatus::HttpError {
                code: 429,
                snippet: "rate limited".to_string()
            })
            .unwrap(),
            r#""http_429: rate limited""#
        );
    }
}
