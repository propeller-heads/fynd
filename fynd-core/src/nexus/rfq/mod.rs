//! RFQ prices shared between pods: the venues a `fynd-nexus` service holds a connection
//! for, and the feed that turns their prices into books.

mod feed;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

use std::{collections::HashMap, fmt, str::FromStr, time::Duration};

pub use feed::RfqPricesFeed;
use tycho_simulation::{
    book::Book,
    pamm::protocols::metric::{feed::MetricFeedBuilder, MetricBookAssembler, MetricPrices},
    rfq::protocols::{
        bebop::{feed::BebopFeedBuilder, BebopBookAssembler, BebopPrices},
        hashflow::{feed::HashflowFeedBuilder, HashflowBookAssembler, HashflowPrices},
    },
};

/// An RFQ venue whose pricing connection Fynd Nexus can manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RfqVenue {
    /// Bebop.
    Bebop,
    /// Hashflow.
    Hashflow,
    /// Metric.
    Metric,
}

impl RfqVenue {
    /// Every venue the service can serve.
    pub const ALL: [RfqVenue; 3] = [Self::Bebop, Self::Hashflow, Self::Metric];

    /// The name of this venue, e.g. `bebop` in `/v1/rfq/bebop`. It is also the
    /// `--venues` value and the `venue` metric label.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bebop => "bebop",
            Self::Hashflow => "hashflow",
            Self::Metric => "metric",
        }
    }
}

impl fmt::Display for RfqVenue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The path that serves the current prices of `venue` in a `fynd-nexus` service,
/// e.g. `/v1/rfq/bebop`.
pub fn prices_path(venue: RfqVenue) -> String {
    format!("/v1/rfq/{venue}")
}

/// A venue name not listed in [`RfqVenue::ALL`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown venue '{0}'; expected one of bebop, hashflow, metric")]
pub struct UnknownRfqVenue(String);

impl FromStr for RfqVenue {
    type Err = UnknownRfqVenue;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|venue| venue.name() == name)
            .ok_or_else(|| UnknownRfqVenue(name.to_string()))
    }
}

/// Builds one venue's books from its prices, with the pod's book settings. The variant says which
/// venue's prices it reads.
pub enum RfqAssembler {
    /// Reads [`BebopPrices`].
    Bebop(BebopBookAssembler),
    /// Reads [`HashflowPrices`].
    Hashflow(HashflowBookAssembler),
    /// Reads [`MetricPrices`].
    Metric(MetricBookAssembler),
}

impl RfqAssembler {
    /// The venue whose prices the assembler reads.
    pub fn venue(&self) -> RfqVenue {
        match self {
            Self::Bebop(_) => RfqVenue::Bebop,
            Self::Hashflow(_) => RfqVenue::Hashflow,
            Self::Metric(_) => RfqVenue::Metric,
        }
    }

    /// Reads `data` as this venue's prices and builds the books, keyed by component id.
    ///
    /// # Errors
    ///
    /// The JSON error when `data` is not this venue's prices.
    fn books(&self, data: serde_json::Value) -> Result<HashMap<String, Book>, serde_json::Error> {
        Ok(match self {
            Self::Bebop(assembler) => assembler.books(serde_json::from_value::<BebopPrices>(data)?),
            Self::Hashflow(assembler) => {
                assembler.books(serde_json::from_value::<HashflowPrices>(data)?)
            }
            Self::Metric(assembler) => {
                assembler.books(serde_json::from_value::<MetricPrices>(data)?)
            }
        })
    }
}

