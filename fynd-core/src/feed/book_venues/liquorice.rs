//! Liquorice connects directly because Fynd Nexus does not serve Liquorice prices.

use tycho_simulation::{
    evm::tycho_models::Chain,
    rfq::protocols::liquorice::feed::{LiquoriceFeed, LiquoriceFeedBuilder},
};

use super::{get_env, stream_error, usd_quote_tokens, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// Rejects Liquorice on a chain where tycho-execution has no executor for it.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    require_executor(chain, entry.name)
}

/// Liquorice's own connection, with `LIQUORICE_USER` and `LIQUORICE_KEY`.
pub(super) fn direct_feed(
    context: &BookVenueContext,
    entry: &BookVenueEntry,
) -> Result<LiquoriceFeed, DataFeedError> {
    let mut builder = LiquoriceFeedBuilder::new(
        context.book_config.clone(),
        usd_quote_tokens(context)?,
        get_env("LIQUORICE_USER")?,
        get_env("LIQUORICE_KEY")?,
    );
    if entry.uses_fallback_router {
        builder = builder.with_fallback_router();
    }
    builder.build().map_err(stream_error)
}
