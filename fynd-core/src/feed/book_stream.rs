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
        all_pairs::QuoteRule, quote_tokens::usd_stablecoins_for_chain, BookFeedConfig,
        BookFeedEvent, BookFeedStreams, BookSnapshot, ReceivedAt,
    },
    pamm::protocols::metric::{self, feed::MetricFeedBuilder},
    protocol::models::ProtocolComponent,
    rfq::protocols::{
        bebop::{self, feed::BebopFeedBuilder},
        hashflow::{self, feed::HashflowFeedBuilder},
        liquorice::{self, feed::LiquoriceFeedBuilder},
        native::{self, feed::NativeFeedBuilder},
    },
    snapshot_feed::{
        errors::FeedError, SnapshotFeed, SnapshotFeedEvent, SnapshotFeedOutcome,
        SnapshotFeedStreams,
    },
    tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim},
    tycho_core::Bytes,
};

use super::{
    external_liquidity::{
        liquidity_snapshot_from_books, ExternalLiquidityConfig, LiquidityComponent,
        LiquiditySnapshot,
    },
    when_configured, DataFeedError, TychoFeedConfig,
};
use crate::{
    feed::protocol_registry::{parse_rfq_entries, require_executor, METRIC_CHAINS},
    types::ComponentId,
};

/// The market changes one book feed reports: the components it started serving, the ones it
/// stopped serving, and the current state of everything it serves.
pub struct BookUpdate {
    /// The venue that published, e.g. `rfq:bebop`. Also the protocol system of its components.
    pub protocol_system: String,
    /// Components the venue serves that the market does not have yet.
    pub added: Vec<ProtocolComponent>,
    /// Components the market has that the venue no longer serves.
    pub removed: Vec<ComponentId>,
    /// Components the market has whose state the venue refreshed. Each comes with its component,
    /// because an all-pairs venue keeps its id while the swap directions it lists change.
    pub updated: Vec<ProtocolComponent>,
    /// The state of every book the venue currently serves, the added ones included.
    pub states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
}

impl BookUpdate {
    /// Whether the update leaves the market exactly as it is.
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.states.is_empty()
    }
}

/// The book feeds a deployment runs, as a stream of the changes they make to the market: the
/// `rfq:` venues a `--protocols` list asked for and the external liquidity a caller added.
///
/// The venues underneath are latest-value feeds — a reader that falls behind skips to the newest
/// book set — but what this yields are ordered deltas against the ids it last served, so its
/// consumer must apply every one.
pub struct BookStream {
    /// The built-in venues' feeds, merged: each publication, withdrawal and ending arrives here.
    feeds: BookFeedStreams,
    /// The external liquidity sources' feeds, merged the same way.
    external: SnapshotFeedStreams<LiquiditySnapshot, FeedError>,
    /// The component ids each venue's last applied book set contained.
    served: HashMap<String, HashSet<ComponentId>>,
}

/// Builds external liquidity from its source name and its config, and adds it to the merged feed
/// streams.
type OpenExternalLiquidity = dyn Fn(
        &str,
        ExternalLiquidityConfig,
        &mut SnapshotFeedStreams<LiquiditySnapshot, FeedError>,
    ) -> Result<(), DataFeedError>
    + Send
    + Sync;

/// External liquidity added through
/// [`FyndBuilder::with_external_liquidity`](crate::FyndBuilder::with_external_liquidity) instead
/// of a `--protocols` entry. Its components reach the market the way the built-in venues' do.
#[derive(Clone)]
pub(crate) struct ExternalLiquidity {
    /// The feed's name, which is the protocol system of the components it publishes.
    protocol_system: String,
    open: Arc<OpenExternalLiquidity>,
}

