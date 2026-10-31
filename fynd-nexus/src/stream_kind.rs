//! The kinds of stream the service serves.

use std::fmt;

use fynd_core::nexus::price_levels::PriceLevelVenue;
use metrics::Label;

/// What a stream carries: one venue's price levels, or the components pods should not route
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    /// The price levels of one venue.
    PriceLevels(PriceLevelVenue),
    /// The components the health monitor found broken or inaccurate.
    ExcludedComponents,
}

impl StreamKind {
    /// The label every metric of a stream of this kind carries besides `chain`. A price levels
    /// stream keeps the `venue` label its metrics have always had.
    pub fn label(self) -> Label {
        match self {
            Self::PriceLevels(venue) => Label::new("venue", venue.to_string()),
            Self::ExcludedComponents => Label::new("stream", self.to_string()),
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PriceLevels(venue) => write!(formatter, "{venue}"),
            Self::ExcludedComponents => formatter.write_str("excluded_components"),
        }
    }
}
