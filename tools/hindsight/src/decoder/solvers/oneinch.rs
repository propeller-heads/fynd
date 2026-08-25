//! 1inch calldata extraction.
//!
//! The `swap` entry of both deployed Aggregation Routers carries a `SwapDescription` struct with
//! everything a declared read needs: both tokens, the input amount, the on-chain floor, and the
//! recipient the output is paid to. v5 and v6 declare the same struct and differ only in the
//! arguments around it — v5 takes a `permit` blob it forwards to the input token — so one read
//! serves both. Both routers are registered under the name `1inch`, so which one settled a trade
//! changes nothing here.
//!
//! Verified against a live Ethereum trade per version (see the fixture tests): the decoded
//! `srcToken`/`amount` matched the settled record exactly, `dstReceiver` was the address the
//! output was paid to, and the settled output sat above `minReturnAmount` — 101 bps on the v6
//! trade, 110 bps on the v5 one.
//!
//! Two other entries appear in live traffic and are deliberately declined, because neither
//! carries the trade in a form this read can recover. Both versions have them, and the selector
//! check declines both versions the same way:
//!
//! - `unoswap` and its variants pack the pools into bitmasked descriptors, with no token pair in
//!   the calldata at all.
//! - `fillOrderArgs` is a limit-order fill, not a router swap. The order's `makingAmount` and
//!   `takingAmount` are a price the maker signed off-chain, and a partial fill settles only part of
//!   it, so reading it as a market swap would compare a signed limit price against a spot quote.
//!   Those trades stay on the netting fallback until we decide what a limit order should be
//!   compared against.

use alloy::{rpc::types::Log, sol, sol_types::SolCall};

use crate::decoder::{
    solvers::{normalize_native, DeclaredSwap, SolverDecoder},
    veto::Veto,
};

sol! {
    /// The router's swap terms, declared identically by v5 and v6. `srcReceiver` is the executor
    /// the input is routed to and `flags` is a bitfield; neither is read here.
    struct SwapDescription {
        address srcToken;
        address dstToken;
        address srcReceiver;
        address dstReceiver;
        uint256 amount;
        uint256 minReturnAmount;
        uint256 flags;
    }

    /// The v6 Aggregation Router's market-swap entry (selector `0x07ed2379`), generated as
    /// `swap_0Call`.
    function swap(address executor, SwapDescription desc, bytes data)
        external
        payable
        returns (uint256 returnAmount, uint256 spentAmount);

    /// The v5 Aggregation Router's market-swap entry (selector `0x12aa3caf`), generated as
    /// `swap_1Call`. `permit` is forwarded to the input token and carries no swap terms.
    function swap(address executor, SwapDescription desc, bytes permit, bytes data)
        external
        payable
        returns (uint256 returnAmount, uint256 spentAmount);
}

/// The 1inch solver.
pub(crate) struct OneInch;

