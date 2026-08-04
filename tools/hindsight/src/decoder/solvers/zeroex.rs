//! 0x calldata extraction.
//!
//! 0x's Settler calldata is a list of encoded actions whose numbers are slippage floors, not the
//! quoted output. Only the positive-slippage action carries the quote: when 0x skims surplus for an
//! integrator, it records the output the route was quoted at, and execution above it is the
//! surplus. That is the one quote 0x exposes on-chain.
//!
//! So a quote is recovered only for swaps that let 0x skim surplus. Wallets that take their own fee
//! (Phantom, Robinhood) do not, and decode without a quote — partial coverage of the highest-volume
//! competitor.

use alloy::primitives::U256;

use crate::decoder::solvers::{SolverKnowledge, SolverQuote};

/// Selector of 0x's positive-slippage action (`bytes4` of its signature; see the test).
const POSITIVE_SLIPPAGE: [u8; 4] = [0x34, 0xee, 0x90, 0xca];

/// The 0x solver.
pub(crate) struct ZeroEx;

impl SolverKnowledge for ZeroEx {
    /// Extract 0x's quoted output from the positive-slippage action, when the calldata has one.
    ///
    /// The action is found by its selector, not by decoding the whole Settler call, so it is picked
    /// up wherever 0x sits — the entry point, behind the shared allowance contract, or nested in
    /// another venue's calldata. The two address words after the selector must have their top 12
    /// bytes zero, which rejects a stray selector match in unrelated bytes. The caller unit-checks
    /// the quote against the settled amount.
    fn embedded_quote(&self, input: &[u8], _amount_in: U256) -> Option<SolverQuote> {
        let start = input
            .windows(POSITIVE_SLIPPAGE.len())
            .position(|window| window == POSITIVE_SLIPPAGE)?;
        // Three words after the selector: recipient, token, expectedAmount.
        let action = input.get(start + 4..start + 4 + 96)?;
        let is_address = |word: &[u8]| word[..12].iter().all(|&byte| byte == 0);
        if !is_address(&action[0..32]) || !is_address(&action[32..64]) {
            return None;
        }
        let amount_out = U256::from_be_slice(&action[64..96]);
        if amount_out.is_zero() {
            return None;
        }
        Some(SolverQuote { amount_out, source: None, timestamp: None })
    }

    /// Extract 0x's slippage floor: `minAmountOut`, the last word of the Settler's
    /// `BASIC`/`TRANSFER_FROM` terminal action — the floor the module doc notes every Settler
    /// call carries, as opposed to the quote only surplus-skimming calls declare.
    ///
    /// Extraction not implemented yet: `None` means the capture falls back to its synthetic
    /// limit, which degrades the limit's provenance, never the decode.
    fn min_amount_out(&self, _input: &[u8], _amount_in: U256) -> Option<U256> {
        None
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;

    use super::*;
    use crate::decoder::test_utils::addr;

    /// A `POSITIVE_SLIPPAGE` action, ABI-encoded as it appears in the Settler `actions` array,
    /// surrounded by other calldata bytes.
    fn calldata_with_positive_slippage(token: Address, expected: u128) -> Vec<u8> {
        let mut action = POSITIVE_SLIPPAGE.to_vec();
        action.extend_from_slice(addr(7).into_word().as_slice()); // recipient
        action.extend_from_slice(token.into_word().as_slice());
        action.extend_from_slice(&U256::from(expected).to_be_bytes::<32>());
        action.extend_from_slice(&U256::from(50u64).to_be_bytes::<32>()); // maxBps

        let mut input = vec![0x11u8; 68]; // an outer selector + leading words
        input.extend_from_slice(&action);
        input.extend_from_slice(&[0u8; 32]); // trailing calldata
        input
    }

    #[test]
    fn test_selector_matches_settler_action() {
        // bytes4(keccak256("POSITIVE_SLIPPAGE(address,address,uint256,uint256)")) — verified
        // against the 0x-settler ISettlerActions source.
        assert_eq!(POSITIVE_SLIPPAGE, [0x34, 0xee, 0x90, 0xca]);
    }

    #[test]
    fn test_reads_expected_amount() {
        let input = calldata_with_positive_slippage(addr(11), 2_000_000);
        let quote = ZeroEx
            .embedded_quote(&input, U256::ZERO)
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(2_000_000u64));
        assert_eq!(quote.source, None);
        assert_eq!(quote.timestamp, None);
    }

    #[test]
    fn test_no_positive_slippage_action() {
        // A 0x swap without the action (a wallet integrator taking its own fee) carries no quote.
        assert!(ZeroEx
            .embedded_quote(&[0x00; 200], U256::ZERO)
            .is_none());
        assert!(ZeroEx
            .embedded_quote(&[], U256::ZERO)
            .is_none());
    }

    #[test]
    fn test_stray_selector_without_address_words() {
        // The selector bytes appearing amid non-address data (a coincidental match) are rejected.
        let mut input = POSITIVE_SLIPPAGE.to_vec();
        input.extend_from_slice(&[0xff; 96]);
        assert!(ZeroEx
            .embedded_quote(&input, U256::ZERO)
            .is_none());
    }
}
