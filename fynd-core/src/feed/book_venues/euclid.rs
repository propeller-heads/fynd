//! Euclid connects directly because Fynd Nexus does not serve Euclid prices. Its gateway is
//! public, so the venue reads no credentials.

use tycho_simulation::{
    evm::tycho_models::Chain,
    rfq::protocols::euclid::feed::{EuclidFeed, EuclidFeedBuilder},
};

use super::{stream_error, usd_quote_tokens, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// Rejects Euclid on a chain where tycho-execution has no executor for it.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    require_executor(chain, entry.name)
}

/// Euclid's own connection, as one component for all its pairs with `--aggregate-rfqs`.
pub(super) fn direct_feed(context: &BookVenueContext) -> Result<EuclidFeed, DataFeedError> {
    let mut builder =
        EuclidFeedBuilder::new(context.book_config.clone(), usd_quote_tokens(context)?);
    if context.aggregate_rfqs {
        builder = builder.all_pairs();
    }
    builder.build().map_err(stream_error)
}