impl ExternalLiquidity {
    /// Wraps `make_feed`, which builds the feed from its [`ExternalLiquidityConfig`].
    pub(crate) fn new<M, F>(protocol_system: String, make_feed: M) -> Self
    where
        M: Fn(ExternalLiquidityConfig) -> Result<F, FeedError> + Send + Sync + 'static,
        F: SnapshotFeed<Snapshot = LiquiditySnapshot, Error = FeedError>,
    {
        let open =
            move |protocol_system: &str,
                  config: ExternalLiquidityConfig,
                  feeds: &mut SnapshotFeedStreams<LiquiditySnapshot, FeedError>| {
                let feed = make_feed(config).map_err(|e| {
                    DataFeedError::Config(format!(
                        "{protocol_system} external liquidity could not be built: {e}"
                    ))
                })?;
                feeds
                    .add(protocol_system, feed)
                    .map_err(|_| {
                        DataFeedError::Config(format!(
                            "{protocol_system} external liquidity is added twice"
                        ))
                    })
            };
        Self { protocol_system, open: Arc::new(open) }
    }

    fn open(
        &self,
        config: ExternalLiquidityConfig,
        feeds: &mut SnapshotFeedStreams<LiquiditySnapshot, FeedError>,
    ) -> Result<(), DataFeedError> {
        (self.open)(&self.protocol_system, config, feeds)
    }
}

impl std::fmt::Debug for ExternalLiquidity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalLiquidity")
            .field("protocol_system", &self.protocol_system)
            .finish_non_exhaustive()
    }
}

