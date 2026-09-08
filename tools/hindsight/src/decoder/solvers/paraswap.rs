//! `ParaSwap` (Velora) Augustus v6 and v5 calldata extraction.
//!
//! Both router versions take live traffic — v5 at `0xdef171fe…`, v6 at `0x6a000f20…` — and both
//! state the same five terms in the one struct their entry takes: the two tokens, the input
//! amount, the floor the trade reverts below, `ParaSwap`'s own quote, and the address the output is
//! paid to. `terms` turns any of them into a `DeclaredSwap`, so each entry below is only the read
//! that finds those five fields.
//!
//! v6 has one generic entry, `swapExactAmountIn`, whose struct holds only fixed-size fields and is
//! ABI-encoded in place rather than behind an offset.
//!
//! v5 has one entry per route shape, and four of them carry the terms:
//!
//! | entry | shape | where the output token sits |
//! |---|---|---|
//! | `simpleSwap` | one pre-built call sequence | `toToken` |
//! | `directUniV3Swap` | a single Uniswap v3 route | `toToken` |
//! | `multiSwap` | a chain of hops | the last `Path.to` |
//! | `megaSwap` | a split, each part its own chain | the last `Path.to` of each part |
//!
//! `swapOnUniswapV2Fork` and `simpleBuy` are not read (4 of 303 sampled v5 transactions): the
//! former packs its route into a `uint256[]` that never names the output token, and the latter is
//! an exact-output entry. Both fall to netting.
//!
//! `quotedAmount` is worth more here than for most solvers: `ParaSwap` caps the user at its quote
//! and keeps the surplus, so a settled amount usually sits between the floor and the quote.
//!
//! Verified against two live v6 Ethereum trades (blocks 25741801 and 25741809): both decoded
//! `fromAmount` matched the settled record exactly, and both settled outputs cleared the decoded
//! floor and stayed under the decoded quote. One paid an ERC-20 input, the other native ETH. Each
//! v5 entry is verified the same way against one live trade, in the fixture-backed tests below. In
//! 303 sampled v5 transactions across Ethereum, Arbitrum and Polygon, no call declared a zero
//! floor, a zero quote, or a floor above its quote.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolCall,
};

use crate::decoder::{
    solvers::{normalize_native, DeclaredSwap, SolverDecoder},
    veto::Veto,
};

