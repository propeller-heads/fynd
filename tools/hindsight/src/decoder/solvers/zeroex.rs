//! 0x calldata extraction.
//!
//! Relay's 0x flow enters through `AllowanceHolder.exec(operator, token, amount, target, data)`
//! before reaching Settler's own `execute(AllowedSlippage, actions, zid)` — confirmed in a live
//! 21-transaction Base sample (settled and reverted): every one is `AllowanceHolder`-wrapped, and
//! for two settled trades the decoded `token`/`amount` matched the netted `token_in`/`amount_in`
//! hindsight already recorded exactly, and the decoded `buyToken` matched `token_out` exactly.
//! Both the wrapper (`exec`) and Settler's own entry (`execute`) are verified against 0x-settler's
//! published source (github.com/0xProject/0x-settler): `IAllowanceHolder.sol`,
//! `ISettlerTakerSubmitted.sol`, `ISettlerBase.sol`'s `AllowedSlippage` struct, and
//! `ISettlerActions.sol`'s `POSITIVE_SLIPPAGE` action.
//!
//! A bare Settler entry (no `AllowanceHolder` wrapper) never occurred in the sample, and — unlike
//! `AllowedSlippage`, which is read the same way either way — has no calldata field that reliably
//! carries `token_in`/`amount_in`: Settler's `actions` array is heterogeneous per liquidity source,
//! so scanning it for an input-token address would be a guess, not a decode. `declared_swap` for a
//! bare entry is declined rather than guessed, per the "no dead code, no guessing" rule; if bare
//! entries turn out to matter, the `token_in` question needs its own investigation, not a shortcut
//! here.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolCall,
};

use crate::decoder::solvers::{Declaration, SolverDecoder, SwapIntent};

sol! {
    /// `IAllowanceHolder.exec` — Relay's 0x flow always enters through this wrapper before
    /// reaching Settler; `token`/`amount` are the taker's declared input, temporarily authorised
    /// for `operator` (Settler) to consume.
    function exec(address operator, address token, uint256 amount, address target, bytes data)
        external
        payable
        returns (bytes memory result);

    /// `ISettlerBase.AllowedSlippage` — the taker's floor and payout address, read by
    /// `Settler.execute` regardless of whether it was reached via `AllowanceHolder` or directly.
    struct AllowedSlippage {
        address recipient;
        address buyToken;
        uint256 minAmountOut;
    }

    /// `ISettlerTakerSubmitted.execute` — Settler's main taker-submitted entry.
    function execute(AllowedSlippage slippage, bytes[] actions, bytes32 zidAndAffiliate)
        external
        payable
        returns (bool);

    /// `ISettlerActions.POSITIVE_SLIPPAGE` — one element of `execute`'s `actions` array, present
    /// only when the route has slippage headroom to declare; `expectedAmount` is 0x's own
    /// off-chain quote for the trade, usable as this solver's declared quote.
    function POSITIVE_SLIPPAGE(address recipient, address token, uint256 expectedAmount, uint256 maxBps)
        external;
}

/// 0x's own native-ETH sentinel — distinct from hindsight's `Address::ZERO` convention, so it is
/// normalized on the way out (matching `KyberSwap`'s equivalent).
const ZEROEX_NATIVE: Address =
    alloy::primitives::address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

fn normalize_native(token: Address) -> Address {
    if token == ZEROEX_NATIVE {
        Address::ZERO
    } else {
        token
    }
}

/// Settler's own terms, decoded from an `execute` call regardless of how it was reached (wrapped
/// in `AllowanceHolder.exec` or, if it ever occurs, called directly).
struct SettlerTerms {
    recipient: Address,
    buy_token: Address,
    min_amount_out: U256,
    declared_quote: Option<U256>,
}