/// The snapshot age limit used by the venue's direct feed, or `None` for no limit.
fn max_snapshot_age(venue: RfqVenue) -> Option<Duration> {
    match venue {
        RfqVenue::Bebop => BebopFeedBuilder::default_feed_config().max_snapshot_age,
        RfqVenue::Hashflow => HashflowFeedBuilder::default_feed_config().max_snapshot_age,
        RfqVenue::Metric => MetricFeedBuilder::default_feed_config().max_snapshot_age,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::{test_support::*, *};

    #[test]
    fn test_venue_names() {
        for venue in RfqVenue::ALL {
            assert_eq!(venue.name().parse(), Ok(venue));
            assert_eq!(venue.to_string(), venue.name());
        }
        assert_eq!("liquorice".parse::<RfqVenue>(), Err(UnknownRfqVenue("liquorice".to_string())));
    }

    #[test]
    fn test_prices_path() {
        assert_eq!(prices_path(RfqVenue::Metric), "/v1/rfq/metric");
    }

    #[rstest]
    #[case::bebop(RfqAssembler::Bebop(bebop_assembler()), RfqVenue::Bebop)]
    #[case::hashflow(RfqAssembler::Hashflow(hashflow_assembler()), RfqVenue::Hashflow)]
    #[case::metric(RfqAssembler::Metric(metric_assembler()), RfqVenue::Metric)]
    fn test_assembler_venue(#[case] assembler: RfqAssembler, #[case] venue: RfqVenue) {
        assert_eq!(assembler.venue(), venue);
    }

    #[rstest]
    #[case::bebop(RfqAssembler::Bebop(bebop_assembler()), bebop_json(), bebop_books())]
    #[case::hashflow(
        RfqAssembler::Hashflow(hashflow_assembler()),
        hashflow_json(),
        hashflow_books()
    )]
    #[case::metric(RfqAssembler::Metric(metric_assembler()), metric_json(), metric_books())]
    fn test_shared_and_direct_book_assembly(
        #[case] assembler: RfqAssembler,
        #[case] data: serde_json::Value,
        #[case] direct: HashMap<String, Book>,
    ) {
        let books = assembler.books(data).unwrap();

        assert!(!direct.is_empty());
        assert_same_books(&books, &direct);
        assert_same_prices(&books, &direct);
    }

    #[rstest]
    #[case::bebop_given_metric(RfqAssembler::Bebop(bebop_assembler()), metric_json())]
    #[case::hashflow_given_bebop(RfqAssembler::Hashflow(hashflow_assembler()), bebop_json())]
    #[case::metric_given_hashflow(RfqAssembler::Metric(metric_assembler()), hashflow_json())]
    #[case::bebop_given_nothing(RfqAssembler::Bebop(bebop_assembler()), json!(null))]
    fn test_books_reject_another_venues_prices(
        #[case] assembler: RfqAssembler,
        #[case] data: serde_json::Value,
    ) {
        assert!(assembler.books(data).is_err());
    }

    fn bebop_books() -> HashMap<String, Book> {
        bebop_assembler().books(bebop_prices(3000.5))
    }

    fn hashflow_books() -> HashMap<String, Book> {
        hashflow_assembler().books(hashflow_prices())
    }

    fn metric_books() -> HashMap<String, Book> {
        metric_assembler().books(metric_prices())
    }

    fn bebop_json() -> serde_json::Value {
        serde_json::to_value(bebop_prices(3000.5)).unwrap()
    }

    fn hashflow_json() -> serde_json::Value {
        serde_json::to_value(hashflow_prices()).unwrap()
    }

    fn metric_json() -> serde_json::Value {
        serde_json::to_value(metric_prices()).unwrap()
    }

    fn assert_same_books(books: &HashMap<String, Book>, direct: &HashMap<String, Book>) {
        assert_eq!(books.len(), direct.len());
        for (id, book) in direct {
            let decoded = books
                .get(id)
                .expect("expected the assembled books to contain this component id");
            assert_eq!(decoded.component, book.component);
        }
    }

    fn assert_same_prices(books: &HashMap<String, Book>, direct: &HashMap<String, Book>) {
        for (id, book) in direct {
            assert_eq!(amount_out(&books[id]), amount_out(book));
        }
    }
}
