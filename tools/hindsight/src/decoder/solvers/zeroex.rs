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
//! A bare Settler entry (no `AllowanceHolder` wrapper) is the dominant shape in production: 110 of
//! 120 sampled netted `0x` records on Ethereum and Base, carrying $13.6M of notional in one day.
//! Its input side is stated by the **first** action, which is always the one that takes the taker's
//! funds. Four selectors appear in that position, and three of them lead with `address recipient`
//! followed by a Permit2 `PermitTransferFrom`, so the input token and amount sit in the same two
//! words in all three:
//!
//! ```text
//!   recipient  permitted.token  permitted.amount  nonce  deadline
//! ```
//!
//! The fourth (`0xbd01c226`) carries only `(deadline, amount)` and names no token: the taker sent
//! native ETH, so there is nothing to permit. Its amount equalled `tx.value` on all 23 occurrences
//! and netting read native for every one, which is what makes the native reading a decode rather
//! than a guess.
//!
//! The four are pinned by selector, as `liquidmesh.rs`'s event topic is: `TRANSFER_FROM` is
//! `0xc1fb425e`, computed from the published signature, and the other three are VIP actions
//! (`0x3036d6a6` also carries a packed v3 path, `0x931997d3` an order) whose exact signatures are
//! not published in a form that reproduces their hashes, so a `sol!` declaration with a guessed
//! name would silently never match. The permit words are read where they sit instead.
//!
//! Verified against what netting recorded on the same 110 transactions: `token_in` matched on all
//! 110, `token_out` on all 110, and `amount_in` on 104. The six differences are 0x's own cut taken
//! off the input before routing (five of them exactly 15 bps): the calldata states what the taker
//! authorised and netting sees what reached the pools. The authorised amount is what is recorded,
//! matching the `AllowanceHolder` path above and the ruling that a solver's own fee is part of its
//! all-in price.

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

/// Settler's own terms, decoded from an `execute` call regardless of how it was reached (wrapped
/// in `AllowanceHolder.exec` or called directly).
struct SettlerTerms {
    recipient: Address,
    buy_token: Address,
    min_amount_out: U256,
    declared_quote: Option<U256>,
    /// The input side, when the first action stated it. Only a bare entry needs this — the
    /// wrapper states the input itself.
    sold: Option<Sold>,
}

/// What the taker paid, read from the first action of a bare Settler entry.
struct Sold {
    token: Address,
    amount: U256,
}

/// The first actions that lead with `address recipient` then a Permit2 `PermitTransferFrom`, whose
/// second and third words are the input token and amount. Pinned by selector — see the module docs
/// for why they are not declared with `sol!`.
const PERMIT_FIRST_ACTIONS: [[u8; 4]; 3] = [
    [0xc1, 0xfb, 0x42, 0x5e], // TRANSFER_FROM
    [0x30, 0x36, 0xd6, 0xa6],
    [0x93, 0x19, 0x97, 0xd3],
];

/// The first action that names no token: the taker sent native ETH, and the action carries only
/// `(deadline, amount)`.
const NATIVE_FIRST_ACTION: [u8; 4] = [0xbd, 0x01, 0xc2, 0x26];

const WORD: usize = 32;
const ADDRESS_LEN: usize = 20;

/// The 32-byte word at `index` of an action's arguments.
fn word(body: &[u8], index: usize) -> Option<U256> {
    body.get(index * WORD..(index + 1) * WORD)
        .map(U256::from_be_slice)
}

/// The address in the low 20 bytes of the word at `index`.
fn address_at(body: &[u8], index: usize) -> Option<Address> {
    let bytes = body.get(index * WORD + WORD - ADDRESS_LEN..(index + 1) * WORD)?;
    Some(Address::from_slice(bytes))
}

/// What the taker paid, from the first action of Settler's `actions` array.
///
/// `None` when the array is empty or its first action is not one of the four that take the taker's
/// funds — a shape this has not seen, which goes to netting rather than being guessed at.
fn sold(actions: &[alloy::primitives::Bytes]) -> Option<Sold> {
    let action = actions.first()?;
    let (selector, body) = action.split_at_checked(4)?;
    if selector == NATIVE_FIRST_ACTION {
        return Some(Sold { token: Address::ZERO, amount: word(body, 1)? });
    }
    if !PERMIT_FIRST_ACTIONS
        .iter()
        .any(|known| known == selector)
    {
        return None;
    }
    Some(Sold { token: normalize_native(address_at(body, 1)?), amount: word(body, 2)? })
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
        sold: sold(&call.actions),
    })
}

pub(crate) struct ZeroEx;

