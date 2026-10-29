//! The book venues Fynd can run: the `rfq:` entries of `--protocols`.

use itertools::Itertools;
use tracing::warn;
use tycho_simulation::{
    pamm::protocols::metric,
    rfq::protocols::{bebop, hashflow, liquorice, native},
    tycho_common::models::Chain,
};

use super::DataFeedError;
use crate::feed::protocol_registry::{parse_rfq_entries, require_executor, METRIC_CHAINS};

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
            bebop::PROTOCOL_SYSTEM => Some(Self::Bebop),
            hashflow::PROTOCOL_SYSTEM => Some(Self::Hashflow),
            metric::PROTOCOL_SYSTEM => Some(Self::Metric),
            native::PROTOCOL_SYSTEM => Some(Self::Native),
            liquorice::PROTOCOL_SYSTEM => Some(Self::Liquorice),
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
            // The feed supports more chains than Tycho's execution registry. Reject deployments
            // that could publish books Fynd cannot encode.
            BookVenue::Bebop => require_executor(chain, self.name),
            // Hashflow's feed builder accepts any chain with a numeric id and defers venue
            // support to its first poll, so the executor is checked here instead.
            BookVenue::Hashflow => require_executor(chain, self.name),
            BookVenue::Native | BookVenue::Liquorice => require_executor(chain, self.name),
            BookVenue::Metric => {
                if !METRIC_CHAINS.contains(&chain) {
                    return Err(DataFeedError::Config(format!(
                        "{} is only served on {}, but this feed runs on {chain}",
                        self.name,
                        METRIC_CHAINS
                            .iter()
                            .map(|chain| chain.to_string())
                            .collect_vec()
                            .join(", ")
                    )));
                }
                require_executor(chain, self.name)
            }
        }
    }
}
