//! Metric's prices feed: its API key, its endpoint, and the chains Fynd serves it on.

use std::fmt;

use fynd_core::feed::book_venues::metric::METRIC_CHAINS;
use tycho_simulation::{
    evm::tycho_models::Chain,
    pamm::protocols::metric::feed::{MetricPricesFeed, MetricPricesFeedBuilder},
    snapshot_feed::errors::FeedError,
};

/// Metric's settings. Its `Debug` output says whether the key is set and never shows it.
#[derive(clap::Args, Clone, Default)]
pub struct MetricArgs {
    /// Metric API key.
    #[arg(
        id = "metric_api_key",
        long = "metric-api-key",
        env = "METRIC_API_KEY",
        hide_env_values = true
    )]
    pub api_key: Option<String>,

    /// Base URL of another Metric deployment. A blank value means Metric's public endpoint.
    #[arg(id = "metric_api_url", long = "metric-api-url", env = "METRIC_API_URL")]
    pub api_url: Option<String>,
}

impl fmt::Debug for MetricArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricArgs")
            .field("api_key", &self.api_key.is_some())
            .field("api_url", &self.api_url)
            .finish()
    }
}

/// Metric's credentials and endpoint, once set.
#[derive(Clone)]
pub struct MetricCredentials {
    api_key: String,
    base_url: Option<String>,
}

impl MetricArgs {
    /// Metric's credentials and endpoint. Trims surrounding whitespace from the endpoint and uses
    /// Metric's public endpoint when the result is blank, matching pod behavior.
    ///
    /// # Errors
    ///
    /// Returns `Err("METRIC_API_KEY")` if the API key is missing.
    pub fn credentials(&self) -> Result<MetricCredentials, &'static str> {
        let api_key = self
            .api_key
            .clone()
            .ok_or("METRIC_API_KEY")?;
        let base_url = self
            .api_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        Ok(MetricCredentials { api_key, base_url })
    }
}

/// Whether Metric supports `chain`: the chains in [`METRIC_CHAINS`].
pub fn supports_chain(chain: Chain) -> bool {
    METRIC_CHAINS.contains(&chain)
}

/// Metric's prices feed on `chain`.
///
/// # Errors
///
/// Fails when the feed cannot be built.
pub fn prices_feed(
    chain: Chain,
    credentials: &MetricCredentials,
) -> Result<MetricPricesFeed, FeedError> {
    let mut builder = MetricPricesFeedBuilder::new(chain, credentials.api_key.clone());
    if let Some(url) = credentials.base_url.clone() {
        builder = builder.base_url(url);
    }
    builder.build()
}
