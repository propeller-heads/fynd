//! Single-block replay decay computation.
//!
//! Computes the proportional output difference between a quote-time execution
//! and a replayed execution at `quote_block + k`. This is the label source for
//! the slippage-decay prediction model (ENG-5986).
//!
//! Decay formula:
//!   `decay_bps = (quote_output - replay_output) / quote_output × 10_000`
//!
//! Interpretation:
//! - **Positive** decay → route degraded (less output at block+k)
//! - **Zero** decay → output unchanged between quote and replay
//! - **Negative** decay → route improved (more output at block+k)

/// Errors that can occur during decay computation.
#[derive(Debug, thiserror::Error)]
pub enum DecayError {
    /// The amount string could not be parsed as a finite positive number.
    #[error("invalid amount '{value}': {reason}")]
    InvalidAmount { value: String, reason: String },

    /// The quote output is zero, so proportional decay is undefined.
    #[error("quote output is zero; proportional decay is undefined")]
    ZeroQuoteOutput,
}

/// Why a route replay was invalid at a particular block offset.
///
/// When the Fynd replayer simulates execution at `quote_block + k`, the route
/// may no longer be executable for structural reasons. These reasons are
/// captured here so the decay pipeline can distinguish "route broken" from
/// "replay returned a number."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteInvalidReason {
    /// The pool's reserves were fully drained at the replay block.
    PoolDrained,
    /// A token in the route was deregistered or blacklisted.
    TokenDeregistered,
    /// Remaining pool liquidity was too low to fill the trade.
    InsufficientLiquidity,
    /// Catch-all for other replay failures with a descriptive message.
    Other(String),
}

impl std::fmt::Display for RouteInvalidReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PoolDrained => write!(f, "pool drained"),
            Self::TokenDeregistered => write!(f, "token deregistered"),
            Self::InsufficientLiquidity => write!(f, "insufficient liquidity"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

/// Outcome of replaying a route at a given block offset.
///
/// Wraps either a valid replay amount (as a numeric string, matching
/// [`QuoteRecord`](crate::QuoteRecord) conventions) or a signal that the route
/// could not be replayed at that block.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayOutcome<'a> {
    /// The replay succeeded and produced this output amount.
    Valid(&'a str),
    /// The route was no longer executable at this block offset.
    RouteInvalid(RouteInvalidReason),
}

/// Result of a single-block replay decay computation.
#[derive(Debug, Clone, PartialEq)]
pub struct DecayResult {
    /// Block offset k (1..=10) from the quote block.
    pub block_offset: u32,
    /// Proportional output difference in basis points (1 bps = 0.01%).
    /// Positive means the route degraded; negative means it improved.
    pub decay_bps: f64,
    /// The quote-time output amount (parsed from string).
    pub quote_output: f64,
    /// The replayed output amount at quote_block+k (parsed from string).
    pub replay_output: f64,
}

/// Compute the decay value for a single (route, block_offset) pair.
///
/// Takes the quote-time output amount and the replayed output amount
/// (both as numeric strings, matching the [`crate::QuoteRecord`] format)
/// and returns the proportional difference in basis points.
///
/// # Errors
///
/// Returns [`DecayError::InvalidAmount`] if either amount string cannot be
/// parsed as a finite f64 value, or [`DecayError::ZeroQuoteOutput`] if the
/// quote output is zero (making the ratio undefined).
pub fn compute_single_block_decay(
    quote_amount_out: &str,
    replay_amount_out: &str,
    block_offset: u32,
) -> Result<DecayResult, DecayError> {
    let quote_output = parse_amount(quote_amount_out)?;
    let replay_output = parse_amount(replay_amount_out)?;

    if quote_output == 0.0 {
        return Err(DecayError::ZeroQuoteOutput);
    }

    let decay_bps = (quote_output - replay_output) / quote_output * 10_000.0;

    Ok(DecayResult { block_offset, decay_bps, quote_output, replay_output })
}

/// Parse a numeric-string amount into a finite f64.
fn parse_amount(value: &str) -> Result<f64, DecayError> {
    let parsed = value
        .parse::<f64>()
        .map_err(|e| DecayError::InvalidAmount {
            value: value.to_owned(),
            reason: e.to_string(),
        })?;

    if !parsed.is_finite() {
        return Err(DecayError::InvalidAmount {
            value: value.to_owned(),
            reason: "value is not finite".to_owned(),
        });
    }

    Ok(parsed)
}

/// The maximum block offset in the replay window (inclusive).
pub const MAX_BLOCK_OFFSET: u32 = 10;

/// Result of building the full replay decay array for k=1..=10.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayDecayArray {
    /// Decay values in basis points for block offsets 1..=10.
    /// Index 0 corresponds to k=1, index 9 to k=10.
    /// `None` for offsets without a valid replay amount.
    pub decay_bps: [Option<f64>; MAX_BLOCK_OFFSET as usize],
    /// Individual per-block decay results, ordered by block_offset,
    /// only for offsets that produced a valid result.
    pub results: Vec<DecayResult>,
}