sol! {
    /// Augustus v6's generic swap terms. Every field is fixed-size, so the struct is encoded
    /// inline. `metadata` is `ParaSwap`'s own bookkeeping and is not read.
    struct SwapExactAmountInParams {
        address srcToken;
        address destToken;
        uint256 fromAmount;
        uint256 toAmount;
        uint256 quotedAmount;
        bytes32 metadata;
        address beneficiary;
    }

    /// Augustus v6's generic entry (selector `0xe3ead59e`).
    function swapExactAmountIn(
        address executor,
        SwapExactAmountInParams params,
        uint256 partnerAndFee,
        bytes permit,
        bytes executorData
    ) external payable returns (uint256 receivedAmount, uint256 paraswapShare, uint256 partnerShare);

    /// One call an Augustus v5 adapter makes on a liquidity venue. Only its position in the
    /// enclosing `Path` matters here; nothing inside it is read.
    struct Route {
        uint256 index;
        address targetExchange;
        uint256 percent;
        bytes payload;
        uint256 networkFee;
    }

    /// The adapter that executes one hop's routes. Augustus names the struct `Adapter`;
    /// the name here carries `Call` so the struct and its `adapter` field do not repeat
    /// each other. Struct names are not part of the ABI, so the encoding is unchanged.
    struct AdapterCall {
        address adapter;
        uint256 percent;
        uint256 networkFee;
        Route[] route;
    }

    /// One hop of an Augustus v5 route. `to` is the token the hop delivers, which is what makes
    /// the last hop's `to` the trade's output token — `multiSwap` and `megaSwap` state it nowhere
    /// else.
    struct Path {
        address to;
        uint256 totalNetworkFee;
        AdapterCall[] adapters;
    }

    /// Augustus v5's `simpleSwap` terms: a route `ParaSwap` pre-built as raw calls to `callees`.
    struct SimpleData {
        address fromToken;
        address toToken;
        uint256 fromAmount;
        uint256 toAmount;
        uint256 expectedAmount;
        address[] callees;
        bytes exchangeData;
        uint256[] startIndexes;
        uint256[] values;
        address beneficiary;
        address partner;
        uint256 feePercent;
        bytes permit;
        uint256 deadline;
        bytes16 uuid;
    }

    /// Augustus v5's `multiSwap` terms: one chain of hops, each hop's output the next hop's input.
    struct SellData {
        address fromToken;
        uint256 fromAmount;
        uint256 toAmount;
        uint256 expectedAmount;
        address beneficiary;
        Path[] path;
        address partner;
        uint256 feePercent;
        bytes permit;
        uint256 deadline;
        bytes16 uuid;
    }

    /// One part of a `megaSwap` split: `fromAmountPercent` of the input down its own chain of hops.
    struct MegaSwapPath {
        uint256 fromAmountPercent;
        Path[] path;
    }

    /// Augustus v5's `megaSwap` terms: a split whose parts all end in the same token.
    struct MegaSwapSellData {
        address fromToken;
        uint256 fromAmount;
        uint256 toAmount;
        uint256 expectedAmount;
        address beneficiary;
        MegaSwapPath[] path;
        address partner;
        uint256 feePercent;
        bytes permit;
        uint256 deadline;
        bytes16 uuid;
    }

    /// Augustus v5's `directUniV3Swap` terms: one Uniswap v3 route, its pools in the packed
    /// `path` bytes. Both tokens are stated outright, so the packed path is not read.
    struct DirectUniV3 {
        address fromToken;
        address toToken;
        address exchange;
        uint256 fromAmount;
        uint256 toAmount;
        uint256 expectedAmount;
        uint256 feePercent;
        uint256 deadline;
        address partner;
        bool isApproved;
        address beneficiary;
        bytes path;
        bytes permit;
        bytes16 uuid;
    }

    /// Augustus v5's entries, in the order `declared` tries them (selectors `0x54e3f31b`,
    /// `0xa6886da9`, `0xa94e78ef`, `0x46c67b6d`).
    function simpleSwap(SimpleData data) external payable returns (uint256 receivedAmount);
    function directUniV3Swap(DirectUniV3 data) external payable returns (uint256 receivedAmount);
    function multiSwap(SellData data) external payable returns (uint256 receivedAmount);
    function megaSwap(MegaSwapSellData data) external payable returns (uint256 receivedAmount);
}

/// The `ParaSwap` solver.
pub(crate) struct Paraswap;

impl SolverDecoder for Paraswap {
    /// The trader's swap terms from whichever Augustus entry the calldata holds.
    ///
    /// Each read validates its own selector, so the order is only which router version is tried
    /// first; a call that is none of them is left to netting.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        Ok(v6_swap_exact_amount_in(input)
            .or_else(|| v5_simple_swap(input))
            .or_else(|| v5_direct_uni_v3_swap(input))
            .or_else(|| v5_multi_swap(input))
            .or_else(|| v5_mega_swap(input)))
    }
}

