//! Helpers shared by quote simulation and quote logging.

use num_bigint::BigUint;
use num_traits::ToPrimitive;

use crate::OrderQuote;

/// How far a simulated amount sits from the post-fee amount the quote promised, in basis points.
///
/// The router returns the output after it takes the router and client fees, so the quote's own
/// `amount_out`, which is the raw swap output, is not the same quantity and would read as a
/// standing fee-sized gap. The comparison is against the quoted amount less those same fees,
/// reached by addition because `min_amount_received` is that amount less the accepted slippage.
///
/// A negative value means the simulation returned less than the quote promised. Returns `None`
/// when the quote states no fees, when the quoted amount is zero and there is no ratio to take,
/// or when an amount past the range of `f64` saturates the ratio to infinity.
pub(crate) fn deviation_bps(quote: &OrderQuote, simulated_amount_out: &BigUint) -> Option<f64> {
    let fees = quote.fee_breakdown()?;
    let quoted = fees.min_amount_received() + fees.max_slippage();
    if quoted == BigUint::ZERO {
        return None;
    }
    let quoted = quoted.to_f64()?;
    let simulated = simulated_amount_out.to_f64()?;
    let deviation = (simulated - quoted) / quoted * 10_000.0;
    deviation
        .is_finite()
        .then_some(deviation)
}

/// Quote fixtures shared by this module's tests and the simulator's.
#[cfg(test)]
pub(crate) mod fixtures {
    use num_bigint::BigUint;

    use crate::OrderQuote;

    /// Router fee the fixture charges, in output-token units.
    pub(crate) const FIXTURE_ROUTER_FEE: u64 = 7_000;

    /// Client fee the fixture charges, in output-token units.
    pub(crate) const FIXTURE_CLIENT_FEE: u64 = 3_000;

    /// A quote whose raw output sits above `after_fees` by exactly the two fees.
    ///
    /// `max_slippage` is non-zero, so a baseline that used `min_amount_received` on its own would
    /// fail the tests rather than pass them.
    pub(crate) fn quote_with_fees(after_fees: u64) -> OrderQuote {
        let slippage = after_fees / 100;
        let mut quote = quote_without_fees();
        quote.set_amount_out(BigUint::from(after_fees + FIXTURE_ROUTER_FEE + FIXTURE_CLIENT_FEE));
        quote.set_fee_breakdown(crate::FeeBreakdown::new(
            BigUint::from(FIXTURE_ROUTER_FEE),
            BigUint::from(FIXTURE_CLIENT_FEE),
            BigUint::from(slippage),
            BigUint::from(after_fees - slippage),
        ));
        quote
    }

    /// The same quote before encoding computes its fees.
    pub(crate) fn quote_without_fees() -> OrderQuote {
        OrderQuote::new(
            "test-order".to_string(),
            crate::QuoteStatus::Success,
            BigUint::from(1_000u64),
            BigUint::from(1_000_000u64),
            BigUint::from(50_000u64),
            BigUint::from(1_000_000u64),
            crate::BlockInfo::new(1, "0x1".to_string(), 1),
            "test_algorithm".to_string(),
            tycho_simulation::tycho_common::Bytes::from(vec![0xAA; 20]),
            tycho_simulation::tycho_common::Bytes::from(vec![0xAA; 20]),
            "1".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::*, *};

    #[test]
    fn test_deviation_bps_simulated_below_quote() {
        let deviation = deviation_bps(&quote_with_fees(1_000_000), &BigUint::from(999_000u64))
            .expect("a quote carrying fees has a baseline");

        assert!((deviation - -10.0).abs() < 1e-9, "got {deviation}");
    }

    #[test]
    fn test_deviation_bps_simulated_above_quote() {
        let deviation = deviation_bps(&quote_with_fees(1_000_000), &BigUint::from(1_001_000u64))
            .expect("a quote carrying fees has a baseline");

        assert!((deviation - 10.0).abs() < 1e-9, "got {deviation}");
    }

    /// The router returns the output after it takes its fees, so the baseline must be the quoted
    /// amount less those fees. Comparing against the raw `amount_out` would read this as a
    /// shortfall the size of the fee.
    #[test]
    fn test_deviation_bps_excludes_the_router_fee_from_the_baseline() {
        let quote = quote_with_fees(1_000_000);
        let after_fees = quote
            .fee_breakdown()
            .expect("the quote carries fees")
            .min_amount_received() +
            quote
                .fee_breakdown()
                .expect("the quote carries fees")
                .max_slippage();

        let deviation =
            deviation_bps(&quote, &after_fees).expect("a quote carrying fees has a baseline");

        assert!(
            deviation.abs() < 1e-9,
            "a simulation matching the post-fee quote is not a deviation"
        );
        assert!(after_fees < *quote.amount_out(), "the baseline sits below the raw swap output");
    }

    #[test]
    fn test_deviation_bps_without_a_fee_breakdown() {
        assert_eq!(deviation_bps(&quote_without_fees(), &BigUint::from(1_000u64)), None);
    }

    #[test]
    fn test_deviation_bps_zero_quoted_amount() {
        let mut quote = quote_with_fees(1_000_000);
        quote.set_fee_breakdown(crate::FeeBreakdown::new(
            BigUint::ZERO,
            BigUint::ZERO,
            BigUint::ZERO,
            BigUint::ZERO,
        ));

        assert_eq!(deviation_bps(&quote, &BigUint::from(1_000u64)), None);
    }

    /// An amount past `f64` saturates to infinity rather than failing to convert, so the ratio is
    /// rejected on being non-finite. Recording it would put `inf` in the histogram.
    #[test]
    fn test_deviation_bps_amount_beyond_f64() {
        let huge = BigUint::from(1u8) << 2_000;

        assert_eq!(deviation_bps(&quote_with_fees(1_000_000), &huge), None);
    }
}
