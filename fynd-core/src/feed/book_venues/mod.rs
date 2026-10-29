//! The book venues Fynd can run: the `rfq:` entries of `--protocols`.
//!
//! Each venue has a module of its own with its environment variables, its checks, its book
//! settings and its own connection. This file holds the two places that choose between venues:
//! [`BookVenueEntry::check_supported`] and [`open_book_feed`].

mod bebop;
mod hashflow;
mod liquorice;
/// Metric, and the chains it is served on.
pub mod metric;
mod native;

use std::{collections::HashSet, future::Future, sync::Arc};

use futures::FutureExt as _;
use tracing::warn;
use tycho_simulation::{
    book::{quote_tokens::usd_stablecoins_for_chain, BookFeedConfig, BookSnapshot, ReceivedAt},
    pamm::protocols::metric::{self as metric_protocol, feed::MetricFeed},
    rfq::protocols::{
        bebop::{self as bebop_protocol, feed::BebopFeed},
        hashflow::{self as hashflow_protocol, feed::HashflowFeed},
        liquorice::{self as liquorice_protocol, feed::LiquoriceFeed},
        native::{self as native_protocol, feed::NativeFeed},
    },
    snapshot_feed::{errors::FeedError, Publisher, SnapshotFeed},
    tycho_common::models::Chain,
    tycho_core::Bytes,
};

use super::DataFeedError;
use crate::{
    feed::protocol_registry::parse_rfq_entries,
    nexus::{
        client::NexusClient,
        rfq::{RfqAssembler, RfqPricesFeed},
    },
};

/// A market-maker venue whose books Fynd builds from the venue's own prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BookVenue {
    Bebop,
    Hashflow,
    Metric,
    Native,
    Liquorice,
}

impl BookVenue {
    /// The venue behind an `rfq:<venue>` protocol system, or `None` when Fynd runs no such venue.
    fn from_protocol_system(protocol_system: &str) -> Option<Self> {
        match protocol_system {
            bebop_protocol::PROTOCOL_SYSTEM => Some(Self::Bebop),
            hashflow_protocol::PROTOCOL_SYSTEM => Some(Self::Hashflow),
            metric_protocol::PROTOCOL_SYSTEM => Some(Self::Metric),
            native_protocol::PROTOCOL_SYSTEM => Some(Self::Native),
            liquorice_protocol::PROTOCOL_SYSTEM => Some(Self::Liquorice),
            _ => None,
        }
    }
}

/// One `rfq:` or `fallback:rfq:` entry of `--protocols`, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BookVenueEntry<'a> {
    /// The entry as written. It labels the venue's components.
    pub(crate) name: &'a str,
    /// The venue the entry names.
    pub(crate) venue: BookVenue,
    /// Whether the entry has the `fallback:` prefix, so its legs execute through the venue's
    /// fallback router.
    pub(crate) uses_fallback_router: bool,
}

impl<'a> BookVenueEntry<'a> {
    /// Parses the book venues named in `protocols`. Warns and skips entries with unknown venues.
    ///
    /// # Errors
    ///
    /// [`DataFeedError::Config`] when a venue is named both with and without the `fallback:`
    /// prefix.
    pub(crate) fn parse_all(protocols: &'a [String]) -> Result<Vec<Self>, DataFeedError> {
        let mut entries = Vec::new();
        for (name, rfq_entry) in parse_rfq_entries(protocols)? {
            let Some(venue) = BookVenue::from_protocol_system(rfq_entry.protocol()) else {
                warn!("Skipping unknown book protocol system: {}", rfq_entry.protocol());
                continue;
            };
            entries.push(Self {
                name,
                venue,
                uses_fallback_router: rfq_entry.uses_fallback_router(),
            });
        }
        Ok(entries)
    }

    /// Rejects an entry whose venue cannot run on `chain`, so that no feed starts for a venue
    /// whose books Fynd could not encode.
    ///
    /// # Errors
    ///
    /// [`DataFeedError::Config`] when the venue is not served on `chain` or has no executor there.
    pub(crate) fn check_supported(&self, chain: Chain) -> Result<(), DataFeedError> {
        match self.venue {
            BookVenue::Bebop => bebop::check_supported(chain, self),
            BookVenue::Hashflow => hashflow::check_supported(chain, self),
            BookVenue::Metric => metric::check_supported(chain, self),
            BookVenue::Native => native::check_supported(chain, self),
            BookVenue::Liquorice => liquorice::check_supported(chain, self),
        }
    }
}