/// The five stated terms as a `DeclaredSwap`, or `None` for calldata that cannot describe a trade.
///
/// `beneficiary` is the declared output recipient, except when it is the zero address — Augustus
/// reads that as "pay the caller", so it is left unset and the caller anchors on the transaction
/// sender. A floor above the quote is inconsistent (the quote is what `ParaSwap` promised, the
/// floor what it would accept) and is declined rather than recorded, as is a zero on either
/// amount.
fn terms(
    token_in: Address,
    token_out: Address,
    from_amount: U256,
    to_amount: U256,
    quoted_amount: U256,
    beneficiary: Address,
) -> Option<DeclaredSwap> {
    if from_amount.is_zero() || to_amount.is_zero() || to_amount > quoted_amount {
        return None;
    }
    let declared = DeclaredSwap::from_calldata(
        normalize_native(token_in),
        normalize_native(token_out),
        from_amount,
        to_amount,
    )
    .with_quote(quoted_amount, None);
    Some(if beneficiary.is_zero() { declared } else { declared.with_recipient(beneficiary) })
}

/// The token a chain of hops ends in: the last hop's `to`. `None` for an empty chain, which
/// describes no trade.
fn chain_output(path: &[Path]) -> Option<Address> {
    path.last().map(|hop| hop.to)
}

/// The token every part of a split ends in, or `None` when the parts disagree — parts that end in
/// different tokens are not one trade, and one record holds one swap.
fn split_output(paths: &[MegaSwapPath]) -> Option<Address> {
    let mut output = None;
    for part in paths {
        let end = chain_output(&part.path)?;
        match output {
            None => output = Some(end),
            Some(token) if token == end => {}
            Some(_) => return None,
        }
    }
    output
}

/// Augustus v6's `swapExactAmountIn`.
fn v6_swap_exact_amount_in(input: &[u8]) -> Option<DeclaredSwap> {
    let params = swapExactAmountInCall::abi_decode(input)
        .ok()?
        .params;
    terms(
        params.srcToken,
        params.destToken,
        params.fromAmount,
        params.toAmount,
        params.quotedAmount,
        params.beneficiary,
    )
}

/// Augustus v5's `simpleSwap`.
fn v5_simple_swap(input: &[u8]) -> Option<DeclaredSwap> {
    let data = simpleSwapCall::abi_decode(input)
        .ok()?
        .data;
    terms(
        data.fromToken,
        data.toToken,
        data.fromAmount,
        data.toAmount,
        data.expectedAmount,
        data.beneficiary,
    )
}

/// Augustus v5's `directUniV3Swap`.
fn v5_direct_uni_v3_swap(input: &[u8]) -> Option<DeclaredSwap> {
    let data = directUniV3SwapCall::abi_decode(input)
        .ok()?
        .data;
    terms(
        data.fromToken,
        data.toToken,
        data.fromAmount,
        data.toAmount,
        data.expectedAmount,
        data.beneficiary,
    )
}

/// Augustus v5's `multiSwap`, whose output token is the last hop's `to`.
fn v5_multi_swap(input: &[u8]) -> Option<DeclaredSwap> {
    let data = multiSwapCall::abi_decode(input)
        .ok()?
        .data;
    terms(
        data.fromToken,
        chain_output(&data.path)?,
        data.fromAmount,
        data.toAmount,
        data.expectedAmount,
        data.beneficiary,
    )
}