impl SolverDecoder for OneInch {
    /// The trader's swap terms from a v6 or v5 `swap` call's `SwapDescription`. `minReturnAmount`
    /// is passed through as declared, including a zero — the router's per-hop checks can leave the
    /// top-level floor at zero, and the terms are still worth recording. The logs are unused:
    /// every field is read by ABI position.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let desc = if let Ok(call) = swap_0Call::abi_decode(input) {
            call.desc
        } else if let Ok(call) = swap_1Call::abi_decode(input) {
            call.desc
        } else {
            return Ok(None);
        };
        if desc.amount.is_zero() {
            return Ok(None);
        }
        let intent = DeclaredSwap::from_calldata(
            normalize_native(desc.srcToken),
            normalize_native(desc.dstToken),
            desc.amount,
            desc.minReturnAmount,
        )
        .with_recipient(desc.dstReceiver);
        Ok(Some(intent))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Address, Bytes, U256};

    use super::*;

    /// The v6 `swap` calldata of a real settled trade (tx
    /// `0x8cbd0e1568faa5084dd02e83b4bc5e98d9b7b685de7f56f4fae0069698a8f1e0`): 51,014.9961 of
    /// `0x73d7c860…` in, native ETH out. The settled record netted 1,541,583,057,157,647,921 wei
    /// out, 101 bps above the floor below.
    fn real_input() -> Vec<u8> {
        let text = include_str!("fixtures/oneinch_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    /// The v5 `swap` calldata of a real settled trade (tx
    /// `0x028f7252f0e4c1f182f21a81112a3113ce07d0031891eb8582d2d7b6184c16aa`, entered through an
    /// unregistered gasless-swap venue): 0.201213365801107114 WETH in, USDC out. The router
    /// returned 493.846340 USDC, 110 bps above the floor below, to a `dstReceiver` that is not
    /// the transaction sender.
    fn real_v5_input() -> Vec<u8> {
        let text = include_str!("fixtures/oneinch_v5_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    const TOKEN_IN: Address = address!("0x73d7c860998ca3c01ce8c808f5577d94d545d1b4");
    const TRADER: Address = address!("0x59e4d2324bf6bfc8f568125b8a03266c7d4a4726");
    const AMOUNT_IN: u128 = 51_014_996_100_000_000_000_000;
    const MIN_AMOUNT_OUT: u128 = 1_526_167_226_586_071_441;

    const V5_WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    const V5_USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const V5_RECIPIENT: Address = address!("0x2b3547a21e84e55a9f029d45383c64e1c5ff0d8c");
    const V5_AMOUNT_IN: u128 = 201_213_365_801_107_114;
    const V5_MIN_AMOUNT_OUT: u128 = 488_461_169;

    fn terms(input: &[u8]) -> Option<DeclaredSwap> {
        OneInch
            .declared(input, &[])
            .ok()
            .flatten()
    }

    #[test]
    fn test_selectors_against_the_deployed_routers() {
        // The selectors observed on live swaps, v6 then v5. A wrong `sol!` declaration would
        // compile and silently never match, so both are pinned here. The overload order also
        // decides which generated name is which, so a reordered `sol!` block fails here too.
        assert_eq!(swap_0Call::SELECTOR, [0x07, 0xed, 0x23, 0x79]);
        assert_eq!(swap_1Call::SELECTOR, [0x12, 0xaa, 0x3c, 0xaf]);
    }

    #[test]
    fn test_real_fixture_declared_swap() {
        let intent = terms(&real_input()).unwrap();
        assert_eq!(intent.token_in, TOKEN_IN);
        // The calldata names 1inch's native sentinel; the record's token_out is the zero address.
        assert_eq!(intent.token_out, Address::ZERO);
        assert_eq!(intent.amount_in, Some(U256::from(AMOUNT_IN)));
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
    fn test_real_v5_fixture_declared_swap() {
        let intent = terms(&real_v5_input()).unwrap();
        assert_eq!(intent.token_in, V5_WETH);
        assert_eq!(intent.token_out, V5_USDC);
        assert_eq!(intent.amount_in, Some(U256::from(V5_AMOUNT_IN)));
        assert_eq!(intent.min_amount_out, Some(U256::from(V5_MIN_AMOUNT_OUT)));
        assert_eq!(intent.declared_quote, None);
    }

    #[test]
    fn test_real_v5_fixture_output_recipient() {
        // v5 pays a `dstReceiver` that is neither the trader's own address on the transaction nor
        // the venue's router, so the recovered output must come from this address's receipt.
        let intent = terms(&real_v5_input()).unwrap();
        assert_eq!(intent.output_recipient, Some(V5_RECIPIENT));
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
        let desc = SwapDescription {
            srcToken: TOKEN_IN,
            dstToken: Address::ZERO,
            srcReceiver: Address::ZERO,
            dstReceiver: TRADER,
            amount: U256::ZERO,
            minReturnAmount: U256::from(1_000),
            flags: U256::ZERO,
        };
        let v6 = swap_0Call { executor: Address::ZERO, desc: desc.clone(), data: Bytes::default() };
        let v5 = swap_1Call {
            executor: Address::ZERO,
            desc,
            permit: Bytes::default(),
            data: Bytes::default(),
        };
        assert!(terms(&v6.abi_encode()).is_none());
        assert!(terms(&v5.abi_encode()).is_none());
    }
}
