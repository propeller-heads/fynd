//! Book feeds: market-maker venues that publish price levels over their own connection.
//!
//! Each venue publishes a complete set of books — one per traded pair — and republishes the whole
//! set whenever anything in it changes. tycho-simulation exposes that as a watch channel per
//! venue, so the newest set is always the one a reader sees and a reader that falls behind skips
//! the sets in between. What arrives is a state of the world, not a change to it, so
//! [`BookStream`] keeps the ids each venue last published and derives the components to add and to
//! drop from the difference.

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
};

use alloy::primitives::Address;
use futures::StreamExt as _;
use itertools::Itertools;
use tracing::{debug, info, warn};
use tycho_simulation::{
    book::{
        quote_tokens::usd_stablecoins_for_chain, BookFeedConfig, BookFeedEvent, BookFeedStreams,
        BookSnapshot, ReceivedAt,
    },
    evm::tycho_models::Chain,
    protocol::models::ProtocolComponent,
    rfq::protocols::{
        bebop::{self, feed::BebopFeedBuilder},
        hashflow::{self, feed::HashflowFeedBuilder},
    },
    snapshot_feed::{errors::FeedError, SnapshotFeed, SnapshotFeedOutcome},
    tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim},
    tycho_core::Bytes,
};

use super::{when_configured, DataFeedError};
use crate::{feed::protocol_registry::BOOK_PREFIX, types::ComponentId};

/// The market changes one book feed reports: the components it started serving, the ones it
/// stopped serving, and the current state of everything it serves.
pub struct BookUpdate {
    /// The venue that published, e.g. `book:bebop`. Also the protocol system of its components.
    pub protocol_system: String,
    /// Components the venue serves that the market does not have yet.
    pub added: Vec<ProtocolComponent>,
    /// Components the market has that the venue no longer serves.
    pub removed: Vec<ComponentId>,
    /// The ids whose state the venue refreshed, the added ones excluded.
    pub updated: Vec<ComponentId>,
    /// The state of every book the venue currently serves, the added ones included.
    pub states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
}

impl BookUpdate {
    /// Whether the update leaves the market exactly as it is.
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.states.is_empty()
    }
}

/// The venues a `--protocols` list asked for with `book:` entries, as a stream of the changes
/// they make to the market.
///
/// The venues underneath are latest-value feeds — a reader that falls behind skips to the newest
/// book set — but what this yields are ordered deltas against the ids it last served, so its
/// consumer must apply every one.
pub struct BookStream {
    /// The venues' feeds, merged: each publication, withdrawal and ending arrives here.
    feeds: BookFeedStreams,
    /// The component ids each venue's last applied book set contained.
    served: HashMap<String, HashSet<ComponentId>>,
}