/// Augustus v5's `megaSwap`, whose output token is the token every part of the split ends in.
fn v5_mega_swap(input: &[u8]) -> Option<DeclaredSwap> {
    let data = megaSwapCall::abi_decode(input)
        .ok()?
        .data;
    terms(
        data.fromToken,
        split_output(&data.path)?,
        data.fromAmount,
        data.toAmount,
        data.expectedAmount,
        data.beneficiary,
    )
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Address, U256};

    use super::*;
    use crate::decoder::solvers::NATIVE_TOKEN_SENTINEL;

    /// The `swapExactAmountIn` calldata of a real settled trade (tx `0x6bb77fe6…`, block
    /// 25741801): 5,599.115792 of `0xce6170ea…` in, WETH out. The settled record netted
    /// 2,997,199,455,534,478,910 wei out — just above the floor below, under the quote.
    fn real_input() -> Vec<u8> {
        let text = include_str!("fixtures/paraswap_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    const TOKEN_IN: Address = address!("0xce6170ea245dc8d1f275a710a062b70f125f0110");
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    const BENEFICIARY: Address = address!("0xfdff0b569f14af593d446e51b3e42f502124ac85");
    const AMOUNT_IN: u64 = 5_599_115_792;
    const FLOOR: u128 = 2_997_199_455_316_012_230;
    const QUOTE: u128 = 2_997_499_205_236_535_884;

    fn terms(input: &[u8]) -> Option<DeclaredSwap> {
        Paraswap
            .declared(input, &[])
            .ok()
            .flatten()
    }

    /// A `swapExactAmountIn` call encoded through the `sol!` types, mirroring a real trade.
    fn call_with(
        src: Address,
        dst: Address,
        from_amount: u64,
        to_amount: u64,
        quoted: u64,
        beneficiary: Address,
    ) -> Vec<u8> {
        swapExactAmountInCall {
            executor: Address::ZERO,
            params: SwapExactAmountInParams {
                srcToken: src,
                destToken: dst,
                fromAmount: U256::from(from_amount),
                toAmount: U256::from(to_amount),
                quotedAmount: U256::from(quoted),
                metadata: alloy::primitives::B256::ZERO,
                beneficiary,
            },
            partnerAndFee: U256::ZERO,
            permit: alloy::primitives::Bytes::default(),
            executorData: alloy::primitives::Bytes::default(),
        }
        .abi_encode()
    }

    #[test]
    fn test_selector_against_the_deployed_router() {
        // The selector observed on both sampled trades. A wrong `sol!` declaration would compile
        // and silently never match, so it is pinned here.
        assert_eq!(swapExactAmountInCall::SELECTOR, [0xe3, 0xea, 0xd5, 0x9e]);
    }

    #[test]
    fn test_real_fixture_declared_swap() {
        let declared = terms(&real_input()).unwrap();
        assert_eq!(declared.token_in, TOKEN_IN);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_in, Some(U256::from(AMOUNT_IN)));
        assert_eq!(declared.min_amount_out, Some(U256::from(FLOOR)));
        assert_eq!(declared.declared_quote, Some(U256::from(QUOTE)));
        // Calldata states no settled output, so the caller recovers it.
        assert_eq!(declared.amount_out, None);
        assert_eq!(declared.tracked, None);
    }

    #[test]
    fn test_real_fixture_beneficiary_is_the_output_recipient() {
        let declared = terms(&real_input()).unwrap();
        assert_eq!(declared.output_recipient, Some(BENEFICIARY));
    }

    #[test]
    fn test_native_sentinel_normalized() {
        // Live tx 0xd427bdec… paid native ETH in, which Augustus writes as 0xeeee…ee.
        let call = call_with(NATIVE_TOKEN_SENTINEL, WETH, 1_000, 900, 950, Address::ZERO);
        assert_eq!(terms(&call).unwrap().token_in, Address::ZERO);
    }

    #[test]
    fn test_zero_beneficiary_leaves_the_recipient_unset() {
        // Augustus reads a zero beneficiary as "pay the caller", so there is no declared
        // recipient and the caller anchors on the transaction sender instead.
        let call = call_with(TOKEN_IN, WETH, 1_000, 900, 950, Address::ZERO);
        assert_eq!(terms(&call).unwrap().output_recipient, None);
    }

    #[test]
    fn test_zero_amounts_declined() {
        assert!(terms(&call_with(TOKEN_IN, WETH, 0, 900, 950, Address::ZERO)).is_none());
        assert!(terms(&call_with(TOKEN_IN, WETH, 1_000, 0, 950, Address::ZERO)).is_none());
    }

    #[test]
    fn test_floor_above_quote_declined() {
        // The floor cannot exceed the amount ParaSwap quoted; such calldata is inconsistent.
        assert!(terms(&call_with(TOKEN_IN, WETH, 1_000, 960, 950, Address::ZERO)).is_none());
    }

    #[test]
    fn test_garbage_and_truncated_input_declined() {
        assert!(terms(&[]).is_none());
        assert!(terms(&[0xde, 0xad, 0xbe, 0xef]).is_none());
        assert!(terms(&real_input()[..100]).is_none());
        // Another Augustus entry never decodes as this one.
        let mut wrong = real_input();
        wrong[0] = 0xff;
        assert!(terms(&wrong).is_none());
    }

    /// The calldata a fixture file holds.
    fn fixture(text: &str) -> Vec<u8> {
        let text = text.trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    /// One real settled Ethereum trade per v5 entry `declared` reads.
    fn v5_simple_input() -> Vec<u8> {
        fixture(include_str!("fixtures/paraswap_v5_simple_input.txt"))
    }

    fn v5_direct_univ3_input() -> Vec<u8> {
        fixture(include_str!("fixtures/paraswap_v5_direct_univ3_input.txt"))
    }

    fn v5_multi_input() -> Vec<u8> {
        fixture(include_str!("fixtures/paraswap_v5_multi_input.txt"))
    }

    fn v5_mega_input() -> Vec<u8> {
        fixture(include_str!("fixtures/paraswap_v5_mega_input.txt"))
    }

    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const CRV: Address = address!("0xd533a949740bb3306d119cc777fa900ba034cd52");

    /// One entry's read, as `declared` holds them.
    type Read = fn(&[u8]) -> Option<DeclaredSwap>;

    #[test]
    fn test_v5_selectors_against_the_deployed_router() {
        // The selectors observed on live Augustus v5 traffic. A wrong `sol!` declaration would
        // compile and silently never match, so all four are pinned.
        assert_eq!(simpleSwapCall::SELECTOR, [0x54, 0xe3, 0xf3, 0x1b]);
        assert_eq!(directUniV3SwapCall::SELECTOR, [0xa6, 0x88, 0x6d, 0xa9]);
        assert_eq!(multiSwapCall::SELECTOR, [0xa9, 0x4e, 0x78, 0xef]);
        assert_eq!(megaSwapCall::SELECTOR, [0x46, 0xc6, 0x7b, 0x6d]);
    }

    #[test]
    fn test_v5_simple_swap_fixture() {
        // Tx 0x98f3e72a…, block 25915307: 5e12 wei of native ETH in, USDC out. The trade settled
        // 12,491 USDC to the sender — the quote exactly, above the floor.
        let declared = terms(&v5_simple_input()).unwrap();
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.token_out, USDC);
        assert_eq!(declared.amount_in, Some(U256::from(5_000_000_000_000u64)));
        assert_eq!(declared.min_amount_out, Some(U256::from(12_366)));
        assert_eq!(declared.declared_quote, Some(U256::from(12_491)));
        assert_eq!(declared.amount_out, None);
        // A zero beneficiary pays the caller, so the caller anchors on the transaction sender.
        assert_eq!(declared.output_recipient, None);
    }

    #[test]
    fn test_v5_direct_uni_v3_fixture() {
        // Tx 0x3a9a0a35…, block 25926601: 2e16 wei of native ETH in, 0x57e114b6… out. The trade
        // settled 298,923,016,963,469,027,388 — above the floor, under the quote.
        let declared = terms(&v5_direct_univ3_input()).unwrap();
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.token_out, address!("0x57e114b691db790c35207b2e685d4a43181e6061"));
        assert_eq!(declared.amount_in, Some(U256::from(20_000_000_000_000_000u64)));
        assert_eq!(declared.min_amount_out, Some(U256::from(295_963_383_132_147_551_843u128)));
        assert_eq!(declared.declared_quote, Some(U256::from(298_952_912_254_694_496_812u128)));
    }

    #[test]
    fn test_v5_multi_swap_output_token_from_the_last_hop() {
        // Tx 0x3e4e96c3…, block 25919717: 149.614 CRV in through WETH to USDC. `multiSwap` names
        // no output token, so USDC comes from the last hop's `to`. Settled 55,230,106 USDC.
        let declared = terms(&v5_multi_input()).unwrap();
        assert_eq!(declared.token_in, CRV);
        assert_eq!(declared.token_out, USDC);
        assert_eq!(declared.amount_in, Some(U256::from(149_614_066_327_394_216_365u128)));
        assert_eq!(declared.min_amount_out, Some(U256::from(54_703_539)));
        assert_eq!(declared.declared_quote, Some(U256::from(55_256_101)));
    }

    #[test]
    fn test_v5_mega_swap_output_token_from_the_split_parts() {
        // Tx 0x961fcd54…, block 25916254: 125 USDC in, split 34/66, both parts ending in the
        // native sentinel. The trade delivered 49,561,764,722,217,372 wei — above the floor,
        // under the quote — as native ETH, which is why the sentinel normalizes to the zero
        // address rather than reading as WETH.
        let declared = terms(&v5_mega_input()).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, Address::ZERO);
        assert_eq!(declared.amount_in, Some(U256::from(125_000_000)));
        assert_eq!(declared.min_amount_out, Some(U256::from(49_250_471_113_512_521u128)));
        assert_eq!(declared.declared_quote, Some(U256::from(49_747_950_619_709_618u128)));
    }

    /// A `megaSwap` call whose split parts end in `ends`, one part per entry.
    fn mega_swap_with(ends: &[Address]) -> Vec<u8> {
        let hop = |to: Address| Path { to, totalNetworkFee: U256::ZERO, adapters: Vec::new() };
        megaSwapCall {
            data: MegaSwapSellData {
                fromToken: USDC,
                fromAmount: U256::from(1_000),
                toAmount: U256::from(900),
                expectedAmount: U256::from(950),
                beneficiary: Address::ZERO,
                path: ends
                    .iter()
                    .map(|end| MegaSwapPath {
                        fromAmountPercent: U256::from(10_000 / ends.len().max(1)),
                        path: vec![hop(*end)],
                    })
                    .collect(),
                partner: Address::ZERO,
                feePercent: U256::ZERO,
                permit: alloy::primitives::Bytes::default(),
                deadline: U256::ZERO,
                uuid: alloy::primitives::FixedBytes::<16>::ZERO,
            },
        }
        .abi_encode()
    }

    #[test]
    fn test_v5_mega_swap_parts_ending_in_different_tokens() {
        // Parts that end in different tokens are two trades, and one record holds one swap.
        assert!(terms(&mega_swap_with(&[USDC, CRV])).is_none());
        // Parts that agree are the split the entry is for.
        assert_eq!(
            terms(&mega_swap_with(&[CRV, CRV]))
                .unwrap()
                .token_out,
            CRV
        );
    }

    #[test]
    fn test_v5_mega_swap_without_a_path() {
        // No hop states no output token, so there is nothing to compare against.
        assert!(terms(&mega_swap_with(&[])).is_none());
    }

    #[test]
    fn test_v5_entries_do_not_decode_as_each_other() {
        // Every read validates its own selector, so one entry's calldata reaches exactly one of
        // them. Without that, a v5 struct could be read through the wrong layout.
        let inputs =
            [v5_simple_input(), v5_direct_univ3_input(), v5_multi_input(), v5_mega_input()];
        let reads: [Read; 5] = [
            v6_swap_exact_amount_in,
            v5_simple_swap,
            v5_direct_uni_v3_swap,
            v5_multi_swap,
            v5_mega_swap,
        ];
        for input in &inputs {
            let matched = reads
                .iter()
                .filter(|read| read(input).is_some())
                .count();
            assert_eq!(matched, 1, "{matched} reads matched one fixture");
        }
        // The v6 fixture still reaches only the v6 read.
        let v6 = real_input();
        assert_eq!(
            reads
                .iter()
                .filter(|read| read(&v6).is_some())
                .count(),
            1
        );
    }
}
