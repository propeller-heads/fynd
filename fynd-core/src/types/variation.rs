//! The variation types, their limits, and their validation.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{
    fallback::FALLBACK_PREFIX,
    feed::protocol_registry::RFQ_PREFIX,
    types::{OrderQuote, SolveError},
};

/// The most variations one request may ask for.
pub const MAX_VARIATIONS: usize = 4;

/// The most routes one `Alternatives` variation may ask for.
pub const MAX_ALTERNATIVES: usize = 4;

/// One extra way to solve an order, asked for beside the main solve.
///
/// A filter variation solves the order again with more liquidity excluded. Its exclusions add to
/// the request's own route filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variation {
    /// Solve without RFQ liquidity.
    NoRfq,
    /// Solve without pAMM liquidity.
    NoPamm,
    /// Solve without these protocol systems. An entry matches a system exactly (`uniswap_v2`), or
    /// a family when it ends in `:` (`fallback:`).
    ExcludeProtocols(Vec<String>),
    /// Find up to this many more routes, each with no pool of the main route or of an earlier
    /// alternative.
    Alternatives(NonZeroUsize),
}

impl Variation {
    /// The protocol entries this variation adds to the request's filter. Empty for `Alternatives`.
    #[must_use]
    pub fn excluded_protocols(&self) -> Vec<String> {
        match self {
            Self::NoRfq => vec![RFQ_PREFIX.to_string(), format!("{FALLBACK_PREFIX}{RFQ_PREFIX}")],
            Self::NoPamm => vec![FALLBACK_PREFIX.to_string()],
            Self::ExcludeProtocols(protocols) => protocols.clone(),
            Self::Alternatives(_) => Vec::new(),
        }
    }
}

/// A variation count or alternative count exceeds its limit.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VariationsValidationError {
    /// The request asks for more variations than [`MAX_VARIATIONS`].
    #[error("at most {max} variations per request, got {count}", max = MAX_VARIATIONS)]
    TooManyVariations {
        /// How many variations the request asked for.
        count: usize,
    },
    /// An `Alternatives` variation asks for more routes than [`MAX_ALTERNATIVES`].
    #[error("alternatives allows at most {max} routes, got {count}", max = MAX_ALTERNATIVES)]
    TooManyAlternatives {
        /// How many routes the variation asked for.
        count: usize,
    },
}

/// Checks the variations against [`MAX_VARIATIONS`] and [`MAX_ALTERNATIVES`].
///
/// # Errors
///
/// Returns [`VariationsValidationError`] for the first exceeded limit.
pub fn validate_variations(variations: &[Variation]) -> Result<(), VariationsValidationError> {
    if variations.len() > MAX_VARIATIONS {
        return Err(VariationsValidationError::TooManyVariations { count: variations.len() });
    }
    for variation in variations {
        let Variation::Alternatives(count) = variation else {
            continue;
        };
        if count.get() > MAX_ALTERNATIVES {
            return Err(VariationsValidationError::TooManyAlternatives { count: count.get() });
        }
    }
    Ok(())
}

/// How one variation of one order ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariationStatus {
    /// The variation found at least one route, and its quotes are attached.
    Success,
    /// The variation found no usable route, or the order has no main quote.
    NoRouteFound,
    /// The variation found paths, but none could take the order's amount.
    InsufficientLiquidity,
    /// The variation ran out of time.
    Timeout,
    /// The solver of the main quote does not solve variations.
    Unsupported,
}

/// The outcome of one variation for one order.
#[must_use]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariationQuote {
    /// The variation that produced these quotes.
    variation: Variation,
    /// How the variation ended.
    status: VariationStatus,
    /// One quote for a filter variation, up to n for `Alternatives(n)`. Empty unless `status` is
    /// [`VariationStatus::Success`].
    quotes: Vec<OrderQuote>,
}

impl VariationQuote {
    /// Creates a variation quote. `quotes` must be empty unless `status` is
    /// [`VariationStatus::Success`].
    pub fn new(variation: Variation, status: VariationStatus, quotes: Vec<OrderQuote>) -> Self {
        Self { variation, status, quotes }
    }

    /// The variation that produced these quotes.
    #[must_use]
    pub fn variation(&self) -> &Variation {
        &self.variation
    }

    /// How the variation ended.
    #[must_use]
    pub fn status(&self) -> VariationStatus {
        self.status
    }

    /// The quotes this variation produced, best first. Empty unless the status is
    /// [`VariationStatus::Success`].
    pub fn quotes(&self) -> &[OrderQuote] {
        &self.quotes
    }

    /// Consumes this variation quote and returns its variation, status and quotes.
    #[must_use]
    pub fn into_parts(self) -> (Variation, VariationStatus, Vec<OrderQuote>) {
        (self.variation, self.status, self.quotes)
    }
}

/// What one variation produced in one worker pool.
#[derive(Debug, Clone)]
pub(crate) enum VariationOutcome {
    /// The worker pool's algorithm does not solve variations.
    Unsupported,
    /// A nonempty list of quotes for routes that passed the worker's checks, best first.
    Solved(Vec<OrderQuote>),
    /// The variation found no route that passed the worker's checks.
    Failed(SolveError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alternatives(count: usize) -> Variation {
        Variation::Alternatives(NonZeroUsize::new(count).unwrap())
    }

    #[test]
    fn test_validate_variations_empty() {
        assert_eq!(validate_variations(&[]), Ok(()));
    }

    #[test]
    fn test_validate_variations_at_limits() {
        let variations = vec![
            Variation::NoRfq,
            Variation::NoPamm,
            Variation::ExcludeProtocols(vec!["uniswap_v2".to_string()]),
            alternatives(MAX_ALTERNATIVES),
        ];
        assert_eq!(validate_variations(&variations), Ok(()));
    }

    #[test]
    fn test_validate_variations_too_many() {
        let variations = vec![Variation::NoRfq; MAX_VARIATIONS + 1];
        assert_eq!(
            validate_variations(&variations),
            Err(VariationsValidationError::TooManyVariations { count: MAX_VARIATIONS + 1 })
        );
    }

    #[test]
    fn test_validate_variations_too_many_alternatives() {
        assert_eq!(
            validate_variations(&[alternatives(MAX_ALTERNATIVES + 1)]),
            Err(VariationsValidationError::TooManyAlternatives { count: MAX_ALTERNATIVES + 1 })
        );
    }

    #[test]
    fn test_alternatives_zero_deserialization() {
        let parsed = serde_json::from_str::<Variation>(r#"{"alternatives": 0}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn test_variation_status_serialization() {
        let json = serde_json::to_string(&VariationStatus::NoRouteFound).unwrap();
        assert_eq!(json, r#""no_route_found""#);
    }

    #[test]
    fn test_excluded_protocols() {
        assert_eq!(Variation::NoRfq.excluded_protocols(), vec!["rfq:", "fallback:rfq:"]);
        assert_eq!(Variation::NoPamm.excluded_protocols(), vec!["fallback:"]);
        assert_eq!(
            Variation::ExcludeProtocols(vec!["uniswap_v2".to_string(), "vm:".to_string()])
                .excluded_protocols(),
            vec!["uniswap_v2", "vm:"]
        );
        assert!(alternatives(2)
            .excluded_protocols()
            .is_empty());
    }
}
