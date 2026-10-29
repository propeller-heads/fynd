//! Metric: its book settings and own price polling.

use itertools::Itertools;
use tycho_simulation::{
    evm::tycho_models::Chain,
    pamm::protocols::metric::{
        feed::{MetricBookAssemblerBuilder, MetricFeed, MetricFeedBuilder},
        MetricBookAssembler,
    },
};

use super::{get_env, stream_error, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// The chains Fynd serves Metric on through its book feed.
///
/// An `rfq:metric` or `fallback:rfq:metric` entry on any other chain is rejected at registration.
/// Each entry must also have its own executor in tycho-execution's `executor_addresses.json`, or
/// its legs would price from the book feed and then fail to encode: Ethereum and BSC have only the
/// fallback executor.
pub const METRIC_CHAINS: &[Chain] =
    &[Chain::Ethereum, Chain::Base, Chain::Robinhood, Chain::Bsc, Chain::Arbitrum, Chain::Polygon];

/// Rejects Metric on a chain outside [`METRIC_CHAINS`] or where tycho-execution has no executor
/// for the entry.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    if !METRIC_CHAINS.contains(&chain) {
        return Err(DataFeedError::Config(format!(
            "{} is only served on {}, but this feed runs on {chain}",
            entry.name,
            METRIC_CHAINS
                .iter()
                .map(|chain| chain.to_string())
                .collect_vec()
                .join(", ")
        )));
    }
    require_executor(chain, entry.name)
}

/// Metric's own pricing settings from the environment: `METRIC_API_KEY`, and `METRIC_API_URL`
/// when set to a non-empty value.
pub(super) struct MetricEnv {
    api_key: String,
    base_url: Option<String>,
}

impl MetricEnv {
    /// Reads Metric's settings.
    ///
    /// # Errors
    ///
    /// [`DataFeedError::Config`] when `METRIC_API_KEY` is unset.
    pub(super) fn read() -> Result<Self, DataFeedError> {
        Ok(Self {
            api_key: get_env("METRIC_API_KEY")?,
            base_url: trimmed_non_empty(std::env::var("METRIC_API_URL").ok()),
        })
    }
}

/// Builds Metric's book assembler from the tokens, TVL floor and fallback router. It needs no API
/// key, since the assembler makes no call to Metric, and no stablecoins, since Metric reports each
/// pool's USD TVL itself.
pub(super) fn assembler(context: &BookVenueContext, entry: &BookVenueEntry) -> MetricBookAssembler {
    let builder = MetricBookAssemblerBuilder::new(context.book_config.clone());
    if entry.uses_fallback_router {
        builder.with_fallback_router().build()
    } else {
        builder.build()
    }
}

/// Polls Metric directly and uses `assembler` to build its books.
pub(super) fn direct_feed(
    context: &BookVenueContext,
    env: &MetricEnv,
    assembler: MetricBookAssembler,
) -> Result<MetricFeed, DataFeedError> {
    let mut builder = MetricFeedBuilder::new(context.book_config.clone(), env.api_key.clone())
        .with_assembler(assembler);
    if let Some(base_url) = &env.base_url {
        builder = builder.base_url(base_url.clone());
    }
    builder.build().map_err(stream_error)
}

/// Trims an optional environment value and treats an empty one as absent.
fn trimmed_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}
