//! ParaSwap-specific calldata extraction.
//!
//! Augustus v6 swap methods carry the trade parameters as consecutive 32-byte words:
//! `…, fromAmount, toAmount, quotedAmount, …`. `toAmount` is the slippage floor and
//! `quotedAmount` the off-chain quoted output, kept on-chain for `ParaSwap`'s surplus accounting
//! (the user is capped at the quote, so settled often equals it exactly). The triple sits at a
//! different offset per method selector, so instead of per-selector ABI decoding it is located
//! by value: find the word equal to the trade's decoded input amount, then read the
//! floor-and-quote pair that follows it.

use alloy::primitives::{Address, U256};

use crate::decoder::solvers::{SolverKnowledge, SolverQuote};

/// The `ParaSwap` solver.
pub(crate) struct Paraswap;

impl SolverKnowledge for Paraswap {
    /// Extract `ParaSwap`'s `quotedAmount` from Augustus calldata.
    ///
    /// Scans word-aligned calldata for `amount_in` and reads the two words after it as
    /// `(toAmount, quotedAmount)`. A false positive would need a word that equals the exact
    /// input amount *and* is followed by a plausible floor/quote pair (`0 < toAmount <=
    /// quotedAmount <= 2 * toAmount`) — and the caller's settled-amount plausibility check
    /// still applies after. Returns `None` when no such triple exists (e.g. a partner fee made
    /// the decoded input differ from `fromAmount`), which just leaves the record without a
    /// quote.
    fn embedded_quote(&self, input: &[u8], amount_in: U256) -> Option<SolverQuote> {
        let (_, quoted) = locate_amount_triple(input, amount_in)?;
        Some(SolverQuote { amount_out: quoted, source: None, timestamp: None })
    }

    /// Extract `ParaSwap`'s slippage floor: `toAmount`, the middle word of the Augustus v6
    /// `(fromAmount, toAmount, quotedAmount)` triple, located by matching `fromAmount` against
    /// the decoded input amount the way `embedded_quote` does.
    fn min_amount_out(&self, input: &[u8], amount_in: U256, _token_out: Address) -> Option<U256> {
        let (to_amount, _) = locate_amount_triple(input, amount_in)?;
        Some(to_amount)
    }
}

/// Locate the Augustus v6 `(fromAmount, toAmount, quotedAmount)` triple by value and return
/// `(toAmount, quotedAmount)`.
///
/// Scans word-aligned calldata for `amount_in` and reads the two words after it. A false
/// positive would need a word that equals the exact input amount *and* is followed by a
/// plausible floor/quote pair (`0 < toAmount <= quotedAmount <= 2 * toAmount`) — and the
/// caller's settled-amount plausibility check still applies after. Returns `None` when no such
/// triple exists (e.g. a partner fee made the decoded input differ from `fromAmount`).
fn locate_amount_triple(input: &[u8], amount_in: U256) -> Option<(U256, U256)> {
    if amount_in.is_zero() || input.len() < 4 {
        return None;
    }
    let words: Vec<U256> = input[4..]
        .as_chunks::<32>()
        .0
        .iter()
        .map(|word| U256::from_be_slice(word))
        .collect();
    for window in words.windows(3) {
        let (from_amount, to_amount, quoted) = (window[0], window[1], window[2]);
        if from_amount != amount_in || to_amount.is_zero() {
            continue;
        }
        if quoted >= to_amount && quoted <= to_amount.saturating_mul(U256::from(2)) {
            return Some((to_amount, quoted));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Word-aligned Augustus-style calldata: selector, then 32-byte words.
    fn calldata(words: &[U256]) -> Vec<u8> {
        let mut input = vec![0xe3u8, 0xea, 0xd5, 0x9e];
        for word in words {
            input.extend_from_slice(&word.to_be_bytes::<32>());
        }
        input
    }

    #[test]
    fn test_real_swap_exact_amount_in_layout() {
        // Live tx 0x1192b394… (block range of run5): srcToken, destToken, fromAmount, toAmount
        // (floor, -10bps), quotedAmount — the settled amount was 171,602,265, one unit under the
        // quote (ParaSwap caps the user at the quote and keeps the surplus).
        let words = [
            U256::from(0x1111u64), // srcToken
            U256::from(0x2222u64), // destToken
            U256::from(171_521_496u64),
            U256::from(171_430_663u64),
            U256::from(171_602_266u64),
            U256::ZERO, // metadata
        ];
        let quote = Paraswap
            .embedded_quote(&calldata(&words), U256::from(171_521_496u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(171_602_266u64));
        assert_eq!(quote.source, None);
        assert_eq!(quote.timestamp, None);
    }

    #[test]
    fn test_missing_input_amount() {
        // A partner fee (or any decode difference) means no word equals the decoded input.
        let words =
            [U256::from(171_521_496u64), U256::from(171_430_663u64), U256::from(171_602_266u64)];
        assert!(Paraswap
            .embedded_quote(&calldata(&words), U256::from(999u64))
            .is_none());
        assert!(Paraswap
            .embedded_quote(&[], U256::from(1u64))
            .is_none());
        assert!(Paraswap
            .embedded_quote(&calldata(&words), U256::ZERO)
            .is_none());
    }

    #[test]
    fn test_min_amount_out_reads_the_floor() {
        // Same live-tx layout as the quote test: the floor is `toAmount`, the middle word.
        let words = [
            U256::from(0x1111u64), // srcToken
            U256::from(0x2222u64), // destToken
            U256::from(171_521_496u64),
            U256::from(171_430_663u64),
            U256::from(171_602_266u64),
            U256::ZERO, // metadata
        ];
        assert_eq!(
            Paraswap.min_amount_out(&calldata(&words), U256::from(171_521_496u64), Address::ZERO),
            Some(U256::from(171_430_663u64))
        );
    }

    #[test]
    fn test_min_amount_out_absent_without_triple() {
        let words =
            [U256::from(171_521_496u64), U256::from(171_430_663u64), U256::from(171_602_266u64)];
        assert!(Paraswap
            .min_amount_out(&calldata(&words), U256::from(999u64), Address::ZERO)
            .is_none());
        assert!(Paraswap
            .min_amount_out(&[], U256::from(1u64), Address::ZERO)
            .is_none());
    }

    #[test]
    fn test_implausible_floor_quote_pair() {
        // The words after the input match are not a floor/quote pair: quote below the floor, or
        // wildly above it (different units).
        let amount_in = U256::from(1_000_000u64);
        let below = [amount_in, U256::from(990_000u64), U256::from(400_000u64)];
        assert!(Paraswap
            .embedded_quote(&calldata(&below), amount_in)
            .is_none());
        let far_above = [amount_in, U256::from(990_000u64), U256::from(10_000_000u64)];
        assert!(Paraswap
            .embedded_quote(&calldata(&far_above), amount_in)
            .is_none());
    }
}