/// Decode `execute`'s `AllowedSlippage` plus, when present, a `POSITIVE_SLIPPAGE` action's
/// declared quote. Each element of `actions` is itself a standalone ABI-encoded call (its own
/// 4-byte selector and args, exactly like top-level calldata) — verified against a real decode —
/// so trying `POSITIVE_SLIPPAGE`'s decode against every action and keeping the one that succeeds
/// is a correct way to find it, not a heuristic.
fn decode_execute(input: &[u8]) -> Option<SettlerTerms> {
    let call = executeCall::abi_decode(input).ok()?;
    let declared_quote = call
        .actions
        .iter()
        .find_map(|action| POSITIVE_SLIPPAGECall::abi_decode(action).ok())
        .map(|action| action.expectedAmount);
    Some(SettlerTerms {
        recipient: call.slippage.recipient,
        buy_token: normalize_native(call.slippage.buyToken),
        min_amount_out: call.slippage.minAmountOut,
        declared_quote,
    })
}

pub(crate) struct ZeroEx;

impl SolverDecoder for ZeroEx {
    /// The trader's swap terms from `AllowanceHolder.exec`'s own parameters (`token`/`amount`,
    /// the input side) and the wrapped `execute` call's `AllowedSlippage` (`buyToken`/
    /// `minAmountOut`, the output side). `minAmountOut` is passed through as-is, including a
    /// legitimate zero (Settler's per-action slippage checks can leave the top-level floor at
    /// zero) — the intent is still worth recording, and the fillable/margin judgment already
    /// treats a zero floor sanely (trivially fillable, no margin to compute). `amount_in_hint` is
    /// unused: `AllowanceHolder`'s own parameter is the real amount, not a value to locate a field
    /// by.
    fn declared(
        &self,
        input: &[u8],
        _logs: &[Log],
        _amount_in_hint: Option<U256>,
    ) -> Option<Declaration> {
        let call = execCall::abi_decode(input).ok()?;
        if call.amount.is_zero() {
            return None;
        }
        let terms = decode_execute(&call.data)?;
        // Settler's `AllowedSlippage.recipient` — the address whose receipt anchors the settled
        // amount, same as Fly/KyberSwap.
        let intent = SwapIntent::new(
            normalize_native(call.token),
            terms.buy_token,
            call.amount,
            terms.min_amount_out,
        )
        .with_recipient(terms.recipient);
        Some(Declaration::Terms(match terms.declared_quote {
            Some(quote) => intent.with_quote(quote, None),
            None => intent,
        }))
    }
}

#[cfg(test)]
mod tests {
    /// The terms this solver reads from `input`, for tests that only care about the calldata path.
    fn terms(input: &[u8], hint: Option<U256>) -> Option<SwapIntent> {
        match ZeroEx.declared(input, &[], hint)? {
            Declaration::Terms(intent) => Some(intent),
            Declaration::Settled(_) => None,
        }
    }

    use alloy::primitives::address;

    use super::*;

