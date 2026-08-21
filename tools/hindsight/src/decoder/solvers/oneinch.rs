//! 1inch calldata extraction.
//!
//! The v6 Aggregation Router's `swap` entry carries a `SwapDescription` struct with everything a
//! declared read needs: both tokens, the input amount, the on-chain floor, and the recipient the
//! output is paid to. Verified against a live Ethereum trade (see the fixture test): the decoded
//! `srcToken`/`amount` matched the settled record exactly, `dstReceiver` was the trader, and the
//! settled output sat 101 bps above `minReturnAmount`.
//!
//! Two other entries appear in live traffic and are deliberately declined, because neither
//! carries the trade in a form this read can recover:
//!
//! - `unoswap` and its variants pack the pools into bitmasked descriptors, with no token pair in
//!   the calldata at all.
//! - `fillOrderArgs` is a limit-order fill, not a router swap. The order's `makingAmount` and
//!   `takingAmount` are a price the maker signed off-chain, and a partial fill settles only part of
//!   it, so reading it as a market swap would compare a signed limit price against a spot quote.
//!   Those trades stay on the netting fallback until we decide what a limit order should be
//!   compared against.

use alloy::{
    primitives::{address, Address},
    rpc::types::Log,
    sol,
    sol_types::SolCall,
};

use crate::decoder::{
    solvers::{DeclaredSwap, SolverDecoder},
    veto::Veto,
};

sol! {
    /// The v6 router's swap terms. `srcReceiver` is the executor the input is routed to and
    /// `flags` is a bitfield; neither is read here.
    struct SwapDescription {
        address srcToken;
        address dstToken;
        address srcReceiver;
        address dstReceiver;
        uint256 amount;
        uint256 minReturnAmount;
        uint256 flags;
    }

    /// The v6 Aggregation Router's market-swap entry (selector `0x07ed2379`).
    function swap(address executor, SwapDescription desc, bytes data)
        external
        payable
        returns (uint256 returnAmount, uint256 spentAmount);
}

/// 1inch's sentinel for native ETH, normalized to the zero address like every other flow.
const ONEINCH_NATIVE_ETH: Address = address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

fn normalize_native(token: Address) -> Address {
    if token == ONEINCH_NATIVE_ETH {
        Address::ZERO
    } else {
        token
    }
}

/// The 1inch solver.
pub(crate) struct OneInch;

impl SolverDecoder for OneInch {
    /// The trader's swap terms from a `swap` call's `SwapDescription`. `minReturnAmount` is passed
    /// through as declared, including a zero — the router's per-hop checks can leave the top-level
    /// floor at zero, and the terms are still worth recording. The hint is unused: every field is
    /// read by ABI position.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let Ok(call) = swapCall::abi_decode(input) else { return Ok(None) };
        if call.desc.amount.is_zero() {
            return Ok(None);
        }
        let intent = DeclaredSwap::from_calldata(
            normalize_native(call.desc.srcToken),
            normalize_native(call.desc.dstToken),
            call.desc.amount,
            call.desc.minReturnAmount,
        )
        .with_recipient(call.desc.dstReceiver);
        Ok(Some(intent))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Bytes, U256};

    use super::*;

    /// The `swap` calldata of a real settled trade (tx
    /// `0x8cbd0e1568faa5084dd02e83b4bc5e98d9b7b685de7f56f4fae0069698a8f1e0`): 51,014.9961 of
    /// `0x73d7c860…` in, native ETH out. The settled record netted 1,541,583,057,157,647,921 wei
    /// out, 101 bps above the floor below.
    fn real_input() -> Vec<u8> {
        let text = include_str!("fixtures/oneinch_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    const TOKEN_IN: Address = address!("0x73d7c860998ca3c01ce8c808f5577d94d545d1b4");
    const TRADER: Address = address!("0x59e4d2324bf6bfc8f568125b8a03266c7d4a4726");
    const AMOUNT_IN: u128 = 51_014_996_100_000_000_000_000;
    const MIN_AMOUNT_OUT: u128 = 1_526_167_226_586_071_441;

    fn terms(input: &[u8]) -> Option<DeclaredSwap> {
        OneInch
            .declared(input, &[])
            .ok()
            .flatten()
    }

    #[test]
    fn test_selector_against_the_deployed_router() {
        // The selector observed on live v6 swaps. A wrong `sol!` declaration would compile and
        // silently never match, so it is pinned here.
        assert_eq!(swapCall::SELECTOR, [0x07, 0xed, 0x23, 0x79]);
    }

    #[test]
    fn test_real_fixture_declared_swap() {
        let intent = terms(&real_input()).unwrap();
        assert_eq!(intent.token_in, TOKEN_IN);
        // The calldata names 1inch's native sentinel; the record's token_out is the zero address.
        assert_eq!(intent.token_out, Address::ZERO);
        assert_eq!(intent.amount_in, U256::from(AMOUNT_IN));
        assert_eq!(intent.min_amount_out, Some(U256::from(MIN_AMOUNT_OUT)));
        // `swap` carries no off-chain quote, so the floor is the best available promise.
        assert_eq!(intent.declared_quote, None);
    }

    #[test]
    fn test_real_fixture_output_recipient() {
        // The trader themselves, unlike Fly's calldata, which names the venue's router.
        let intent = terms(&real_input()).unwrap();
        assert_eq!(intent.output_recipient, Some(TRADER));
    }

    #[test]
    fn test_limit_order_and_compact_entries_declined() {
        // `fillOrderArgs` (0xf497df75) is a limit-order fill and `unoswap` (0x83800a8e) packs its
        // pools into bitmasks; neither is a `swap` call, so both decline rather than guess.
        for selector in [[0xf4, 0x97, 0xdf, 0x75], [0x83, 0x80, 0x0a, 0x8e]] {
            let mut input = selector.to_vec();
            input.extend_from_slice(&real_input()[4..]);
            assert!(terms(&input).is_none());
        }
    }

    #[test]
    fn test_garbage_and_truncated_input_declined() {
        assert!(terms(&[]).is_none());
        assert!(terms(&[0xde, 0xad, 0xbe, 0xef]).is_none());
        assert!(terms(&real_input()[..100]).is_none());
    }

    #[test]
    fn test_zero_amount_declined() {
        let call = swapCall {
            executor: Address::ZERO,
            desc: SwapDescription {
                srcToken: TOKEN_IN,
                dstToken: Address::ZERO,
                srcReceiver: Address::ZERO,
                dstReceiver: TRADER,
                amount: U256::ZERO,
                minReturnAmount: U256::from(1_000),
                flags: U256::ZERO,
            },
            data: Bytes::default(),
        };
        assert!(terms(&call.abi_encode()).is_none());
    }
}
