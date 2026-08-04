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

use alloy::primitives::{address, Address, U256};

use crate::decoder::solvers::{SolverKnowledge, SolverQuote};

/// Selector of 0x's positive-slippage action (`bytes4` of its signature; see the test).
const POSITIVE_SLIPPAGE: [u8; 4] = [0x34, 0xee, 0x90, 0xca];

/// Settler entrypoints whose first parameter is the `AllowedSlippage` struct:
/// `execute((address,address,uint256),bytes[],bytes32)`,
/// `executeWithPermit((address,address,uint256),bytes[],bytes32,bytes)`, and
/// `executeMetaTxn((address,address,uint256),bytes[],bytes32,address,bytes)` (shared by
/// `SettlerMetaTxn` and `SettlerIntent`). Selectors verified against 0x-settler source.
const SETTLER_SELECTORS: [[u8; 4]; 3] =
    [[0x1f, 0xff, 0x99, 0x1f], [0x06, 0xb8, 0x52, 0x4c], [0xfd, 0x3a, 0xd6, 0xd4]];

/// Selector of `AllowanceHolder.exec(address,address,uint256,address,bytes)` — the shared
/// allowance contract that forwards its `data` parameter verbatim to a Settler.
const ALLOWANCE_HOLDER_EXEC: [u8; 4] = [0x22, 0x13, 0xbc, 0x0b];

/// The sentinel Settler uses for native-ETH settlement (`SettlerAbstract.ETH_ADDRESS`).
const SETTLER_ETH: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

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

    /// Extract 0x's slippage floor: `AllowedSlippage.minAmountOut`, the third head word of every
    /// Settler entrypoint — `(recipient, buyToken, minAmountOut)` is a static tuple inlined at
    /// calldata bytes `[4..100)`, identical across `execute`, `executeWithPermit`, and
    /// `executeMetaTxn`, and preserved verbatim when the call is wrapped in
    /// `AllowanceHolder.exec`.
    ///
    /// The floor is accepted only when the declared `buyToken` is the decoded trade's
    /// `token_out` (or Settler's native-ETH sentinel, whose floor is in wei — the same units the
    /// wrapped-native record uses): the struct covers exactly one settlement token, so a floor
    /// declared for a different token is not this trade's limit. `minAmountOut == 0` is
    /// Settler's own skip sentinel (no check performed) and carries no floor.
    fn min_amount_out(&self, input: &[u8], _amount_in: U256, token_out: Address) -> Option<U256> {
        let settler_input = settler_call(input)?;
        let slippage_head = settler_input.get(4..100)?;
        let buy_token_word = &slippage_head[32..64];
        if buy_token_word[..12]
            .iter()
            .any(|&byte| byte != 0)
        {
            return None;
        }
        let buy_token = Address::from_slice(&buy_token_word[12..]);
        if buy_token != token_out && buy_token != SETTLER_ETH {
            return None;
        }
        let min_amount_out = U256::from_be_slice(&slippage_head[64..96]);
        if min_amount_out.is_zero() {
            return None;
        }
        Some(min_amount_out)
    }
}