impl BookStream {
    /// Subscribes to every `book:` entry in `protocols`, or `None` when it names none.
    ///
    /// The venues price their books' TVL in USD stablecoins, so `min_tvl_usd` is a USD amount —
    /// unlike the native-token threshold the Tycho component filter takes.
    ///
    /// # Errors
    ///
    /// Returns [`DataFeedError::Config`] if a venue's credentials are missing from the
    /// environment, or if the chain has no curated USD stablecoin set to price books against.
    pub fn open(
        chain: Chain,
        min_tvl_usd: f64,
        protocols: &[String],
        tokens: HashMap<Bytes, Token>,
    ) -> Result<Option<Self>, DataFeedError> {
        let venues = protocols
            .iter()
            .filter(|protocol| protocol.starts_with(BOOK_PREFIX))
            .collect_vec();
        if venues.is_empty() {
            return Ok(None);
        }

        let usd_quote_tokens = Arc::new(usd_stablecoins_for_chain(chain).ok_or_else(|| {
            DataFeedError::Config(format!(
                "book feeds price their books in USD stablecoins, and none are known for {chain}"
            ))
        })?);
        let book_config = BookFeedConfig { chain, tokens: Arc::new(tokens), min_tvl_usd };

        let mut books = Self { feeds: BookFeedStreams::new(), served: HashMap::new() };
        for venue in venues {
            match venue.as_str() {
                bebop::PROTOCOL_SYSTEM => {
                    let mut builder = BebopFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(&usd_quote_tokens),
                        get_env("BEBOP_KEY")?,
                    );
                    if let Some(address) = optional_address_env("BEBOP_ORIGIN_ADDRESS")? {
                        builder = builder.origin_address(address);
                    }
                    if let Some(target) = optional_address_env("BEBOP_ORIGIN_TARGET")? {
                        builder = builder.origin_target(target);
                    }
                    if let Ok(source) = std::env::var("BEBOP_ORIGIN_SOURCE") {
                        builder = builder.origin_source(source);
                    }
                    let feed = builder
                        .build()
                        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(bebop::PROTOCOL_SYSTEM, feed);
                }
                hashflow::PROTOCOL_SYSTEM => {
                    let feed = HashflowFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(&usd_quote_tokens),
                        get_env("HASHFLOW_USER")?,
                        get_env("HASHFLOW_KEY")?,
                    )
                    .build()
                    .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(hashflow::PROTOCOL_SYSTEM, feed);
                }
                unknown => warn!("Skipping unknown book protocol system: {unknown}"),
            }
        }
        Ok((!books.feeds.is_empty()).then_some(books))
    }

    /// Opens one venue's channel and spawns the task that keeps it fresh.
    fn add_feed<F>(&mut self, protocol_system: &str, feed: F)
    where
        F: SnapshotFeed<Snapshot = BookSnapshot<ReceivedAt>, Error = FeedError>,
    {
        info!("Adding {protocol_system} book feed...");
        if self
            .feeds
            .add(protocol_system, feed)
            .is_err()
        {
            warn!("{protocol_system} book feed is already running; ignoring the second one");
        }
    }

    /// Waits for the next update any venue has for the market: a published set, whose every state
    /// the market takes even when no component came or went, or a withdrawal that removes what a
    /// venue served. Updates that would change nothing — a venue reporting nothing servable before
    /// its first set, or publishing an empty one while serving nothing — are skipped here.
    ///
    /// # Errors
    ///
    /// Returns [`DataFeedError::StreamError`] once every venue has given up, matching what the
    /// Tycho stream ending does: the feed stops and the process is restarted.
    async fn next(&mut self) -> Result<BookUpdate, DataFeedError> {
        loop {
            let Some((provider, event)) = self.feeds.next().await else {
                return Err(DataFeedError::StreamError("every book feed gave up".to_string()));
            };

            let update = match event {
                BookFeedEvent::Published(snapshot) => self.apply(provider, snapshot),
                BookFeedEvent::Withdrawn => {
                    let withdrawn = self.withdraw(&provider);
                    // A venue that withdraws while serving nothing leaves the market nothing to
                    // clean up; only a venue that had books makes this worth a warning.
                    if !withdrawn.removed.is_empty() {
                        warn!(
                            "Book feed {provider} has no servable books; dropping its {} components",
                            withdrawn.removed.len()
                        );
                    }
                    withdrawn
                }
                BookFeedEvent::Ended(SnapshotFeedOutcome::Panicked(error)) => {
                    return Err(DataFeedError::StreamError(format!(
                        "book feed {provider} panicked: {error}"
                    )))
                }
                // Both of these end the venue for this process; the feed withdraws on its way
                // out, so its books are usually gone already and this covers the one that ended
                // before publishing anything.
                BookFeedEvent::Ended(SnapshotFeedOutcome::Failed(error)) => {
                    warn!("Book feed {provider} gave up: {error}");
                    self.withdraw(&provider)
                }
                BookFeedEvent::Ended(SnapshotFeedOutcome::RanOut) => {
                    warn!("Book feed {provider} has nothing left to serve");
                    self.withdraw(&provider)
                }
            };

            if !update.is_empty() {
                return Ok(update);
            }
        }
    }

    /// Turns one venue's published book set into the change it makes to the market.
    fn apply(
        &mut self,
        protocol_system: String,
        BookSnapshot { anchor, books }: BookSnapshot<ReceivedAt>,
    ) -> BookUpdate {
        debug!(
            protocol_system,
            books = books.len(),
            received_at = %anchor.0,
            "received book set"
        );

        let served = self
            .served
            .entry(protocol_system.clone())
            .or_default();
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut states = HashMap::with_capacity(books.len());
        let mut current = HashSet::with_capacity(books.len());
        for (id, book) in books.iter() {
            if served.contains(id) {
                updated.push(id.clone());
            } else {
                added.push(book.component.clone());
            }
            // The book set is shared with every other reader of the channel, so the state has to
            // be copied out to be owned by the market.
            states.insert(id.clone(), book.state.clone_box());
            current.insert(id.clone());
        }
        let removed = served
            .difference(&current)
            .cloned()
            .collect();
        *served = current;

        BookUpdate { protocol_system, added, removed, updated, states }
    }

    /// Drops everything a venue served, leaving it able to serve again if it recovers.
    fn withdraw(&mut self, protocol_system: &str) -> BookUpdate {
        let removed = self
            .served
            .remove(protocol_system)
            .unwrap_or_default()
            .into_iter()
            .collect();
        BookUpdate {
            protocol_system: protocol_system.to_string(),
            added: Vec::new(),
            removed,
            updated: Vec::new(),
            states: HashMap::new(),
        }
    }
}