impl SolverDecoder for ZeroEx {
    /// The trader's swap terms. The output side is always Settler's `AllowedSlippage`
    /// (`buyToken`/`minAmountOut`); the input side comes from whichever entry was used —
    /// `AllowanceHolder.exec`'s own `token`/`amount`, or the first action of a bare entry.
    ///
    /// `minAmountOut` is passed through as-is, including a legitimate zero (Settler's per-action
    /// slippage checks can leave the top-level floor at zero). A zero floor costs nothing
    /// downstream: `declared::recover_output` compares the recovered output against it, and every
    /// output clears zero.
    ///
    /// A bare entry whose first action is not one of the four that take the taker's funds is
    /// declined, so netting carries it rather than the input being guessed at.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        // A bare Settler entry: the wrapper is absent, so the input side comes from the first
        // action instead of from `exec`'s own parameters.
        let (terms, sold) = if let Ok(call) = execCall::abi_decode(input) {
            let Some(terms) = decode_execute(&call.data) else { return Ok(None) };
            (terms, Sold { token: normalize_native(call.token), amount: call.amount })
        } else {
            let Some(mut terms) = decode_execute(input) else { return Ok(None) };
            let Some(sold) = terms.sold.take() else { return Ok(None) };
            (terms, sold)
        };
        // An unlimited approval authorises `U256::MAX` and states no amount at all. Reading that
        // sentinel as the amount spent asked the re-solve to sell `2^256 - 1` USDC and scored the
        // answer as a win worth tens of millions of dollars.
        if sold.amount.is_zero() || sold.amount == U256::MAX {
            return Ok(None);
        }
        // Settler's `AllowedSlippage.recipient` — the address whose receipt anchors the settled
        // amount, same as Fly/KyberSwap.
        let intent = DeclaredSwap::from_calldata(
            sold.token,
            terms.buy_token,
            sold.amount,
            terms.min_amount_out,
        )
        .with_recipient(terms.recipient);
        Ok(Some(match terms.declared_quote {
            Some(quote) => intent.with_quote(quote, None),
            None => intent,
        }))
    }
}