/// The Settler call within `input`: `input` itself when it starts with a Settler entrypoint
/// selector, or the verbatim `data` parameter when `input` is an `AllowanceHolder.exec` call.
/// `None` when 0x is nested some other way (e.g. inside another venue's calldata) — the fixed
/// slippage-struct offsets are only trustworthy behind a matched entrypoint selector.
fn settler_call(input: &[u8]) -> Option<&[u8]> {
    let selector = input.get(..4)?;
    if SETTLER_SELECTORS
        .iter()
        .any(|settler| selector == settler)
    {
        return Some(input);
    }
    if selector != ALLOWANCE_HOLDER_EXEC {
        return None;
    }
    // exec's head: operator, token, amount, target, offset-of-data — five 32-byte words. The
    // offset is relative to the start of the arguments (byte 4).
    let data_offset_word = input.get(4 + 32 * 4..4 + 32 * 5)?;
    let data_offset = usize::try_from(U256::from_be_slice(data_offset_word)).ok()?;
    let length_start = 4usize.checked_add(data_offset)?;
    let length_word = input.get(length_start..length_start + 32)?;
    let data_length = usize::try_from(U256::from_be_slice(length_word)).ok()?;
    let data_start = length_start + 32;
    let inner = input.get(data_start..data_start.checked_add(data_length)?)?;
    let inner_selector = inner.get(..4)?;
    SETTLER_SELECTORS
        .iter()
        .any(|settler| inner_selector == settler)
        .then_some(inner)
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

    /// A Settler `execute`-shaped call: selector, then the inline `AllowedSlippage` head
    /// `(recipient, buyToken, minAmountOut)`, the `actions` offset, `zid`, and an empty
    /// `actions` array.
    fn settler_calldata(selector: [u8; 4], buy_token: Address, min_amount_out: u128) -> Vec<u8> {
        let mut input = selector.to_vec();
        input.extend_from_slice(addr(7).into_word().as_slice()); // recipient
        input.extend_from_slice(buy_token.into_word().as_slice());
        input.extend_from_slice(&U256::from(min_amount_out).to_be_bytes::<32>());
        input.extend_from_slice(&U256::from(0xa0u64).to_be_bytes::<32>()); // actions offset
        input.extend_from_slice(&[0u8; 32]); // zid
        input.extend_from_slice(&[0u8; 32]); // actions length 0
        input
    }

    /// The Settler call wrapped in `AllowanceHolder.exec(operator, token, amount, target, data)`,
    /// with `data` carried verbatim as the dynamic fifth argument.
    fn allowance_holder_calldata(settler_input: &[u8]) -> Vec<u8> {
        let mut input = ALLOWANCE_HOLDER_EXEC.to_vec();
        input.extend_from_slice(addr(1).into_word().as_slice()); // operator
        input.extend_from_slice(addr(2).into_word().as_slice()); // token
        input.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>()); // amount
        input.extend_from_slice(addr(3).into_word().as_slice()); // target
        input.extend_from_slice(&U256::from(0xa0u64).to_be_bytes::<32>()); // data offset
        input.extend_from_slice(&U256::from(settler_input.len()).to_be_bytes::<32>());
        input.extend_from_slice(settler_input);
        input.extend_from_slice(&[0u8; 28]); // padding to a word boundary
        input
    }

    #[test]
    fn test_selector_matches_settler_action() {
        // bytes4(keccak256("POSITIVE_SLIPPAGE(address,address,uint256,uint256)")) — verified
        // against the 0x-settler ISettlerActions source.
        assert_eq!(POSITIVE_SLIPPAGE, [0x34, 0xee, 0x90, 0xca]);
    }

    #[test]
    fn test_floor_from_every_settler_entrypoint() {
        // execute, executeWithPermit, executeMetaTxn share the inline AllowedSlippage head.
        for selector in SETTLER_SELECTORS {
            let input = settler_calldata(selector, addr(11), 5_000_000);
            assert_eq!(
                ZeroEx.min_amount_out(&input, U256::ZERO, addr(11)),
                Some(U256::from(5_000_000u64))
            );
        }
    }

    #[test]
    fn test_floor_through_allowance_holder() {
        let settler = settler_calldata(SETTLER_SELECTORS[0], addr(11), 5_000_000);
        let input = allowance_holder_calldata(&settler);
        assert_eq!(
            ZeroEx.min_amount_out(&input, U256::ZERO, addr(11)),
            Some(U256::from(5_000_000u64))
        );
    }

    #[test]
    fn test_floor_rejected_for_a_different_buy_token() {
        // The AllowedSlippage struct names one settlement token; a floor for another token is
        // not this trade's limit.
        let input = settler_calldata(SETTLER_SELECTORS[0], addr(11), 5_000_000);
        assert!(ZeroEx
            .min_amount_out(&input, U256::ZERO, addr(12))
            .is_none());
    }

    #[test]
    fn test_native_eth_sentinel_accepted() {
        // Settler settles native ETH via its 0xEeee… sentinel; the floor is in wei — the same
        // units the wrapped-native trade record uses.
        let input = settler_calldata(SETTLER_SELECTORS[0], SETTLER_ETH, 7_000);
        assert_eq!(ZeroEx.min_amount_out(&input, U256::ZERO, addr(11)), Some(U256::from(7_000u64)));
    }

    #[test]
    fn test_zero_floor_is_settlers_skip_sentinel() {
        // minAmountOut == 0 (with buyToken == 0) is Settler's own "no check" encoding; even with
        // a real buyToken a zero floor carries no commitment.
        let skipped = settler_calldata(SETTLER_SELECTORS[0], Address::ZERO, 0);
        assert!(ZeroEx
            .min_amount_out(&skipped, U256::ZERO, Address::ZERO)
            .is_none());
        let zero_floor = settler_calldata(SETTLER_SELECTORS[0], addr(11), 0);
        assert!(ZeroEx
            .min_amount_out(&zero_floor, U256::ZERO, addr(11))
            .is_none());
    }

    #[test]
    fn test_floor_absent_without_a_settler_selector() {
        // An unknown outer selector (0x nested in another venue's calldata) must not be read
        // with the fixed offsets; truncated calldata must not panic.
        let mut wrong_selector = settler_calldata(SETTLER_SELECTORS[0], addr(11), 5_000_000);
        wrong_selector[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert!(ZeroEx
            .min_amount_out(&wrong_selector, U256::ZERO, addr(11))
            .is_none());
        assert!(ZeroEx
            .min_amount_out(&SETTLER_SELECTORS[0], U256::ZERO, addr(11))
            .is_none());
        assert!(ZeroEx
            .min_amount_out(&[], U256::ZERO, addr(11))
            .is_none());
        // AllowanceHolder wrapping a non-Settler payload is rejected by the inner selector gate.
        let input = allowance_holder_calldata(&[0xde, 0xad, 0xbe, 0xef, 0x00]);
        assert!(ZeroEx
            .min_amount_out(&input, U256::ZERO, addr(11))
            .is_none());
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