/// Yields the next book update, pending forever when no book feed is configured.
pub async fn next_book_update(books: &mut Option<BookStream>) -> Result<BookUpdate, DataFeedError> {
    when_configured(books.as_mut().map(BookStream::next)).await
}

fn get_env(var: &str) -> Result<String, DataFeedError> {
    std::env::var(var).map_err(|_| DataFeedError::Config(format!("{var} env var not set")))
}

/// The address `var` holds, or `None` when it is unset.
///
/// A set-but-unparseable value is a configuration error rather than an absent address: Bebop
/// identifies the caller by these, so silently quoting as nobody is worse than refusing to start.
fn optional_address_env(var: &str) -> Result<Option<Bytes>, DataFeedError> {
    let Ok(value) = std::env::var(var) else {
        return Ok(None);
    };
    Address::from_str(value.trim())
        .map(|address| Some(Bytes::from(address.into_array())))
        .map_err(|e| DataFeedError::Config(format!("{var} is not an address: {e}")))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tycho_simulation::tycho_common::models::Chain;

    use super::*;
    use crate::algorithm::test_utils::MockProtocolSim;

    const VENUE: &str = "book:bebop";

    fn book_stream() -> BookStream {
        BookStream { feeds: BookFeedStreams::new(), served: HashMap::new() }
    }

    fn token(address: &str) -> Token {
        let address = Bytes::from(address);
        Token::new(&address, "TKN", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
    }

    /// A snapshot holding one book per id, all on the same pair.
    fn book_snapshot(ids: &[&str]) -> BookSnapshot<ReceivedAt> {
        let books = ids
            .iter()
            .map(|id| {
                let component = ProtocolComponent::new(
                    Bytes::from(*id),
                    VENUE.to_string(),
                    "book".to_string(),
                    Chain::Ethereum,
                    vec![token("0x01"), token("0x02")],
                    vec![],
                    HashMap::new(),
                    Bytes::default(),
                    Default::default(),
                );
                let book = tycho_simulation::book::Book {
                    component,
                    state: Arc::new(MockProtocolSim::new(1.0)),
                    updated_at: None,
                };
                ((*id).to_string(), book)
            })
            .collect();
        BookSnapshot { anchor: ReceivedAt(Utc::now()), books: Arc::new(books) }
    }

    #[test]
    fn test_apply_first_snapshot() {
        let update = book_stream().apply(VENUE.to_string(), book_snapshot(&["0x01", "0x02"]));

        assert_eq!(update.added.len(), 2);
        assert_eq!(update.states.len(), 2);
        assert!(update.removed.is_empty());
        assert!(update.updated.is_empty());
    }

    #[test]
    fn test_apply_republished_snapshot() {
        let mut books = book_stream();
        books.apply(VENUE.to_string(), book_snapshot(&["0x01", "0x02"]));

        let update = books.apply(VENUE.to_string(), book_snapshot(&["0x01", "0x02"]));

        assert!(update.added.is_empty());
        assert!(update.removed.is_empty());
        // The venue's books arrive in a `HashMap`, so the refreshed ids carry its order.
        let mut updated = update.updated.clone();
        updated.sort();
        assert_eq!(updated, ["0x01", "0x02"]);
    }

    #[test]
    fn test_apply_snapshot_missing_a_pair() {
        let mut books = book_stream();
        books.apply(VENUE.to_string(), book_snapshot(&["0x01", "0x02"]));

        let update = books.apply(VENUE.to_string(), book_snapshot(&["0x01"]));

        assert_eq!(update.removed, vec!["0x02".to_string()]);
        assert!(update.added.is_empty());
        assert_eq!(update.states.len(), 1);
    }

    #[test]
    fn test_apply_withdrawn_books() {
        let mut books = book_stream();
        books.apply(VENUE.to_string(), book_snapshot(&["0x01", "0x02"]));

        let update = books.withdraw(VENUE);

        let mut removed = update.removed.clone();
        removed.sort();
        assert_eq!(removed, vec!["0x01".to_string(), "0x02".to_string()]);
        assert!(update.states.is_empty());
        // The venue serves nothing, so the books it publishes next are all new again.
        let update = books.apply(VENUE.to_string(), book_snapshot(&["0x01"]));
        assert_eq!(update.added.len(), 1);
    }

    #[test]
    fn test_apply_untouched_by_another_venue() {
        let mut books = book_stream();
        books.apply(VENUE.to_string(), book_snapshot(&["0x01"]));

        let update = books.apply("book:hashflow".to_string(), book_snapshot(&["0x02"]));

        assert_eq!(update.added.len(), 1);
        assert!(update.removed.is_empty());
    }
}
