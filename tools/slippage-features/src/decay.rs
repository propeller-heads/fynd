use num_bigint::BigInt;
use num_traits::ToPrimitive;
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
///
/// Uses integer arithmetic for the difference to preserve precision for
/// 256-bit token amounts, converting only the final ratio to f64.
/// Positive = route degraded, negative = route improved.
pub fn compute_decay_bps(
    quote_amount_out: &str,
    replay_amount_out: &str,
) -> Result<f64, DecayError> {
    let quote = parse_bigint(quote_amount_out)?;
    let replay = parse_bigint(replay_amount_out)?;
    if quote.sign() == num_bigint::Sign::NoSign {
        return Err(DecayError::ZeroQuoteOutput);
    }
    let diff = &quote - &replay;
    let numer = diff * BigInt::from(10_000);
    let bps = numer.to_f64().unwrap_or(f64::NAN)
        / quote.to_f64().unwrap_or(f64::NAN);
    Ok(bps)
}

fn parse_bigint(s: &str) -> Result<BigInt, DecayError> {
    s.parse::<BigInt>().map_err(|_| DecayError::InvalidAmount {
        value: s.to_string(),
        reason: "not a valid integer".into(),
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
        assert!(matches!(compute_decay_bps("0", "100"), Err(DecayError::ZeroQuoteOutput)));
    }
}