/// Build the full replay-decay label array for block offsets k=1..=10.
///
/// Takes the quote-time output and a set of `(block_offset, replay_amount_out)`
/// pairs. Calls [`compute_single_block_decay`] for each provided offset in
/// 1..=10, assembling results into the fixed-size array.
///
/// Offsets outside 1..=10 are silently ignored. If a replay amount string
/// is unparseable for a given offset, that slot is set to `None` (graceful
/// degradation per-block).
///
/// # Errors
///
/// Returns [`DecayError::InvalidAmount`] if `quote_amount_out` cannot be
/// parsed as a finite number — this is a hard failure because the quote
/// amount is shared across all offsets.
///
/// Returns [`DecayError::ZeroQuoteOutput`] if the quote output is zero.
pub fn build_replay_decay_array(
    quote_amount_out: &str,
    replay_amounts: &[(u32, &str)],
) -> Result<ReplayDecayArray, DecayError> {
    let quote_val = parse_amount(quote_amount_out)?;
    if quote_val == 0.0 {
        return Err(DecayError::ZeroQuoteOutput);
    }

    let mut decay_bps = [None; MAX_BLOCK_OFFSET as usize];
    let mut results = Vec::new();

    for k in 1..=MAX_BLOCK_OFFSET {
        let idx = (k - 1) as usize;
        let Some((_, replay_out)) = replay_amounts
            .iter()
            .find(|(offset, _)| *offset == k)
        else {
            continue;
        };

        match compute_single_block_decay(quote_amount_out, replay_out, k) {
            Ok(result) => {
                decay_bps[idx] = Some(result.decay_bps);
                results.push(result);
            }
            Err(DecayError::InvalidAmount { .. }) => {
                // Per-block replay failure: slot stays None.
            }
            Err(e) => return Err(e),
        }
    }

    Ok(ReplayDecayArray { decay_bps, results })
}

