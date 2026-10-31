//! The pod's connection to a `fynd-nexus` service.

use std::time::Duration;

use reqwest::Url;

use super::{
    components::UnhealthyPoolsPoller,
    rfq::{RfqAssembler, RfqPricesFeed},
};
use crate::feed::market_data::MarketData;

/// Errors when creating a [`NexusClient`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NexusError {
    /// The service URL is not an HTTP URL.
    #[error(
        "nexus URL '{url}' must be an http:// or https:// URL, e.g. \
         http://fynd-nexus:3000"
    )]
    InvalidUrl {
        /// The service URL supplied by the caller.
        url: String,
    },
    /// The HTTP client could not be built.
    #[error("cannot build the nexus HTTP client: {0}")]
    HttpClient(String),
}

/// Connects a pod to a `fynd-nexus` service.
#[derive(Debug, Clone)]
pub struct NexusClient {
    base_url: String,
    http: reqwest::Client,
}

impl NexusClient {
    /// A client for the service at `base_url`, such as `http://fynd-nexus:3000`.
    ///
    /// The service serves one chain; the caller points `base_url` at the one of the pod's chain.
    ///
    /// # Errors
    ///
    /// [`NexusError::InvalidUrl`] when `base_url` is not an http:// or https:// URL, and
    /// [`NexusError::HttpClient`] when the HTTP client cannot be built.
    pub fn new(base_url: &str) -> Result<Self, NexusError> {
        let valid = Url::parse(base_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host());
        if !valid {
            return Err(NexusError::InvalidUrl { url: base_url.to_string() });
        }
        let base_url = base_url.trim_end_matches('/');
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| NexusError::HttpClient(e.to_string()))?;
        Ok(Self { base_url: base_url.to_string(), http })
    }

    /// A feed of one RFQ venue's books, built by `assembler` from the prices the service serves.
    pub fn rfq_prices_feed(&self, assembler: RfqAssembler) -> RfqPricesFeed {
        RfqPricesFeed::new(&self.base_url, self.http.clone(), assembler)
    }

    /// A poller that keeps the excluded pools of `market` in step with the components the
    /// service reports as unhealthy. See [`UnhealthyPoolsPoller`] for when it excludes nothing;
    /// `max_unconfirmed_age` is how long the service may stay silent before then.
    pub fn unhealthy_pools_poller(
        &self,
        market: MarketData,
        max_unconfirmed_age: Duration,
    ) -> UnhealthyPoolsPoller {
        UnhealthyPoolsPoller::new(&self.base_url, self.http.clone(), market, max_unconfirmed_age)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::http("http://fynd-nexus:3000")]
    #[case::https("https://shared.example.com")]
    #[case::trailing_slash("http://fynd-nexus:3000/")]
    fn test_new_http_url(#[case] url: &str) {
        assert!(NexusClient::new(url).is_ok());
    }

    #[rstest]
    #[case::websocket("ws://shared:3000")]
    #[case::no_scheme("shared:3000")]
    #[case::empty("")]
    #[case::no_host("http://")]
    fn test_new_invalid_url(#[case] url: &str) {
        let error = NexusClient::new(url).unwrap_err();

        assert!(matches!(error, NexusError::InvalidUrl { .. }), "got {error}");
        assert!(
            error
                .to_string()
                .contains("http:// or https://"),
            "got {error}"
        );
    }
}
