use thiserror::Error;

pub const MAX_BLOCK_OFFSET: u32 = fynd_core::observer::MAX_BLOCK_OFFSET;

#[derive(Debug, Error)]
pub enum DecayError {
    #[error("invalid amount '{value}': {reason}")]
    InvalidAmount { value: String, reason: String },
    #[error("quote output is zero; proportional decay is undefined")]
    ZeroQuoteOutput,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecayResult {
    pub block_offset: u32,
    pub decay_bps: f64,
    pub quote_output: f64,
    pub replay_output: f64,
}

/// Compute decay in basis points between a quote output and a replay output.
/// Positive = route degraded, negative = route improved.
/// Formula: (quote_output - replay_output) / quote_output * 10_000
pub fn compute_decay_bps(
    quote_amount_out: &str,
    replay_amount_out: &str,
) -> Result<f64, DecayError> {
    let quote = parse_bigint_to_f64(quote_amount_out)?;
    let replay = parse_bigint_to_f64(replay_amount_out)?;
    if quote == 0.0 {
        return Err(DecayError::ZeroQuoteOutput);
    }
    Ok((quote - replay) / quote * 10_000.0)
}

fn parse_bigint_to_f64(s: &str) -> Result<f64, DecayError> {
    s.parse::<f64>()
        .map_err(|_| DecayError::InvalidAmount {
            value: s.to_string(),
            reason: "not a valid number".into(),
        })
        .and_then(|v| {
            if v.is_finite() {
                Ok(v)
            } else {
                Err(DecayError::InvalidAmount {
                    value: s.to_string(),
                    reason: "not finite".into(),
                })
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_decay_when_output_drops() {
        let bps = compute_decay_bps("1000000", "999000").unwrap();
        assert!((bps - 10.0).abs() < 0.01);
    }

    #[test]
    fn zero_decay_when_unchanged() {
        let bps = compute_decay_bps("1000000", "1000000").unwrap();
        assert!(bps.abs() < 0.001);
    }

    #[test]
    fn negative_decay_when_output_improves() {
        let bps = compute_decay_bps("1000000", "1001000").unwrap();
        assert!(bps < 0.0);
    }

    #[test]
    fn zero_quote_output_is_error() {
        assert!(matches!(
            compute_decay_bps("0", "100"),
            Err(DecayError::ZeroQuoteOutput)
        ));
    }
}