/// Build the replay-decay label array from typed [`ReplayOutcome`] values.
///
/// Like [`build_replay_decay_array`] but accepts [`ReplayOutcome`] per offset
/// instead of raw strings. This allows callers to express that a route was
/// **structurally invalid** at a given block (pool drained, token deregistered,
/// etc.) rather than merely providing an unparseable amount string.
///
/// Slots where the replay outcome is [`ReplayOutcome::RouteInvalid`] are set
/// to `None` — the same sentinel as a missing offset — so downstream consumers
/// (correlation analysis, model training) can treat them uniformly as "no
/// observation." Valid offsets are unaffected by invalid neighbors.
///
/// # Errors
///
/// Returns [`DecayError::InvalidAmount`] if `quote_amount_out` is not a finite
/// number, or [`DecayError::ZeroQuoteOutput`] if it is zero.
pub fn build_decay_from_replay_outcomes<'a>(
    quote_amount_out: &str,
    replay_outcomes: &[(u32, ReplayOutcome<'a>)],
) -> Result<ReplayDecayArray, DecayError> {
    let quote_val = parse_amount(quote_amount_out)?;
    if quote_val == 0.0 {
        return Err(DecayError::ZeroQuoteOutput);
    }

    let mut decay_bps = [None; MAX_BLOCK_OFFSET as usize];
    let mut results = Vec::new();

    for k in 1..=MAX_BLOCK_OFFSET {
        let idx = (k - 1) as usize;
        let Some((_, outcome)) = replay_outcomes
            .iter()
            .find(|(offset, _)| *offset == k)
        else {
            continue;
        };

        let replay_str = match outcome {
            ReplayOutcome::Valid(amount) => amount,
            ReplayOutcome::RouteInvalid(_) => {
                // Route structurally invalid at this block: sentinel None.
                continue;
            }
        };

        match compute_single_block_decay(quote_amount_out, replay_str, k) {
            Ok(result) => {
                decay_bps[idx] = Some(result.decay_bps);
                results.push(result);
            }
            Err(DecayError::InvalidAmount { .. }) => {
                // Per-block replay parse failure: slot stays None.
            }
            Err(e) => return Err(e),
        }
    }

    Ok(ReplayDecayArray { decay_bps, results })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Correct decay against known examples ──────────────────────

    #[test]
    fn decay_for_known_values() {
        // Quote output: 1000, replay output: 990 → lost 1% = 100 bps
        let result = compute_single_block_decay("1000", "990", 1).expect("valid inputs");

        assert_eq!(result.block_offset, 1);
        assert!(
            (result.decay_bps - 100.0).abs() < 1e-10,
            "expected 100 bps, got {}",
            result.decay_bps
        );
        assert_eq!(result.quote_output, 1000.0);
        assert_eq!(result.replay_output, 990.0);
    }

    #[test]
    fn decay_with_large_token_amounts() {
        // Realistic ETH→USDC: 1e18 Wei → 3500e6 USDC at quote,
        // 3497.5e6 USDC at replay (≈7.14 bps)
        let quote = "3500000000";
        let replay = "3497500000";
        let result = compute_single_block_decay(quote, replay, 3).expect("valid inputs");

        let expected_bps = (3_500_000_000.0 - 3_497_500_000.0) / 3_500_000_000.0 * 10_000.0;
        assert!(
            (result.decay_bps - expected_bps).abs() < 1e-6,
            "expected {expected_bps} bps, got {}",
            result.decay_bps
        );
        assert_eq!(result.block_offset, 3);
    }

    #[test]
    fn decay_50_percent_loss() {
        // Quote 200, replay 100 → 50% loss = 5000 bps
        let result = compute_single_block_decay("200", "100", 5).expect("valid inputs");

        assert!(
            (result.decay_bps - 5000.0).abs() < 1e-10,
            "expected 5000 bps, got {}",
            result.decay_bps
        );
    }

    #[test]
    fn decay_small_fractional_amounts() {
        // Quote 0.001, replay 0.0009 → 10% loss = 1000 bps
        let result = compute_single_block_decay("0.001", "0.0009", 1).expect("valid inputs");

        assert!(
            (result.decay_bps - 1000.0).abs() < 1e-6,
            "expected 1000 bps, got {}",
            result.decay_bps
        );
    }

    // ── Zero decay (output unchanged) ─────────────────────────────

    #[test]
    fn zero_decay_when_output_unchanged() {
        let result = compute_single_block_decay("1000", "1000", 1).expect("valid inputs");

        assert!(result.decay_bps.abs() < 1e-10, "expected 0 bps, got {}", result.decay_bps);
    }

    #[test]
    fn zero_decay_with_large_equal_amounts() {
        let amount = "999999999999999999";
        let result = compute_single_block_decay(amount, amount, 7).expect("valid inputs");

        assert!(
            result.decay_bps.abs() < 1e-10,
            "expected 0 bps for identical amounts, got {}",
            result.decay_bps
        );
    }

    // ── Negative decay (improvement) ──────────────────────────────

    #[test]
    fn negative_decay_when_output_improves() {
        // Quote 1000, replay 1010 → -1% improvement = -100 bps
        let result = compute_single_block_decay("1000", "1010", 2).expect("valid inputs");

        assert!(
            (result.decay_bps - (-100.0)).abs() < 1e-10,
            "expected -100 bps, got {}",
            result.decay_bps
        );
    }

    #[test]
    fn negative_decay_large_improvement() {
        // Quote 500, replay 750 → -50% improvement = -5000 bps
        let result = compute_single_block_decay("500", "750", 10).expect("valid inputs");

        assert!(
            (result.decay_bps - (-5000.0)).abs() < 1e-10,
            "expected -5000 bps, got {}",
            result.decay_bps
        );
        assert_eq!(result.block_offset, 10);
    }

    // ── Error cases ───────────────────────────────────────────────

    #[test]
    fn error_on_zero_quote_output() {
        let result = compute_single_block_decay("0", "100", 1);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DecayError::ZeroQuoteOutput), "expected ZeroQuoteOutput, got {err}");
        assert!(err.to_string().contains("zero"));
    }

    #[test]
    fn error_on_non_numeric_quote() {
        let result = compute_single_block_decay("abc", "100", 1);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DecayError::InvalidAmount { .. }),
            "expected InvalidAmount, got {err}"
        );
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn error_on_non_numeric_replay() {
        let result = compute_single_block_decay("100", "xyz", 1);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DecayError::InvalidAmount { .. }),
            "expected InvalidAmount, got {err}"
        );
        assert!(err.to_string().contains("xyz"));
    }

    #[test]
    fn error_on_empty_quote_string() {
        let result = compute_single_block_decay("", "100", 1);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::InvalidAmount { .. }));
    }

    #[test]
    fn error_on_empty_replay_string() {
        let result = compute_single_block_decay("100", "", 1);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::InvalidAmount { .. }));
    }

    #[test]
    fn error_on_infinity_quote() {
        let result = compute_single_block_decay("inf", "100", 1);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::InvalidAmount { .. }));
    }

    #[test]
    fn error_on_nan_replay() {
        let result = compute_single_block_decay("100", "NaN", 1);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::InvalidAmount { .. }));
    }

    // ── Block offset passthrough ──────────────────────────────────

    #[test]
    fn block_offset_is_preserved_in_result() {
        for k in 1..=10 {
            let result = compute_single_block_decay("1000", "999", k).expect("valid inputs");
            assert_eq!(result.block_offset, k, "block_offset should be {k}");
        }
    }

    // ── Total loss ────────────────────────────────────────────────

    #[test]
    fn total_loss_is_10000_bps() {
        // Quote 1000, replay 0 → 100% loss = 10000 bps
        let result = compute_single_block_decay("1000", "0", 1).expect("valid inputs");

        assert!(
            (result.decay_bps - 10_000.0).abs() < 1e-10,
            "expected 10000 bps for total loss, got {}",
            result.decay_bps
        );
    }

    // ── Negative replay output ────────────────────────────────────

    #[test]
    fn negative_replay_output_produces_large_positive_decay() {
        // In theory a negative replay output doesn't make physical
        // sense, but the function should handle it mathematically.
        // Quote 100, replay -50 → (100 - (-50)) / 100 * 10000 = 15000
        let result = compute_single_block_decay("100", "-50", 1).expect("valid inputs");

        assert!(
            (result.decay_bps - 15_000.0).abs() < 1e-10,
            "expected 15000 bps, got {}",
            result.decay_bps
        );
    }

    // ══════════════════════════════════════════════════════════════
    // build_replay_decay_array tests
    // ══════════════════════════════════════════════════════════════

    // ── Array length ─────────────────────────────────────────────

    #[test]
    fn array_always_has_10_elements_with_all_offsets() {
        let replays: Vec<(u32, &str)> = (1..=10).map(|k| (k, "990")).collect();
        let result = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert_eq!(
            result.decay_bps.len(),
            MAX_BLOCK_OFFSET as usize,
            "array must always have exactly 10 elements"
        );
    }

    #[test]
    fn array_has_10_elements_with_no_replays() {
        let result = build_replay_decay_array("1000", &[]).expect("valid inputs");

        assert_eq!(result.decay_bps.len(), 10);
        assert!(result
            .decay_bps
            .iter()
            .all(|v| v.is_none()));
        assert!(result.results.is_empty());
    }

    #[test]
    fn array_has_10_elements_with_partial_replays() {
        let replays = vec![(1, "990"), (5, "950")];
        let result = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert_eq!(result.decay_bps.len(), 10);
    }

    // ── Ordering of offsets ──────────────────────────────────────

    #[test]
    fn offsets_map_to_correct_array_indices() {
        let replays: Vec<(u32, &str)> = (1..=10).map(|k| (k, "990")).collect();
        let result = build_replay_decay_array("1000", &replays).expect("valid inputs");

        for (idx, val) in result.decay_bps.iter().enumerate() {
            assert!(val.is_some(), "index {idx} (k={}) should be populated", idx + 1);
        }
    }

    #[test]
    fn sparse_offsets_leave_gaps_as_none() {
        let replays = vec![(2, "980"), (7, "930")];
        let result = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert!(result.decay_bps[0].is_none(), "k=1 not provided");
        assert!(result.decay_bps[1].is_some(), "k=2 provided");
        for idx in 2..6 {
            assert!(result.decay_bps[idx].is_none(), "k={} not provided", idx + 1);
        }
        assert!(result.decay_bps[6].is_some(), "k=7 provided");
        for idx in 7..10 {
            assert!(result.decay_bps[idx].is_none(), "k={} not provided", idx + 1);
        }
    }

    #[test]
    fn results_vec_ordered_by_block_offset() {
        let replays = vec![(5, "950"), (1, "990"), (10, "900")];
        let result = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert_eq!(result.results.len(), 3);
        assert_eq!(result.results[0].block_offset, 1);
        assert_eq!(result.results[1].block_offset, 5);
        assert_eq!(result.results[2].block_offset, 10);
    }

    #[test]
    fn out_of_range_offsets_are_ignored() {
        let replays = vec![(0, "999"), (1, "990"), (11, "880"), (20, "800")];
        let result = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].block_offset, 1);
    }

    // ── Propagation of per-block results ─────────────────────────

    #[test]
    fn decay_values_match_single_block_results() {
        let replays: Vec<(u32, &str)> = vec![(1, "990"), (2, "980"), (3, "970")];
        let array = build_replay_decay_array("1000", &replays).expect("valid inputs");

        for (k, replay_str) in &replays {
            let expected = compute_single_block_decay("1000", replay_str, *k).expect("valid");
            let idx = (*k - 1) as usize;
            assert!(
                (array.decay_bps[idx].expect("populated") - expected.decay_bps).abs() < 1e-10,
                "array[{idx}] should match single-block result for k={k}"
            );
        }
    }

    #[test]
    fn full_10_block_decay_matches_individual_calls() {
        let replay_amounts = ["995", "990", "985", "980", "975", "970", "965", "960", "955", "950"];
        let replays: Vec<(u32, &str)> = (1..=10)
            .zip(replay_amounts.iter().copied())
            .collect();

        let array = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert_eq!(array.results.len(), 10);

        for k in 1..=10u32 {
            let idx = (k - 1) as usize;
            let expected =
                compute_single_block_decay("1000", replay_amounts[idx], k).expect("valid");

            let actual_bps = array.decay_bps[idx].expect("populated");
            assert!(
                (actual_bps - expected.decay_bps).abs() < 1e-10,
                "k={k}: expected {} bps, got {actual_bps}",
                expected.decay_bps
            );

            assert_eq!(array.results[idx].block_offset, k);
            assert_eq!(array.results[idx].quote_output, 1000.0);
            assert_eq!(array.results[idx].replay_output, expected.replay_output);
        }
    }

    #[test]
    fn results_carry_full_decay_result_fields() {
        let replays = vec![(3, "970")];
        let array = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert_eq!(array.results.len(), 1);
        let r = &array.results[0];
        assert_eq!(r.block_offset, 3);
        assert_eq!(r.quote_output, 1000.0);
        assert_eq!(r.replay_output, 970.0);
        let expected_bps = (1000.0 - 970.0) / 1000.0 * 10_000.0;
        assert!((r.decay_bps - expected_bps).abs() < 1e-10);
    }

    // ── Error handling ───────────────────────────────────────────

    #[test]
    fn error_on_invalid_quote_amount() {
        let result = build_replay_decay_array("not_a_number", &[(1, "990")]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::InvalidAmount { .. }));
    }

    #[test]
    fn error_on_zero_quote_amount() {
        let result = build_replay_decay_array("0", &[(1, "990")]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::ZeroQuoteOutput));
    }

    #[test]
    fn error_on_inf_quote_amount() {
        let result = build_replay_decay_array("inf", &[(1, "990")]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::InvalidAmount { .. }));
    }

    #[test]
    fn invalid_replay_for_one_offset_yields_none_not_error() {
        let replays = vec![(1, "990"), (2, "bad"), (3, "970")];
        let result = build_replay_decay_array("1000", &replays)
            .expect("should not fail for per-block replay errors");

        assert!(result.decay_bps[0].is_some(), "k=1 valid");
        assert!(result.decay_bps[1].is_none(), "k=2 invalid replay");
        assert!(result.decay_bps[2].is_some(), "k=3 valid");
        assert_eq!(result.results.len(), 2);
    }

    #[test]
    fn all_replays_invalid_yields_all_none() {
        let replays: Vec<(u32, &str)> = (1..=10)
            .map(|k| (k, "invalid"))
            .collect();
        let result =
            build_replay_decay_array("1000", &replays).expect("invalid replays are graceful");

        assert!(result
            .decay_bps
            .iter()
            .all(|v| v.is_none()));
        assert!(result.results.is_empty());
    }

    // ── Negative decay (improvement) ─────────────────────────────

    #[test]
    fn negative_decay_propagates_through_array() {
        let replays = vec![(1, "1010"), (2, "1020")];
        let array = build_replay_decay_array("1000", &replays).expect("valid inputs");

        assert!(
            array.decay_bps[0].expect("k=1") < 0.0,
            "k=1 should show improvement (negative decay)"
        );
        assert!(
            array.decay_bps[1].expect("k=2") < 0.0,
            "k=2 should show improvement (negative decay)"
        );
    }

    // ── Monotonically increasing decay scenario ──────────────────

    #[test]
    fn monotonic_decay_preserved_in_array_order() {
        let replay_amounts = ["999", "998", "997", "996", "995", "994", "993", "992", "991", "990"];
        let replays: Vec<(u32, &str)> = (1..=10)
            .zip(replay_amounts.iter().copied())
            .collect();

        let array = build_replay_decay_array("1000", &replays).expect("valid inputs");

        for idx in 0..9 {
            let curr = array.decay_bps[idx].expect("populated");
            let next = array.decay_bps[idx + 1].expect("populated");
            assert!(
                next > curr,
                "decay should increase: k={} ({curr}) < k={} ({next})",
                idx + 1,
                idx + 2
            );
        }
    }

    // ── Duplicate offsets ────────────────────────────────────────

    #[test]
    fn first_replay_for_duplicate_offset_wins() {
        let replays = vec![(1, "990"), (1, "500")];
        let array = build_replay_decay_array("1000", &replays).expect("valid inputs");

        // find() returns first match, so k=1 uses "990"
        let expected = compute_single_block_decay("1000", "990", 1).expect("valid");
        let actual = array.decay_bps[0].expect("populated");
        assert!((actual - expected.decay_bps).abs() < 1e-10, "first replay for offset should win");
    }

    // ── Large token amounts ──────────────────────────────────────

    #[test]
    fn large_token_amounts_through_array() {
        let replays: Vec<(u32, &str)> = (1..=10)
            .map(|k| (k, "3497500000"))
            .collect();
        let array = build_replay_decay_array("3500000000", &replays).expect("valid inputs");

        assert_eq!(array.results.len(), 10);
        for val in &array.decay_bps {
            let bps = val.expect("populated");
            let expected_bps = (3_500_000_000.0 - 3_497_500_000.0) / 3_500_000_000.0 * 10_000.0;
            assert!((bps - expected_bps).abs() < 1e-6, "expected {expected_bps} bps, got {bps}");
        }
    }

    // ══════════════════════════════════════════════════════════════
    // build_decay_from_replay_outcomes tests
    // ══════════════════════════════════════════════════════════════

    // ── Route-invalid at specific offsets yields None ────────────

    #[test]
    fn pool_drained_at_k5_yields_none_for_k5_only() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("990")),
            (2, ReplayOutcome::Valid("980")),
            (3, ReplayOutcome::Valid("970")),
            (4, ReplayOutcome::Valid("960")),
            (5, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (6, ReplayOutcome::Valid("940")),
            (7, ReplayOutcome::Valid("930")),
            (8, ReplayOutcome::Valid("920")),
            (9, ReplayOutcome::Valid("910")),
            (10, ReplayOutcome::Valid("900")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert_eq!(array.decay_bps.len(), 10);
        for idx in 0..10 {
            if idx == 4 {
                assert!(
                    array.decay_bps[idx].is_none(),
                    "k=5 (pool drained) should be None"
                );
            } else {
                assert!(
                    array.decay_bps[idx].is_some(),
                    "k={} should be populated",
                    idx + 1
                );
            }
        }
        assert_eq!(array.results.len(), 9, "9 valid offsets");
    }

    #[test]
    fn token_deregistered_at_k3_leaves_earlier_blocks_valid() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("990")),
            (2, ReplayOutcome::Valid("980")),
            (3, ReplayOutcome::RouteInvalid(RouteInvalidReason::TokenDeregistered)),
            (4, ReplayOutcome::Valid("960")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_some(), "k=1 valid");
        assert!(array.decay_bps[1].is_some(), "k=2 valid");
        assert!(array.decay_bps[2].is_none(), "k=3 token deregistered");
        assert!(array.decay_bps[3].is_some(), "k=4 valid");
        assert_eq!(array.results.len(), 3);
    }

    #[test]
    fn insufficient_liquidity_produces_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("990")),
            (2, ReplayOutcome::RouteInvalid(
                RouteInvalidReason::InsufficientLiquidity,
            )),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_some(), "k=1 valid");
        assert!(array.decay_bps[1].is_none(), "k=2 insufficient liquidity");
    }

    #[test]
    fn other_reason_produces_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::RouteInvalid(RouteInvalidReason::Other(
                "oracle stale".to_owned(),
            ))),
            (2, ReplayOutcome::Valid("980")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_none(), "k=1 other reason");
        assert!(array.decay_bps[1].is_some(), "k=2 valid");
    }

    // ── Multiple consecutive invalid blocks ─────────────────────

    #[test]
    fn consecutive_invalid_blocks_all_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("995")),
            (2, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (3, ReplayOutcome::RouteInvalid(
                RouteInvalidReason::InsufficientLiquidity,
            )),
            (4, ReplayOutcome::RouteInvalid(RouteInvalidReason::TokenDeregistered)),
            (5, ReplayOutcome::Valid("975")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_some(), "k=1 valid");
        assert!(array.decay_bps[1].is_none(), "k=2 pool drained");
        assert!(array.decay_bps[2].is_none(), "k=3 insufficient liquidity");
        assert!(array.decay_bps[3].is_none(), "k=4 token deregistered");
        assert!(array.decay_bps[4].is_some(), "k=5 valid");
        assert_eq!(array.results.len(), 2);
    }

    // ── All offsets invalid ─────────────────────────────────────

    #[test]
    fn all_offsets_route_invalid_yields_all_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = (1..=10)
            .map(|k| (k, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)))
            .collect();
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(
            array.decay_bps.iter().all(|v| v.is_none()),
            "all slots should be None when all routes invalid"
        );
        assert!(array.results.is_empty());
    }

    // ── All offsets valid (baseline) ────────────────────────────

    #[test]
    fn all_offsets_valid_via_outcomes() {
        let outcomes: Vec<(u32, ReplayOutcome)> = (1..=10)
            .map(|k| (k, ReplayOutcome::Valid("990")))
            .collect();
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(
            array.decay_bps.iter().all(|v| v.is_some()),
            "all slots should be populated"
        );
        assert_eq!(array.results.len(), 10);
    }

    // ── Valid blocks unaffected by neighboring invalid blocks ────

    #[test]
    fn valid_decay_values_unaffected_by_neighboring_invalid() {
        let outcomes_mixed: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("990")),
            (2, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (3, ReplayOutcome::Valid("970")),
        ];
        let array_mixed = build_decay_from_replay_outcomes("1000", &outcomes_mixed)
            .expect("valid quote");

        // Compute reference values without any invalid blocks
        let k1_ref = compute_single_block_decay("1000", "990", 1).expect("valid");
        let k3_ref = compute_single_block_decay("1000", "970", 3).expect("valid");

        let k1_actual = array_mixed.decay_bps[0].expect("k=1 populated");
        let k3_actual = array_mixed.decay_bps[2].expect("k=3 populated");

        assert!(
            (k1_actual - k1_ref.decay_bps).abs() < 1e-10,
            "k=1 decay should match reference: expected {}, got {k1_actual}",
            k1_ref.decay_bps
        );
        assert!(
            (k3_actual - k3_ref.decay_bps).abs() < 1e-10,
            "k=3 decay should match reference: expected {}, got {k3_actual}",
            k3_ref.decay_bps
        );
    }

    // ── Mix of route-invalid and parse-error (invalid amount) ───

    #[test]
    fn mix_of_route_invalid_and_parse_error_both_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("990")),
            (2, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (3, ReplayOutcome::Valid("not_a_number")),
            (4, ReplayOutcome::Valid("960")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_some(), "k=1 valid amount");
        assert!(array.decay_bps[1].is_none(), "k=2 route invalid");
        assert!(array.decay_bps[2].is_none(), "k=3 unparseable amount");
        assert!(array.decay_bps[3].is_some(), "k=4 valid amount");
        assert_eq!(array.results.len(), 2);
    }

    // ── Missing offsets (not provided) remain None ──────────────

    #[test]
    fn missing_and_invalid_offsets_both_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("990")),
            (3, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (5, ReplayOutcome::Valid("950")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_some(), "k=1 valid");
        assert!(array.decay_bps[1].is_none(), "k=2 not provided");
        assert!(array.decay_bps[2].is_none(), "k=3 route invalid");
        assert!(array.decay_bps[3].is_none(), "k=4 not provided");
        assert!(array.decay_bps[4].is_some(), "k=5 valid");
    }

    // ── Error propagation for invalid quote amount ──────────────

    #[test]
    fn outcomes_error_on_invalid_quote_amount() {
        let outcomes: Vec<(u32, ReplayOutcome)> =
            vec![(1, ReplayOutcome::Valid("990"))];
        let result = build_decay_from_replay_outcomes("bad_quote", &outcomes);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DecayError::InvalidAmount { .. }
        ));
    }

    #[test]
    fn outcomes_error_on_zero_quote_amount() {
        let outcomes: Vec<(u32, ReplayOutcome)> =
            vec![(1, ReplayOutcome::Valid("990"))];
        let result = build_decay_from_replay_outcomes("0", &outcomes);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DecayError::ZeroQuoteOutput));
    }

    // ── Out-of-range offsets ignored ────────────────────────────

    #[test]
    fn outcomes_out_of_range_offsets_ignored() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (0, ReplayOutcome::Valid("999")),
            (1, ReplayOutcome::Valid("990")),
            (11, ReplayOutcome::Valid("880")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert_eq!(array.results.len(), 1);
        assert_eq!(array.results[0].block_offset, 1);
    }

    // ── Empty outcomes list ─────────────────────────────────────

    #[test]
    fn outcomes_empty_list_yields_all_none() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps.iter().all(|v| v.is_none()));
        assert!(array.results.is_empty());
    }

    // ── Array always has exactly 10 elements ────────────────────

    #[test]
    fn outcomes_array_always_10_elements() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (3, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (7, ReplayOutcome::Valid("930")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert_eq!(
            array.decay_bps.len(),
            MAX_BLOCK_OFFSET as usize,
            "array must always have exactly 10 elements"
        );
    }

    // ── Route-invalid reason display ────────────────────────────

    #[test]
    fn route_invalid_reason_display_messages() {
        assert_eq!(RouteInvalidReason::PoolDrained.to_string(), "pool drained");
        assert_eq!(
            RouteInvalidReason::TokenDeregistered.to_string(),
            "token deregistered"
        );
        assert_eq!(
            RouteInvalidReason::InsufficientLiquidity.to_string(),
            "insufficient liquidity"
        );
        assert_eq!(
            RouteInvalidReason::Other("oracle stale".to_owned()).to_string(),
            "oracle stale"
        );
    }

    // ── Late-onset invalidity (valid early, invalid late) ───────

    #[test]
    fn route_degrades_then_becomes_invalid() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::Valid("995")),
            (2, ReplayOutcome::Valid("985")),
            (3, ReplayOutcome::Valid("970")),
            (4, ReplayOutcome::Valid("950")),
            (5, ReplayOutcome::Valid("920")),
            (6, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (7, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (8, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (9, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
            (10, ReplayOutcome::RouteInvalid(RouteInvalidReason::PoolDrained)),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        for idx in 0..5 {
            assert!(
                array.decay_bps[idx].is_some(),
                "k={} should be valid (early blocks)",
                idx + 1
            );
        }
        for idx in 5..10 {
            assert!(
                array.decay_bps[idx].is_none(),
                "k={} should be None (route invalid)",
                idx + 1
            );
        }
        assert_eq!(array.results.len(), 5);

        // Verify early blocks have increasing decay
        for idx in 0..4 {
            let curr = array.decay_bps[idx].expect("populated");
            let next = array.decay_bps[idx + 1].expect("populated");
            assert!(
                next > curr,
                "decay should increase: k={} ({curr}) < k={} ({next})",
                idx + 1,
                idx + 2
            );
        }
    }

    // ── First block immediately invalid ─────────────────────────

    #[test]
    fn first_block_invalid_rest_valid() {
        let outcomes: Vec<(u32, ReplayOutcome)> = vec![
            (1, ReplayOutcome::RouteInvalid(
                RouteInvalidReason::TokenDeregistered,
            )),
            (2, ReplayOutcome::Valid("980")),
            (3, ReplayOutcome::Valid("970")),
        ];
        let array =
            build_decay_from_replay_outcomes("1000", &outcomes).expect("valid quote");

        assert!(array.decay_bps[0].is_none(), "k=1 route invalid");
        assert!(array.decay_bps[1].is_some(), "k=2 valid");
        assert!(array.decay_bps[2].is_some(), "k=3 valid");
        assert_eq!(array.results.len(), 2);
    }
}