/// The settings every venue's feed is built with: the book settings, the `--aggregate-rfqs`
/// layout and the optional nexus client.
pub(crate) struct BookVenueContext {
    /// The chain, tokens and TVL floor the books are built from.
    pub(crate) book_config: BookFeedConfig,
    /// Whether Bebop and Hashflow stream as one component for all their pairs.
    pub(crate) aggregate_rfqs: bool,
    /// Optional client for shared Bebop, Hashflow and Metric prices. Without this client, each
    /// feed connects directly to its venue.
    pub(crate) nexus: Option<NexusClient>,
}

/// The feed of one book venue that [`BookStream`] runs: the venue's own connection, or `SharedRfq`
/// for prices polled from Fynd Nexus.
///
/// [`BookStream`]: super::book_stream::BookStream
pub(crate) enum BookVenueFeed {
    Bebop(BebopFeed),
    Hashflow(HashflowFeed),
    Metric(MetricFeed),
    Native(NativeFeed),
    Liquorice(LiquoriceFeed),
    SharedRfq(RfqPricesFeed),
}

impl SnapshotFeed for BookVenueFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        match self {
            Self::Bebop(feed) => feed.run(publisher).boxed(),
            Self::Hashflow(feed) => feed.run(publisher).boxed(),
            Self::Metric(feed) => feed.run(publisher).boxed(),
            Self::Native(feed) => feed.run(publisher).boxed(),
            Self::Liquorice(feed) => feed.run(publisher).boxed(),
            Self::SharedRfq(feed) => feed.run(publisher).boxed(),
        }
    }
}

/// Builds the feed that serves `entry`, reading the venue's credentials from the environment.
///
/// With a nexus client in `context`, Bebop, Hashflow and Metric build their books from the
/// service's prices; Metric then needs no API key. Native and Liquorice always connect to the
/// venue.
///
/// # Errors
///
/// [`DataFeedError::Config`] when a credential is missing or the chain has no USD stablecoins to
/// price the venue's books against, and [`DataFeedError::StreamError`] when the venue's feed
/// cannot be built.
pub(crate) fn open_book_feed(
    entry: &BookVenueEntry,
    context: &BookVenueContext,
) -> Result<BookVenueFeed, DataFeedError> {
    Ok(match entry.venue {
        BookVenue::Bebop => {
            let env = bebop::BebopEnv::read()?;
            let assembler = bebop::assembler(context, entry, &env)?;
            match &context.nexus {
                Some(client) => {
                    BookVenueFeed::SharedRfq(client.rfq_prices_feed(RfqAssembler::Bebop(assembler)))
                }
                None => BookVenueFeed::Bebop(bebop::direct_feed(context, &env, assembler)?),
            }
        }
        BookVenue::Hashflow => {
            let env = hashflow::HashflowEnv::read()?;
            let assembler = hashflow::assembler(context, entry, &env)?;
            match &context.nexus {
                Some(client) => BookVenueFeed::SharedRfq(
                    client.rfq_prices_feed(RfqAssembler::Hashflow(assembler)),
                ),
                None => BookVenueFeed::Hashflow(hashflow::direct_feed(context, &env, assembler)?),
            }
        }
        BookVenue::Metric => {
            let assembler = metric::assembler(context, entry);
            match &context.nexus {
                Some(client) => BookVenueFeed::SharedRfq(
                    client.rfq_prices_feed(RfqAssembler::Metric(assembler)),
                ),
                None => {
                    let env = metric::MetricEnv::read()?;
                    BookVenueFeed::Metric(metric::direct_feed(context, &env, assembler)?)
                }
            }
        }
        BookVenue::Native => BookVenueFeed::Native(native::direct_feed(context, entry)?),
        BookVenue::Liquorice => BookVenueFeed::Liquorice(liquorice::direct_feed(context, entry)?),
    })
}

