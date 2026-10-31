//! The kinds of stream the service serves.

use std::fmt;

use fynd_core::nexus::rfq::RfqVenue;
use metrics::Label;

/// What a stream carries: one RFQ venue's prices, or the components pods should not route
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    /// The prices of one RFQ venue.
    Rfq(RfqVenue),
    /// The components the health monitor found broken or inaccurate.
    ExcludedComponents,
}

impl StreamKind {
    /// The label every metric of a stream of this kind carries besides `chain`. An RFQ stream
    /// keeps the `venue` label its metrics have always had.
    pub fn label(self) -> Label {
        match self {
            Self::Rfq(venue) => Label::new("venue", venue.to_string()),
            Self::ExcludedComponents => Label::new("stream", self.to_string()),
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rfq(venue) => write!(formatter, "{venue}"),
            Self::ExcludedComponents => formatter.write_str("excluded_components"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::rfq(StreamKind::Rfq(RfqVenue::Bebop), "bebop", "venue")]
    #[case::components(StreamKind::ExcludedComponents, "excluded_components", "stream")]
    fn test_name_and_label(#[case] kind: StreamKind, #[case] name: &str, #[case] key: &str) {
        let label = kind.label();

        assert_eq!(kind.to_string(), name);
        assert_eq!((label.key(), label.value()), (key, name));
    }
}
