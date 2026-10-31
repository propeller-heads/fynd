//! Native connects directly because Fynd Nexus does not serve Native prices.

use tycho_simulation::{
    evm::tycho_models::Chain,
    rfq::protocols::native::feed::{NativeFeed, NativeFeedBuilder},
};

use super::{get_env, stream_error, usd_quote_tokens, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// Rejects Native on a chain where tycho-execution has no executor for it.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    require_executor(chain, entry.name)
}

/// Native's own connection, with `NATIVE_API_KEY`.
pub(super) fn direct_feed(
    context: &BookVenueContext,
    entry: &BookVenueEntry,
) -> Result<NativeFeed, DataFeedError> {
    let mut builder = NativeFeedBuilder::new(
        context.book_config.clone(),
        usd_quote_tokens(context)?,
        get_env("NATIVE_API_KEY")?,
    );
    if entry.uses_fallback_router {
        builder = builder.with_fallback_router();
    }
    builder.build().map_err(stream_error)
}