/// The chain's USD stablecoins, which Bebop, Hashflow, Native and Liquorice price their books'
/// TVL in. Metric reports each pool's USD TVL itself, so only the others need a chain that has a
/// curated set.
fn usd_quote_tokens(context: &BookVenueContext) -> Result<Arc<HashSet<Bytes>>, DataFeedError> {
    let chain = context.book_config.chain;
    usd_stablecoins_for_chain(chain)
        .map(Arc::new)
        .ok_or_else(|| {
            DataFeedError::Config(format!(
                "book feeds price their books in USD stablecoins, and none are known for {chain}"
            ))
        })
}

fn get_env(var: &str) -> Result<String, DataFeedError> {
    std::env::var(var).map_err(|_| DataFeedError::Config(format!("{var} env var not set")))
}

fn stream_error(error: FeedError) -> DataFeedError {
    DataFeedError::StreamError(error.to_string())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn protocols(entries: &[&str]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| (*entry).to_string())
            .collect()
    }

    #[rstest]
    #[case::bebop("rfq:bebop", BookVenue::Bebop)]
    #[case::hashflow("rfq:hashflow", BookVenue::Hashflow)]
    #[case::metric("rfq:metric", BookVenue::Metric)]
    #[case::native("rfq:native", BookVenue::Native)]
    #[case::liquorice("rfq:liquorice", BookVenue::Liquorice)]
    fn test_parse_all_direct_entry(#[case] name: &str, #[case] venue: BookVenue) {
        let protocols = protocols(&[name]);

        let entries = BookVenueEntry::parse_all(&protocols).unwrap();

        assert_eq!(entries, [BookVenueEntry { name, venue, uses_fallback_router: false }]);
    }

    #[test]
    fn test_parse_all_fallback_entry() {
        let protocols = protocols(&["fallback:rfq:metric"]);

        let entries = BookVenueEntry::parse_all(&protocols).unwrap();

        assert_eq!(
            entries,
            [BookVenueEntry {
                name: "fallback:rfq:metric",
                venue: BookVenue::Metric,
                uses_fallback_router: true,
            }]
        );
    }

    #[test]
    fn test_parse_all_skips_unknown_venues_and_other_entries() {
        let protocols =
            protocols(&["uniswap_v3", "rfq:unknown", "rfq:bebop", "fallback:fermiswap"]);

        let entries = BookVenueEntry::parse_all(&protocols).unwrap();

        let names: Vec<_> = entries
            .iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["rfq:bebop"]);
    }

    #[test]
    fn test_parse_all_direct_and_fallback_entries_for_one_venue() {
        let protocols = protocols(&["rfq:bebop", "fallback:rfq:bebop"]);

        let Err(DataFeedError::Config(message)) = BookVenueEntry::parse_all(&protocols) else {
            panic!("both forms of one venue must be rejected");
        };

        assert!(message.contains("both with and without the 'fallback:' prefix"), "{message}");
    }

    #[rstest]
    #[case::metric_chain_list("rfq:metric", Chain::Unichain, "only served on ethereum, base")]
    #[case::bebop_executor("rfq:bebop", Chain::Bsc, "rfq:bebop has no executor on bsc")]
    #[case::metric_executor("rfq:metric", Chain::Bsc, "rfq:metric has no executor on bsc")]
    #[case::native_executor("rfq:native", Chain::Polygon, "rfq:native has no executor on polygon")]
    fn test_check_supported_unsupported_chain(
        #[case] name: &str,
        #[case] chain: Chain,
        #[case] expected: &str,
    ) {
        let protocols = protocols(&[name]);
        let entries = BookVenueEntry::parse_all(&protocols).unwrap();

        let Err(DataFeedError::Config(message)) = entries[0].check_supported(chain) else {
            panic!("{name} must be rejected on {chain}");
        };

        assert!(message.contains(expected), "{message}");
    }

    #[rstest]
    #[case::bebop("rfq:bebop", Chain::Ethereum)]
    #[case::metric_via_fallback_router("fallback:rfq:metric", Chain::Ethereum)]
    #[case::hashflow("rfq:hashflow", Chain::Ethereum)]
    fn test_check_supported_supported_chain(#[case] name: &str, #[case] chain: Chain) {
        let protocols = protocols(&[name]);
        let entries = BookVenueEntry::parse_all(&protocols).unwrap();

        entries[0]
            .check_supported(chain)
            .unwrap();
    }
}
