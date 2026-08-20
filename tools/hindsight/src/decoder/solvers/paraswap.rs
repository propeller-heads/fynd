//! ParaSwap-specific calldata extraction.
//!
//! Augustus v6 swap methods carry the trade parameters as consecutive 32-byte words:
//! `…, srcToken, destToken, …, fromAmount, toAmount, quotedAmount, …`. `toAmount` is the
//! slippage floor and `quotedAmount` the off-chain quoted output, kept on-chain for `ParaSwap`'s
//! surplus accounting (the user is capped at the quote, so settled often equals it exactly). The
//! triple sits at a different offset per method selector, so instead of per-selector ABI decoding
//! it is located by value: find the word equal to the trade's known input amount — the caller's
//! hint, there is no other way to find it — then read the floor-and-quote pair that follows it and
//! the token pair that precedes it.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
};

use crate::decoder::solvers::{Declaration, SolverDecoder, SwapIntent};

/// Byte length of an ABI-encoded word.
const WORD_LEN: usize = 32;
/// A `bytes32` word encoding an `address` has 12 zero prefix bytes, then the 20-byte address.
const ADDRESS_PREFIX_LEN: usize = 12;

/// Whether a word is address-shaped: 12 zero prefix bytes and a non-zero address. Rejects a
/// coincidental match against unrelated calldata bytes.
fn is_address_word(word: U256) -> bool {
    let bytes = word.to_be_bytes::<WORD_LEN>();
    bytes[..ADDRESS_PREFIX_LEN]
        .iter()
        .all(|&byte| byte == 0) &&
        bytes[ADDRESS_PREFIX_LEN..]
            .iter()
            .any(|&byte| byte != 0)
}

/// The address encoded in a word already known to be address-shaped.
fn address_from_word(word: U256) -> Address {
    Address::from_slice(&word.to_be_bytes::<WORD_LEN>()[ADDRESS_PREFIX_LEN..])
}

/// The `ParaSwap` solver.
pub(crate) struct Paraswap;