#[cfg(test)]
mod tests {
    /// The terms this solver reads from `input`, for tests that only care about the calldata path.
    fn terms(input: &[u8]) -> Option<DeclaredSwap> {
        ZeroEx
            .declared(input, &[])
            .ok()
            .flatten()
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
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    const RELAY_ROUTER: Address = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");

    #[test]
    fn test_real_settled_declared_swap() {
        let intent = terms(&settled_input()).unwrap();
        assert_eq!(intent.token_in, Address::ZERO); // 0x's native-ETH sentinel, normalized
        assert_eq!(intent.token_out, USDC);
        assert_eq!(intent.amount_in, Some(U256::from(214_715_436_309_542_453u64)));
        assert_eq!(intent.min_amount_out, Some(U256::from(388_129_000u64)));
        assert_eq!(intent.declared_quote, Some(U256::from(396_058_371u64)));
    }

    #[test]
    fn test_real_settled_output_recipient() {
        let intent = terms(&settled_input()).unwrap();
        assert_eq!(intent.output_recipient, Some(RELAY_ROUTER));
    }

    #[test]
    fn test_real_reverted_declared_swap() {
        // The reverted trade's terms decode the same way a settled one's do — a revert emits no
        // logs, so calldata is the only source, and it is read no differently here.
        let intent = terms(&reverted_input()).unwrap();
        assert_eq!(intent.token_in, Address::ZERO);
        assert_eq!(intent.token_out, USDC);
        assert_eq!(intent.amount_in, Some(U256::from(2_018_128_791_326_365_345u64)));
        assert_eq!(intent.min_amount_out, Some(U256::from(3_643_640_000u64)));
        assert_eq!(intent.declared_quote, Some(U256::from(3_718_000_789u64)));
    }

    #[test]
    fn test_real_reverted_output_recipient() {
        let intent = terms(&reverted_input()).unwrap();
        assert_eq!(intent.output_recipient, Some(RELAY_ROUTER));
    }

    #[test]
    fn test_selectors_match_computed_signatures() {
        assert_eq!(execCall::SELECTOR, [0x22, 0x13, 0xbc, 0x0b]);
        assert_eq!(executeCall::SELECTOR, [0x1f, 0xff, 0x99, 0x1f]);
        assert_eq!(POSITIVE_SLIPPAGECall::SELECTOR, [0x34, 0xee, 0x90, 0xca]);
    }

    /// A bare `execute` call carrying `actions`.
    fn bare_settler(actions: Vec<alloy::primitives::Bytes>, buy_token: Address) -> Vec<u8> {
        executeCall::abi_encode(&executeCall {
            slippage: AllowedSlippage {
                recipient: RELAY_ROUTER,
                buyToken: buy_token,
                minAmountOut: U256::from(1_000u64),
            },
            actions,
            zidAndAffiliate: alloy::primitives::FixedBytes::default(),
        })
    }

    /// A first action of the permit-carrying shape: `recipient`, then `permitted.token` and
    /// `permitted.amount`, then two words this does not read.
    fn permit_action(selector: [u8; 4], token: Address, amount: u64) -> alloy::primitives::Bytes {
        let mut body = selector.to_vec();
        for word in [
            U256::from_be_bytes(RELAY_ROUTER.into_word().0),
            U256::from_be_bytes(token.into_word().0),
            U256::from(amount),
            U256::from(7u64),
            U256::from(9u64),
        ] {
            body.extend_from_slice(&word.to_be_bytes::<32>());
        }
        body.into()
    }

    #[test]
    fn test_bare_settler_entry_with_no_actions_declines() {
        // Nothing takes the taker's funds, so there is nowhere to read the input from.
        assert!(terms(&bare_settler(vec![], USDC)).is_none());
    }

    #[test]
    fn test_bare_settler_entry_reads_every_permit_first_action() {
        // All three lead with the same five words, so one read serves them.
        for selector in PERMIT_FIRST_ACTIONS {
            let input = bare_settler(vec![permit_action(selector, USDC, 250_000)], WETH);
            assert!(terms(&input).is_some(), "first action {selector:?} was not read");
            let intent = terms(&input).unwrap();
            assert_eq!(intent.token_in, USDC);
            assert_eq!(intent.token_out, WETH);
            assert_eq!(intent.amount_in, Some(U256::from(250_000u64)));
            assert_eq!(intent.min_amount_out, Some(U256::from(1_000u64)));
            assert_eq!(intent.output_recipient, Some(RELAY_ROUTER));
        }
    }

    #[test]
    fn test_bare_settler_native_first_action_names_no_token() {
        // `(deadline, amount)` only: the taker sent native ETH, so there is nothing to permit.
        let mut body = NATIVE_FIRST_ACTION.to_vec();
        body.extend_from_slice(&U256::from(1_787_706_953u64).to_be_bytes::<32>());
        body.extend_from_slice(&U256::from(407_197_625_223_450u64).to_be_bytes::<32>());
        let intent = terms(&bare_settler(vec![body.into()], USDC)).unwrap();
        assert_eq!(intent.token_in, Address::ZERO);
        assert_eq!(intent.amount_in, Some(U256::from(407_197_625_223_450u64)));
    }

    #[test]
    fn test_bare_settler_unknown_first_action_declines() {
        // `BASIC` never takes the taker's funds, so a stream starting with it is a shape this has
        // not seen and netting carries it.
        let basic = permit_action([0x38, 0xc9, 0xc1, 0x47], USDC, 1_000);
        assert!(terms(&bare_settler(vec![basic], WETH)).is_none());
    }

    #[test]
    fn test_bare_settler_truncated_first_action_declines() {
        let short = permit_action(PERMIT_FIRST_ACTIONS[0], USDC, 1_000);
        let truncated: alloy::primitives::Bytes = short[..40].to_vec().into();
        assert!(terms(&bare_settler(vec![truncated], WETH)).is_none());
    }

    #[test]
    fn test_bare_settler_unlimited_permit_declines() {
        let mut body = PERMIT_FIRST_ACTIONS[0].to_vec();
        for word in [
            U256::from_be_bytes(RELAY_ROUTER.into_word().0),
            U256::from_be_bytes(USDC.into_word().0),
            U256::MAX,
            U256::from(7u64),
            U256::from(9u64),
        ] {
            body.extend_from_slice(&word.to_be_bytes::<32>());
        }
        assert!(terms(&bare_settler(vec![body.into()], WETH)).is_none());
    }

    /// The calldata of a real bare Settler entry on Ethereum
    /// (`0xfcd6b98c5031c297c125b7e7db6717879747e555159e0987a9f2cea3026140e9`): 16 actions, the
    /// first of them the native-ETH shape. Netting recorded the same token pair and the same
    /// amount, and `tx.value` was the amount too.
    fn bare_settler_native_input() -> Vec<u8> {
        let text = include_str!("fixtures/zeroex_bare_settler_native_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    #[test]
    fn test_live_bare_settler_entry() {
        let intent = terms(&bare_settler_native_input()).unwrap();
        assert_eq!(intent.token_in, Address::ZERO);
        assert_eq!(intent.amount_in, Some(U256::from(10_370_000_000_000_000_000u128)));
        assert_eq!(intent.token_out, address!("0xdac17f958d2ee523a2206206994597c13d831ec7"));
        assert_eq!(intent.min_amount_out, Some(U256::from(25_223_142_880u64)));
    }

    #[test]
    fn test_garbage_input_declines() {
        assert!(terms(&[0xde, 0xad, 0xbe, 0xef]).is_none());
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
        assert!(terms(&input).is_none());
    }

    #[test]
    fn test_unlimited_allowance_rejected() {
        // An unlimited approval authorises `U256::MAX` and states no amount. Seven live records
        // read it as the amount spent, which asked the re-solve to sell 2^256 - 1 USDC and scored
        // the answer as a win worth tens of millions of dollars.
        let execute_call = executeCall {
            slippage: AllowedSlippage {
                recipient: RELAY_ROUTER,
                buyToken: USDC,
                minAmountOut: U256::from(1_000u64),
            },
            actions: vec![],
            zidAndAffiliate: alloy::primitives::FixedBytes::default(),
        };
        let call = execCall {
            operator: RELAY_ROUTER,
            token: USDC,
            amount: U256::MAX,
            target: RELAY_ROUTER,
            data: executeCall::abi_encode(&execute_call).into(),
        };
        assert!(terms(&execCall::abi_encode(&call)).is_none());
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
        let intent = terms(&input).unwrap();
        assert_eq!(intent.min_amount_out, Some(U256::ZERO));
    }
}