    /// The `AllowanceHolder.exec` calldata of a real settled Base trade (tx
    /// `0x229f8cd137f7c9de635021de197d5472759d03993564853ff4690c327707ed79`): decoding recovers
    /// `token_in`/`amount_in` matching the netted settled record exactly (native ETH,
    /// 214715436309542453 wei) and `token_out` matching too (Base USDC).
    fn settled_input() -> Vec<u8> {
        let text = include_str!("fixtures/zeroex_settled_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    /// The `AllowanceHolder.exec` calldata of a real reverted Base trade (tx
    /// `0x157e025bd22ff0b222e4d2a04bb27caaef241f53f2bb7ecd22b7ab438c6b713f`) whose deepest frame
    /// reverted with `TooMuchSlippage` (see `trace.rs`'s
    /// `test_classify_revert_cause_real_zeroex_slippage`, which shares this transaction).
    fn reverted_input() -> Vec<u8> {
        let text = include_str!("fixtures/zeroex_reverted_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    const USDC: Address = address!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const RELAY_ROUTER: Address = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");

    #[test]
    fn test_real_settled_declared_swap() {
        let intent = terms(&settled_input(), None).unwrap();
        assert_eq!(intent.token_in, Address::ZERO); // 0x's native-ETH sentinel, normalized
        assert_eq!(intent.token_out, USDC);
        assert_eq!(intent.amount_in, U256::from(214_715_436_309_542_453u64));
        assert_eq!(intent.min_amount_out, U256::from(388_129_000u64));
        assert_eq!(intent.declared_quote(), Some(U256::from(396_058_371u64)));
    }

    #[test]
    fn test_real_settled_output_recipient() {
        let intent = terms(&settled_input(), None).unwrap();
        assert_eq!(intent.output_recipient, Some(RELAY_ROUTER));
    }

    #[test]
    fn test_real_reverted_declared_swap() {
        // The reverted trade's terms decode the same way a settled one's do — a revert emits no
        // logs, so calldata is the only source, and it is read no differently here.
        let intent = terms(&reverted_input(), None).unwrap();
        assert_eq!(intent.token_in, Address::ZERO);
        assert_eq!(intent.token_out, USDC);
        assert_eq!(intent.amount_in, U256::from(2_018_128_791_326_365_345u64));
        assert_eq!(intent.min_amount_out, U256::from(3_643_640_000u64));
        assert_eq!(intent.declared_quote(), Some(U256::from(3_718_000_789u64)));
    }

    #[test]
    fn test_real_reverted_output_recipient() {
        let intent = terms(&reverted_input(), None).unwrap();
        assert_eq!(intent.output_recipient, Some(RELAY_ROUTER));
    }

    #[test]
    fn test_selectors_match_computed_signatures() {
        assert_eq!(execCall::SELECTOR, [0x22, 0x13, 0xbc, 0x0b]);
        assert_eq!(executeCall::SELECTOR, [0x1f, 0xff, 0x99, 0x1f]);
        assert_eq!(POSITIVE_SLIPPAGECall::SELECTOR, [0x34, 0xee, 0x90, 0xca]);
    }

    #[test]
    fn test_bare_settler_entry_declines() {
        // A direct `execute` call (no `AllowanceHolder` wrapper): there is nowhere to read
        // token_in/amount_in from, so the whole decode declines and netting carries the trade.
        let call = executeCall {
            slippage: AllowedSlippage {
                recipient: RELAY_ROUTER,
                buyToken: USDC,
                minAmountOut: U256::from(1_000u64),
            },
            actions: vec![],
            zidAndAffiliate: alloy::primitives::FixedBytes::default(),
        };
        let input = executeCall::abi_encode(&call);
        assert!(terms(&input, None).is_none());
    }

    #[test]
    fn test_garbage_input_declines() {
        assert!(terms(&[0xde, 0xad, 0xbe, 0xef], None).is_none());
    }

    #[test]
    fn test_zero_amount_in_rejected() {
        // A garbage/zero AllowanceHolder amount is not a real trade.
        let call = execCall {
            operator: RELAY_ROUTER,
            token: Address::ZERO,
            amount: U256::ZERO,
            target: RELAY_ROUTER,
            data: alloy::primitives::Bytes::new(),
        };
        let input = execCall::abi_encode(&call);
        assert!(terms(&input, None).is_none());
    }

    #[test]
    fn test_zero_min_amount_out_still_produces_an_intent() {
        // A legitimate shape: Settler can leave the top-level floor at zero when per-action
        // slippage checks are used instead. The intent is still recorded rather than dropped.
        let execute_call = executeCall {
            slippage: AllowedSlippage {
                recipient: RELAY_ROUTER,
                buyToken: USDC,
                minAmountOut: U256::ZERO,
            },
            actions: vec![],
            zidAndAffiliate: alloy::primitives::FixedBytes::default(),
        };
        let call = execCall {
            operator: RELAY_ROUTER,
            token: Address::ZERO,
            amount: U256::from(1_000u64),
            target: RELAY_ROUTER,
            data: executeCall::abi_encode(&execute_call).into(),
        };
        let input = execCall::abi_encode(&call);
        let intent = terms(&input, None).unwrap();
        assert_eq!(intent.min_amount_out, U256::ZERO);
    }

    #[test]
    fn test_native_eth_sentinel_normalized() {
        assert_eq!(normalize_native(ZEROEX_NATIVE), Address::ZERO);
        assert_eq!(normalize_native(USDC), USDC);
    }
}