impl SolverDecoder for Paraswap {
    /// Extract the trader's swap terms from Augustus calldata: the enforced floor and declared
    /// quote by scanning for the word equal to `amount_in_hint`, the tokens from the two words
    /// immediately preceding it.
    ///
    /// A false positive would need a word that equals the exact input amount *and* is followed
    /// by a plausible floor/quote pair (`0 < toAmount <= quotedAmount <= 2 * toAmount`) *and*
    /// preceded by two address-shaped words — and the caller's settled-amount plausibility check
    /// still applies to the quote afterward. Returns `None` when no such shape exists (e.g. a
    /// partner fee made the decoded input differ from `fromAmount`, or the words before it are
    /// not a token pair) — the intent is lost along with the quote, since there is nothing left
    /// to recover the floor from. A reverted trade has no netted flow to draw a hint from, so
    /// `amount_in_hint: None` always yields `None`.
    fn declared(
        &self,
        input: &[u8],
        _logs: &[Log],
        amount_in_hint: Option<U256>,
    ) -> Option<Declaration> {
        let amount_in = amount_in_hint.filter(|hint| !hint.is_zero())?;
        if input.len() < 4 {
            return None;
        }
        let words: Vec<U256> = input[4..]
            .as_chunks::<WORD_LEN>()
            .0
            .iter()
            .map(|word| U256::from_be_slice(word))
            .collect();
        for (index, window) in words.windows(3).enumerate() {
            let (from_amount, to_amount, quoted) = (window[0], window[1], window[2]);
            if from_amount != amount_in || to_amount.is_zero() {
                continue;
            }
            if quoted < to_amount || quoted > to_amount.saturating_mul(U256::from(2)) {
                continue;
            }
            if index < 2 {
                continue;
            }
            let (src_token, dst_token) = (words[index - 2], words[index - 1]);
            if !is_address_word(src_token) || !is_address_word(dst_token) {
                continue;
            }
            let intent = SwapIntent::new(
                address_from_word(src_token),
                address_from_word(dst_token),
                amount_in,
                to_amount,
            );
            return Some(Declaration::Terms(intent.with_quote(quoted, None)));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    /// The terms this solver reads from `input`, for tests that only care about the calldata path.
    fn terms(input: &[u8], hint: Option<U256>) -> Option<SwapIntent> {
        match Paraswap.declared(input, &[], hint)? {
            Declaration::Terms(intent) => Some(intent),
            Declaration::Settled(_) => None,
        }
    }

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
        let src_token = U256::from(0x1111u64);
        let dst_token = U256::from(0x2222u64);
        let amount_in = U256::from(171_521_496u64);
        let words = [
            src_token,
            dst_token,
            amount_in,
            U256::from(171_430_663u64),
            U256::from(171_602_266u64),
            U256::ZERO, // metadata
        ];
        let intent = terms(&calldata(&words), Some(amount_in)).unwrap();
        assert_eq!(intent.token_in, address_from_word(src_token));
        assert_eq!(intent.token_out, address_from_word(dst_token));
        assert_eq!(intent.amount_in, amount_in);
        assert_eq!(intent.min_amount_out, U256::from(171_430_663u64));
        assert_eq!(intent.quoted_amount_out(), U256::from(171_602_266u64));
    }

    #[test]
    fn test_missing_input_amount() {
        // A partner fee (or any decode difference) means no word equals the decoded input.
        let words = [
            U256::from(0x1111u64),
            U256::from(0x2222u64),
            U256::from(171_521_496u64),
            U256::from(171_430_663u64),
            U256::from(171_602_266u64),
        ];
        assert!(terms(&calldata(&words), Some(U256::from(999u64))).is_none());
        assert!(terms(&[], Some(U256::from(1u64))).is_none());
        assert!(terms(&calldata(&words), Some(U256::ZERO)).is_none());
    }

    #[test]
    fn test_no_hint_never_yields_an_intent() {
        // The revert path: no netted flow means no hint, so there is no way to locate the
        // triple even when the calldata is otherwise well-formed.
        let words = [
            U256::from(0x1111u64),
            U256::from(0x2222u64),
            U256::from(171_521_496u64),
            U256::from(171_430_663u64),
            U256::from(171_602_266u64),
        ];
        assert!(terms(&calldata(&words), None).is_none());
    }

    #[test]
    fn test_implausible_floor_quote_pair() {
        // The words after the input match are not a floor/quote pair: quote below the floor, or
        // wildly above it (different units).
        let amount_in = U256::from(1_000_000u64);
        let below = [
            U256::from(0x1111u64),
            U256::from(0x2222u64),
            amount_in,
            U256::from(990_000u64),
            U256::from(400_000u64),
        ];
        assert!(terms(&calldata(&below), Some(amount_in)).is_none());
        let far_above = [
            U256::from(0x1111u64),
            U256::from(0x2222u64),
            amount_in,
            U256::from(990_000u64),
            U256::from(10_000_000u64),
        ];
        assert!(terms(&calldata(&far_above), Some(amount_in)).is_none());
    }

    #[test]
    fn test_non_address_shaped_tokens_rejected() {
        // The words before fromAmount are not address-shaped (top bytes set): the intent is
        // dropped even though the floor/quote pair itself is plausible — there is nothing to
        // build a trustworthy token pair from.
        let amount_in = U256::from(1_000_000u64);
        let words = [
            U256::MAX, // not address-shaped: every byte set
            U256::from(0x2222u64),
            amount_in,
            U256::from(990_000u64),
            U256::from(995_000u64),
        ];
        assert!(terms(&calldata(&words), Some(amount_in)).is_none());
    }

    #[test]
    fn test_fromamount_too_early_for_token_words() {
        // fromAmount at the very start of the calldata: no room for the two preceding token
        // words, even though the hint matches and the floor/quote pair is plausible.
        let amount_in = U256::from(1_000_000u64);
        let words = [amount_in, U256::from(990_000u64), U256::from(995_000u64)];
        assert!(terms(&calldata(&words), Some(amount_in)).is_none());
    }
}
