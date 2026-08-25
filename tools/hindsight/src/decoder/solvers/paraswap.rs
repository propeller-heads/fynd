//! `ParaSwap` (Velora) Augustus v6 calldata extraction.
//!
//! `swapExactAmountIn` carries every term in one static struct: both tokens, the input amount, the
//! floor the trade reverts below, `ParaSwap`'s own quote, and the address the output is paid to.
//! The struct holds only fixed-size fields, so it is ABI-encoded in place rather than behind an
//! offset — a plain decode reaches all of it.
//!
//! `quotedAmount` is worth more here than for most solvers: `ParaSwap` caps the user at its quote
//! and keeps the surplus, so a settled amount usually sits between the floor and the quote.
//!
//! Verified against two live Ethereum trades (blocks 25741801 and 25741809): both decoded
//! `fromAmount` matched the settled record exactly, and both settled outputs cleared the decoded
//! floor and stayed under the decoded quote. One paid an ERC-20 input, the other native ETH.

use alloy::{rpc::types::Log, sol, sol_types::SolCall};

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
}

/// The `ParaSwap` solver.
pub(crate) struct Paraswap;

impl SolverDecoder for Paraswap {
    /// The trader's swap terms from a `swapExactAmountIn` call's params.
    ///
    /// `beneficiary` is the declared output recipient, except when it is the zero address —
    /// Augustus reads that as "pay the caller", so it is left unset and the caller anchors on the
    /// transaction sender. A floor above the quote is inconsistent (the quote is what `ParaSwap`
    /// promised, the floor what it would accept) and is declined rather than recorded.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let Ok(call) = swapExactAmountInCall::abi_decode(input) else { return Ok(None) };
        let params = call.params;
        if params.fromAmount.is_zero() || params.toAmount.is_zero() {
            return Ok(None);
        }
        if params.toAmount > params.quotedAmount {
            return Ok(None);
        }
        let declared = DeclaredSwap::from_calldata(
            normalize_native(params.srcToken),
            normalize_native(params.destToken),
            params.fromAmount,
            params.toAmount,
        )
        .with_quote(params.quotedAmount, None);
        Ok(Some(if params.beneficiary.is_zero() {
            declared
        } else {
            declared.with_recipient(params.beneficiary)
        }))
    }
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
}