impl BookStream {
    /// Subscribes to every `rfq:` entry in `config.protocols` and to `config.external_liquidity`.
    /// Returns `None` if there are neither.
    ///
    /// The venues price their books' TVL in USD stablecoins, so `config.min_tvl` is read here as a
    /// USD amount — unlike the native-token threshold the Tycho component filter takes. With
    /// `config.aggregate_rfqs`, Bebop and Hashflow each stream as one component for all their
    /// pairs. External sources get an [`ExternalLiquidityConfig`] with the same chain, tokens and
    /// floor.
    ///
    /// # Errors
    ///
    /// Returns [`DataFeedError::Config`] if a venue's credentials are missing from the
    /// environment, if the chain has no curated USD stablecoin set to price built-in venues'
    /// books against, if external liquidity cannot be built, if two sources share a name, or if a
    /// source has the name of a `--protocols` venue.
    pub fn open(
        config: &TychoFeedConfig,
        tokens: HashMap<Bytes, Token>,
    ) -> Result<Option<Self>, DataFeedError> {
        let chain = config.chain;
        let external_liquidity = &config.external_liquidity;
        let mut venues = Vec::new();
        for (entry, venue) in parse_rfq_entries(&config.protocols)? {
            reject_name_clash(entry, venue.protocol(), external_liquidity)?;
            match venue.protocol() {
                bebop::PROTOCOL_SYSTEM => {
                    // The feed supports more chains than Tycho's execution registry. Reject
                    // deployments that could publish books Fynd cannot encode.
                    require_executor(chain, entry)?;
                }
                hashflow::PROTOCOL_SYSTEM => {
                    // Hashflow's feed builder accepts any chain with a numeric id and defers
                    // venue support to its first poll. Reject a deployment that could publish
                    // books Fynd cannot encode before starting any feed tasks.
                    require_executor(chain, entry)?;
                }
                metric::PROTOCOL_SYSTEM => {
                    if !METRIC_CHAINS.contains(&chain) {
                        return Err(DataFeedError::Config(format!(
                            "{entry} is only served on {}, but this feed runs on {chain}",
                            METRIC_CHAINS
                                .iter()
                                .map(|chain| chain.to_string())
                                .collect_vec()
                                .join(", ")
                        )));
                    }
                    require_executor(chain, entry)?;
                }
                native::PROTOCOL_SYSTEM | liquorice::PROTOCOL_SYSTEM => {
                    require_executor(chain, entry)?;
                }
                unknown => {
                    warn!("Skipping unknown book protocol system: {unknown}");
                    continue;
                }
            }
            venues.push((entry, venue));
        }
        if venues.is_empty() && external_liquidity.is_empty() {
            return Ok(None);
        }

        // Metric reports each pool's USD TVL itself; the other venues price their books' TVL in
        // the chain's USD stablecoins, so only they need a chain that has a curated set.
        let usd_quote_tokens = || {
            usd_stablecoins_for_chain(chain)
                .map(Arc::new)
                .ok_or_else(|| {
                    DataFeedError::Config(format!(
                        "book feeds price their books in USD stablecoins, and none are known for \
                         {chain}"
                    ))
                })
        };
        let book_config =
            BookFeedConfig { chain, tokens: Arc::new(tokens), min_tvl_usd: config.min_tvl };

        let external_config = ExternalLiquidityConfig {
            chain,
            tokens: Arc::clone(&book_config.tokens),
            min_tvl_usd: config.min_tvl,
        };

        let mut books = Self {
            feeds: BookFeedStreams::new(),
            external: SnapshotFeedStreams::new(),
            served: HashMap::new(),
        };
        for source in external_liquidity {
            source.open(external_config.clone(), &mut books.external)?;
        }
        for (entry, venue) in venues {
            match venue.protocol() {
                bebop::PROTOCOL_SYSTEM => {
                    let mut builder = BebopFeedBuilder::new(
                        book_config.clone(),
                        usd_quote_tokens()?,
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
                    if config.aggregate_rfqs {
                        builder = builder.all_pairs();
                    }
                    if venue.uses_fallback_router() {
                        builder = builder.with_fallback_router();
                    }
                    let feed = builder
                        .build()
                        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(entry, feed);
                }
                hashflow::PROTOCOL_SYSTEM => {
                    let mut builder = HashflowFeedBuilder::new(
                        book_config.clone(),
                        usd_quote_tokens()?,
                        get_env("HASHFLOW_USER")?,
                        get_env("HASHFLOW_KEY")?,
                    );
                    if config.aggregate_rfqs {
                        builder = builder.all_pairs(QuoteRule::OncePerMaker);
                    }
                    if venue.uses_fallback_router() {
                        builder = builder.with_fallback_router();
                    }
                    let feed = builder
                        .build()
                        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(entry, feed);
                }
                metric::PROTOCOL_SYSTEM => {
                    let mut builder =
                        MetricFeedBuilder::new(book_config.clone(), get_env("METRIC_API_KEY")?);
                    if let Some(base_url) = trimmed_non_empty(std::env::var("METRIC_API_URL").ok())
                    {
                        builder = builder.base_url(base_url);
                    }
                    if venue.uses_fallback_router() {
                        builder = builder.with_fallback_router();
                    }
                    let feed = builder
                        .build()
                        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(entry, feed);
                }
                native::PROTOCOL_SYSTEM => {
                    let mut builder = NativeFeedBuilder::new(
                        book_config.clone(),
                        usd_quote_tokens()?,
                        get_env("NATIVE_API_KEY")?,
                    );
                    if venue.uses_fallback_router() {
                        builder = builder.with_fallback_router();
                    }
                    let feed = builder
                        .build()
                        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(entry, feed);
                }
                liquorice::PROTOCOL_SYSTEM => {
                    let mut builder = LiquoriceFeedBuilder::new(
                        book_config.clone(),
                        usd_quote_tokens()?,
                        get_env("LIQUORICE_USER")?,
                        get_env("LIQUORICE_KEY")?,
                    );
                    if venue.uses_fallback_router() {
                        builder = builder.with_fallback_router();
                    }
                    let feed = builder
                        .build()
                        .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                    books.add_feed(entry, feed);
                }
                // Every unknown entry was removed during preflight above.
                _ => unreachable!("preflight retained an unknown book protocol"),
            }
        }
        Ok((!books.feeds.is_empty() || !books.external.is_empty()).then_some(books))
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
            let (provider, event) = tokio::select! {
                Some((provider, event)) = self.feeds.next(), if !self.feeds.is_empty() => {
                    (provider, liquidity_event(event))
                }
                Some(next) = self.external.next(), if !self.external.is_empty() => next,
                else => {
                    return Err(DataFeedError::StreamError(
                        "every book feed and external liquidity source gave up".to_string(),
                    ));
                }
            };

            let update = match event {
                SnapshotFeedEvent::Published(snapshot) => self.apply(provider, snapshot),
                SnapshotFeedEvent::Withdrawn => {
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
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Panicked(error)) => {
                    return Err(DataFeedError::StreamError(format!(
                        "book feed {provider} panicked: {error}"
                    )))
                }
                // Both of these end the venue for this process; the feed withdraws on its way
                // out, so its books are usually gone already and this covers the one that ended
                // before publishing anything.
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Failed(error)) => {
                    warn!("Book feed {provider} gave up: {error}");
                    self.withdraw(&provider)
                }
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::RanOut) => {
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
    fn apply(&mut self, protocol_system: String, snapshot: LiquiditySnapshot) -> BookUpdate {
        let components = snapshot.components();
        debug!(protocol_system, components = components.len(), "received snapshot");

        let served = self
            .served
            .entry(protocol_system.clone())
            .or_default();
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut states = HashMap::with_capacity(components.len());
        let mut current = HashSet::with_capacity(components.len());
        for LiquidityComponent { component, state } in components {
            let id = component.id.to_string();
            if served.contains(&id) {
                updated.push(component.clone());
            } else {
                added.push(component.clone());
            }
            // The snapshot is shared with every other reader of the channel, so the state has to
            // be copied out to be owned by the market.
            states.insert(id.clone(), state.clone_box());
            current.insert(id);
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

/// Reads a built-in venue's event as the external liquidity kind, which is what `apply` takes.
fn liquidity_event(event: BookFeedEvent) -> SnapshotFeedEvent<LiquiditySnapshot, FeedError> {
    match event {
        BookFeedEvent::Published(snapshot) => {
            SnapshotFeedEvent::Published(liquidity_snapshot_from_books(&snapshot))
        }
        BookFeedEvent::Withdrawn => SnapshotFeedEvent::Withdrawn,
        BookFeedEvent::Ended(outcome) => SnapshotFeedEvent::Ended(outcome),
    }
}

/// Yields the next book update, pending forever when no book feed is configured.
pub async fn next_book_update(books: &mut Option<BookStream>) -> Result<BookUpdate, DataFeedError> {
    when_configured(books.as_mut().map(BookStream::next)).await
}

/// Rejects external liquidity named like a `--protocols` venue: both would publish components under
/// one protocol system.
fn reject_name_clash(
    entry: &str,
    venue_system: &str,
    external_liquidity: &[ExternalLiquidity],
) -> Result<(), DataFeedError> {
    for source in external_liquidity {
        if source.protocol_system == entry || source.protocol_system == venue_system {
            return Err(DataFeedError::Config(format!(
                "external liquidity {} has the name of the --protocols entry {entry}; rename it or \
                 drop the entry",
                source.protocol_system
            )));
        }
    }
    Ok(())
}

fn get_env(var: &str) -> Result<String, DataFeedError> {
    std::env::var(var).map_err(|_| DataFeedError::Config(format!("{var} env var not set")))
}

/// Trims an optional environment value and treats an empty one as absent.
fn trimmed_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
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

    const VENUE: &str = "rfq:bebop";

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
        let mut updated: Vec<String> = update
            .updated
            .iter()
            .map(|component| component.id.to_string())
            .collect();
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

        let update = books.apply("rfq:hashflow".to_string(), book_snapshot(&["0x02"]));

        assert_eq!(update.added.len(), 1);
        assert!(update.removed.is_empty());
    }

    #[test]
    fn test_trimmed_non_empty() {
        assert_eq!(trimmed_non_empty(None), None);
        assert_eq!(trimmed_non_empty(Some(String::new())), None);
        assert_eq!(trimmed_non_empty(Some("  \t".to_string())), None);
        assert_eq!(
            trimmed_non_empty(Some(" https://metric.example/ ".to_string())),
            Some("https://metric.example/".to_string())
        );
    }
}
