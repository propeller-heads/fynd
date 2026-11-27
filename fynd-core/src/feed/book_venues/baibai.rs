//! BaiBai reads its curves from the chain through Fynd's node, so it needs no credentials and
//! does not go through Fynd Nexus.

use tycho_simulation::{
    evm::tycho_models::Chain,
    pamm::protocols::baibai::{BaibaiFeed, BaibaiFeedBuilder},
};

use super::{stream_error, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// The only chain BaiBai is deployed on.
const BAIBAI_CHAIN: Chain = Chain::Base;

/// Rejects BaiBai on a chain other than [`BAIBAI_CHAIN`] or where tycho-execution has no executor
/// for the entry.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    if chain != BAIBAI_CHAIN {
        return Err(DataFeedError::Config(format!(
            "{} is only served on {BAIBAI_CHAIN}, but this feed runs on {chain}",
            entry.name
        )));
    }
    require_executor(chain, entry.name)
}

/// BaiBai's feed, polling `context.rpc_url`.
pub(super) fn direct_feed(
    context: &BookVenueContext,
    entry: &BookVenueEntry,
) -> Result<BaibaiFeed, DataFeedError> {
    let Some(rpc_url) = &context.rpc_url else {
        return Err(DataFeedError::Config(format!(
            "{} reads its curves from the chain and needs an RPC URL",
            entry.name
        )));
    };
    let mut builder = BaibaiFeedBuilder::new(context.book_config.clone(), rpc_url.clone());
    if entry.uses_fallback_router {
        builder = builder.with_fallback_router();
    }
    builder.build().map_err(stream_error)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rstest::rstest;
    use tycho_simulation::book::BookFeedConfig;

    use super::*;
    use crate::feed::book_venues::BookVenue;

    fn entry(name: &str) -> BookVenueEntry<'_> {
        let protocols = [name.to_string()];
        let entries = BookVenueEntry::parse_all(&protocols).unwrap();
        BookVenueEntry { name, ..entries[0] }
    }

    fn context(chain: Chain, rpc_url: Option<&str>) -> BookVenueContext {
        BookVenueContext {
            book_config: BookFeedConfig {
                chain,
                tokens: Arc::new(Default::default()),
                min_tvl_usd: 0.0,
            },
            aggregate_rfqs: false,
            nexus_client: None,
            rpc_url: rpc_url.map(str::to_string),
        }
    }

    #[rstest]
    #[case::direct("pamm:baibai", false)]
    #[case::via_fallback_router("fallback:pamm:baibai", true)]
    fn test_parse_entry(#[case] name: &str, #[case] uses_fallback_router: bool) {
        let entry = entry(name);

        assert_eq!(entry.venue, BookVenue::Baibai);
        assert_eq!(entry.uses_fallback_router, uses_fallback_router);
    }

    #[rstest]
    #[case::direct("pamm:baibai")]
    #[case::via_fallback_router("fallback:pamm:baibai")]
    fn test_check_supported_on_base(#[case] name: &str) {
        entry(name)
            .check_supported(Chain::Base)
            .unwrap();
    }

    #[test]
    fn test_check_supported_off_base() {
        let Err(DataFeedError::Config(message)) =
            entry("pamm:baibai").check_supported(Chain::Ethereum)
        else {
            panic!("BaiBai must be rejected off Base");
        };

        assert!(message.contains("pamm:baibai is only served on base"), "{message}");
    }

    #[test]
    fn test_direct_feed_without_rpc_url() {
        let Err(DataFeedError::Config(message)) =
            direct_feed(&context(Chain::Base, None), &entry("pamm:baibai"))
        else {
            panic!("BaiBai must need an RPC URL");
        };

        assert!(message.contains("needs an RPC URL"), "{message}");
    }

    #[rstest]
    #[case::direct("pamm:baibai")]
    #[case::via_fallback_router("fallback:pamm:baibai")]
    fn test_direct_feed(#[case] name: &str) {
        direct_feed(&context(Chain::Base, Some("http://127.0.0.1:1")), &entry(name))
            .unwrap_or_else(|e| panic!("{name} should open: {e}"));
    }
}
