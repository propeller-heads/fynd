//! Uniswap `SwapRouter02` calldata extraction.
//!
//! `SwapRouter02` is the router the Universal Router replaced, and it is still the larger share of
//! Uniswap's order flow on Base: of 200 sampled netted `uniswap` records across Ethereum and Base,
//! 111 were `SwapRouter02` calls that `uniswap.rs`'s `execute` reader cannot decode, against 12
//! Universal Router streams. Four entries state a trade — `exactInputSingle`, `exactInput`,
//! `exactOutputSingle`, `exactOutput` — plus the two v2 entries the same router carries, and any of
//! them can arrive wrapped in one of three `multicall` overloads.
//!
//! Verified against what netting recorded on those 111 transactions: the token pair matched on all
//! 111, and `amount_in` matched on 69 of 69 once the price-limit rule below is applied.
//!
//! **A non-zero `sqrtPriceLimitX96` makes `amountIn` a ceiling, not the amount spent.** A v3 swap
//! given a price limit stops when the pool reaches it and consumes only part of the amount, and
//! these calls pair the limit with `amountOutMinimum = 0`, so nothing reverts on the shortfall. In
//! the sample the two populations separate exactly: with no limit, all 69 amounts matched netting;
//! with a limit, 40 of 41 did not, the worst reading 23.75x the amount the trader actually sent
//! (verified on-chain: 33.85e18 stated, 1.43e18 transferred). Reading the stated amount would ask a
//! re-solve to sell 23x the trade and score the answer as a win, the same failure the unlimited
//! `AllowanceHolder` approval produced. Those calls are declined and netting carries them.
//!
//! **Native ETH is not named in the calldata.** The router takes `msg.value` and wraps it itself,
//! so the path names the wrapped token where the trader paid native — 26 of the 111. The wrap is
//! visible in the logs instead: a `Deposit(dst, wad)` emitted by the token the calldata names, for
//! the amount the calldata states, is the router wrapping the trader's ETH. That pairing is what
//! rewrites the input to `Address::ZERO`; the chain's wrapped-native address is not needed, and no
//! address is guessed. A native *output* is named outright by the trailing `unwrapWETH9`.

use alloy::{
    primitives::{b256, Address, Bytes, B256, U256},
    rpc::types::Log,
    sol,
    sol_types::SolCall,
};

use crate::decoder::solvers::{
    uniswap::{readable_recipient, v3_path_ends},
    DeclaredSwap,
};

sol! {
    /// `IV3SwapRouter.ExactInputSingleParams`. No deadline: `SwapRouter02` moved it to
    /// `multicall`, which is what distinguishes this selector from the original router's.
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256);

    /// `IV3SwapRouter.ExactInputParams` — a packed v3 path running input to output.
    struct ExactInputParams {
        bytes path;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
    }

    function exactInput(ExactInputParams params) external payable returns (uint256);

    /// The exact-output mirror: the output is fixed and the input only bounded.
    struct ExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountOut;
        uint256 amountInMaximum;
        uint160 sqrtPriceLimitX96;
    }

    function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256);

    /// `IV3SwapRouter.ExactOutputParams`. Uniswap encodes an exact-output path output-first, so its
    /// ends are read the other way round.
    struct ExactOutputParams {
        bytes path;
        address recipient;
        uint256 amountOut;
        uint256 amountInMaximum;
    }

    function exactOutput(ExactOutputParams params) external payable returns (uint256);

    /// The v2 entries the same router carries. A v2 path always runs input to output.
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] path,
        address to
    ) external payable returns (uint256);

    function swapTokensForExactTokens(
        uint256 amountOut,
        uint256 amountInMax,
        address[] path,
        address to
    ) external payable returns (uint256);

    /// The three `multicall` overloads, all carrying the same `bytes[]` of inner calls.
    function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);
    function multicall(bytes[] data) external payable returns (bytes[] results);
    function multicall(bytes32 previousBlockhash, bytes[] data)
        external
        payable
        returns (bytes[] results);

    /// Pays the router's wrapped-native balance out as native ETH — the trade's output when it
    /// follows the swap.
    function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;

    /// Pays the router's balance of one token out, floor enforced.
    function sweepToken(address token, uint256 amountMinimum, address recipient) external payable;
}

/// `WETH9.Deposit(address indexed dst, uint256 wad)` — the router wrapping native ETH.
const DEPOSIT_TOPIC: B256 =
    b256!("0xe1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c");

/// Which side of the trade the call fixes, and the two amounts it states.
enum Amounts {
    ExactIn { amount_in: U256, min_amount_out: U256 },
    ExactOut { amount_out: U256, max_amount_in: U256 },
}

impl Amounts {
    /// Whether the fixed side is zero, which no real trade states.
    fn is_zero(&self) -> bool {
        match self {
            Self::ExactIn { amount_in, .. } => amount_in.is_zero(),
            Self::ExactOut { amount_out, .. } => amount_out.is_zero(),
        }
    }
}

/// One swap call's terms.
struct Swap {
    token_in: Address,
    token_out: Address,
    amounts: Amounts,
    /// The call's declared recipient, unless it is one of the router's sentinels.
    recipient: Option<Address>,
}

/// The command that pays the output out when the swap sent it to the router instead.
#[derive(Clone, Copy)]
struct Payout {
    /// `Address::ZERO` for `unwrapWETH9`, which always pays native ETH.
    token: Address,
    recipient: Option<Address>,
    min_amount_out: U256,
}

/// The inner calls of a `multicall`, or the call itself when it is not one.
///
/// Nesting is not followed: `SwapRouter02` does not nest `multicall`s, and a nested one would be a
/// shape this has not seen rather than one to guess at.
fn inner_calls(input: &[u8]) -> Vec<Bytes> {
    if let Ok(call) = multicall_1Call::abi_decode(input) {
        return call.data;
    }
    if let Ok(call) = multicall_0Call::abi_decode(input) {
        return call.data;
    }
    if let Ok(call) = multicall_2Call::abi_decode(input) {
        return call.data;
    }
    vec![Bytes::copy_from_slice(input)]
}

/// The first and last token of a v2 `address[]` path.
fn v2_path_ends(path: &[Address]) -> Option<(Address, Address)> {
    match (path.first(), path.last()) {
        (Some(first), Some(last)) if path.len() >= 2 => Some((*first, *last)),
        _ => None,
    }
}

/// One swap call's terms, or `None` when the call is not a swap this reads.
///
/// A `*Single` call carrying a non-zero `sqrtPriceLimitX96` is not read at all: see the module
/// docs for why its stated amount is a ceiling.
fn read_swap(input: &[u8]) -> Option<Swap> {
    if let Ok(call) = exactInputSingleCall::abi_decode(input) {
        let params = call.params;
        if !params.sqrtPriceLimitX96.is_zero() {
            return None;
        }
        return Some(Swap {
            token_in: params.tokenIn,
            token_out: params.tokenOut,
            amounts: Amounts::ExactIn {
                amount_in: params.amountIn,
                min_amount_out: params.amountOutMinimum,
            },
            recipient: readable_recipient(params.recipient),
        });
    }
    if let Ok(call) = exactOutputSingleCall::abi_decode(input) {
        let params = call.params;
        if !params.sqrtPriceLimitX96.is_zero() {
            return None;
        }
        return Some(Swap {
            token_in: params.tokenIn,
            token_out: params.tokenOut,
            amounts: Amounts::ExactOut {
                amount_out: params.amountOut,
                max_amount_in: params.amountInMaximum,
            },
            recipient: readable_recipient(params.recipient),
        });
    }
    if let Ok(call) = exactInputCall::abi_decode(input) {
        let params = call.params;
        let (token_in, token_out) = v3_path_ends(&params.path)?;
        return Some(Swap {
            token_in,
            token_out,
            amounts: Amounts::ExactIn {
                amount_in: params.amountIn,
                min_amount_out: params.amountOutMinimum,
            },
            recipient: readable_recipient(params.recipient),
        });
    }
    if let Ok(call) = exactOutputCall::abi_decode(input) {
        let params = call.params;
        // An exact-output path is encoded output-first.
        let (token_out, token_in) = v3_path_ends(&params.path)?;
        return Some(Swap {
            token_in,
            token_out,
            amounts: Amounts::ExactOut {
                amount_out: params.amountOut,
                max_amount_in: params.amountInMaximum,
            },
            recipient: readable_recipient(params.recipient),
        });
    }
    if let Ok(call) = swapExactTokensForTokensCall::abi_decode(input) {
        let (token_in, token_out) = v2_path_ends(&call.path)?;
        return Some(Swap {
            token_in,
            token_out,
            amounts: Amounts::ExactIn {
                amount_in: call.amountIn,
                min_amount_out: call.amountOutMin,
            },
            recipient: readable_recipient(call.to),
        });
    }
    if let Ok(call) = swapTokensForExactTokensCall::abi_decode(input) {
        let (token_in, token_out) = v2_path_ends(&call.path)?;
        return Some(Swap {
            token_in,
            token_out,
            amounts: Amounts::ExactOut {
                amount_out: call.amountOut,
                max_amount_in: call.amountInMax,
            },
            recipient: readable_recipient(call.to),
        });
    }
    None
}

/// The payout a call states, when it is one of the two that pay the router's balance out.
fn read_payout(input: &[u8]) -> Option<Payout> {
    if let Ok(call) = unwrapWETH9Call::abi_decode(input) {
        return Some(Payout {
            token: Address::ZERO,
            recipient: readable_recipient(call.recipient),
            min_amount_out: call.amountMinimum,
        });
    }
    let call = sweepTokenCall::abi_decode(input).ok()?;
    Some(Payout {
        token: call.token,
        recipient: readable_recipient(call.recipient),
        min_amount_out: call.amountMinimum,
    })
}

/// Whether the logs show the router wrapping `amount` of `token` — the trader paid native ETH and
/// the calldata names the wrapped token.
///
/// Both the emitter and the amount have to match: a `Deposit` of some other amount is the router
/// wrapping for a different purpose, and one from a different token says nothing about this trade.
fn wrapped_the_input(logs: &[Log], token: Address, amount: U256) -> bool {
    logs.iter().any(|log| {
        log.address() == token &&
            log.topics().first() == Some(&DEPOSIT_TOPIC) &&
            U256::from_be_slice(log.data().data.as_ref()) == amount
    })
}

/// The trade a `SwapRouter02` call states, or `None` when the calldata is not one of its swap
/// entries, carries more than one swap, or states an amount that is only a bound.
///
/// Declines a call stream carrying more than one swap, for the same reason the Universal Router
/// reader does: a route split across calls has no single one that is the trade.
pub(super) fn declared(input: &[u8], logs: &[Log]) -> Option<DeclaredSwap> {
    let calls = inner_calls(input);
    let mut swap: Option<Swap> = None;
    let mut payout: Option<Payout> = None;
    for call in &calls {
        if let Some(read) = read_swap(call) {
            if swap.is_some() {
                return None;
            }
            swap = Some(read);
            continue;
        }
        // Only a payout after the swap is this trade's output; one before it clears an earlier
        // balance.
        if swap.is_some() && payout.is_none() {
            payout = read_payout(call);
        }
    }
    let swap = swap?;
    if swap.amounts.is_zero() {
        return None;
    }
    // A trailing `unwrapWETH9` names a native output outright.
    let token_out = match payout {
        Some(paid) if paid.token == Address::ZERO => Address::ZERO,
        _ => swap.token_out,
    };
    let token_in = match &swap.amounts {
        Amounts::ExactIn { amount_in, .. }
            if wrapped_the_input(logs, swap.token_in, *amount_in) =>
        {
            Address::ZERO
        }
        _ => swap.token_in,
    };
    // A path that ends in the token it started from is a bot cycling pools, not a trade a re-solve
    // can price.
    if token_in == token_out {
        return None;
    }
    // Only a payout of this trade's own output token says anything about it.
    let payout = payout.filter(|paid| paid.token == token_out);
    let declared = match swap.amounts {
        Amounts::ExactIn { amount_in, min_amount_out } => DeclaredSwap::from_calldata(
            token_in,
            token_out,
            amount_in,
            // The stricter of the two floors: a swap paying the router leaves its own at zero.
            min_amount_out.max(payout.map_or(U256::ZERO, |paid| paid.min_amount_out)),
        ),
        Amounts::ExactOut { amount_out, max_amount_in } => {
            DeclaredSwap::from_calldata_exact_out(token_in, token_out, amount_out, max_amount_in)
        }
    };
    let recipient = swap
        .recipient
        .or_else(|| payout.and_then(|paid| paid.recipient));
    Some(match recipient {
        Some(recipient) => declared.with_recipient(recipient),
        None => declared,
    })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, LogData};

    use super::*;

    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    const TRADER: Address = address!("0x000000000000000000000000000000000000dead");

    fn fee() -> alloy::primitives::Uint<24, 1> {
        alloy::primitives::Uint::<24, 1>::from(3000u32)
    }

    /// A packed v3 path: `token (fee token)+`.
    fn v3_path(tokens: &[Address]) -> Bytes {
        let mut path = tokens[0].to_vec();
        for token in &tokens[1..] {
            path.extend_from_slice(&[0x00, 0x0b, 0xb8]);
            path.extend_from_slice(token.as_slice());
        }
        path.into()
    }

    fn exact_in_single(recipient: Address, amount_in: u64, floor: u64, limit: u128) -> Vec<u8> {
        exactInputSingleCall {
            params: ExactInputSingleParams {
                tokenIn: USDC,
                tokenOut: WETH,
                fee: fee(),
                recipient,
                amountIn: U256::from(amount_in),
                amountOutMinimum: U256::from(floor),
                sqrtPriceLimitX96: alloy::primitives::Uint::<160, 3>::from(limit),
            },
        }
        .abi_encode()
    }

    /// A `WETH9.Deposit` log: the router wrapping `amount` of native ETH into `token`.
    fn deposit_log(token: Address, amount: u64) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address: token,
                data: LogData::new_unchecked(
                    vec![DEPOSIT_TOPIC, B256::from(TRADER.into_word())],
                    U256::from(amount)
                        .to_be_bytes_vec()
                        .into(),
                ),
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_selectors_against_the_deployed_router() {
        assert_eq!(exactInputSingleCall::SELECTOR, [0x04, 0xe4, 0x5a, 0xaf]);
        assert_eq!(exactInputCall::SELECTOR, [0xb8, 0x58, 0x18, 0x3f]);
        assert_eq!(exactOutputSingleCall::SELECTOR, [0x50, 0x23, 0xb4, 0xdf]);
        assert_eq!(exactOutputCall::SELECTOR, [0x09, 0xb8, 0x13, 0x46]);
        assert_eq!(multicall_0Call::SELECTOR, [0x5a, 0xe4, 0x01, 0xdc]);
        assert_eq!(multicall_1Call::SELECTOR, [0xac, 0x96, 0x50, 0xd8]);
        assert_eq!(multicall_2Call::SELECTOR, [0x1f, 0x04, 0x64, 0xd1]);
        assert_eq!(swapExactTokensForTokensCall::SELECTOR, [0x47, 0x2b, 0x43, 0xf3]);
        assert_eq!(unwrapWETH9Call::SELECTOR, [0x49, 0x40, 0x4b, 0x7c]);
        assert_eq!(sweepTokenCall::SELECTOR, [0xdf, 0x2a, 0xb5, 0xbb]);
    }

    #[test]
    fn test_exact_input_single() {
        let read = declared(&exact_in_single(TRADER, 100_000_000, 5, 0), &[]).unwrap();
        assert_eq!(read.token_in, USDC);
        assert_eq!(read.token_out, WETH);
        assert_eq!(read.amount_in, Some(U256::from(100_000_000u64)));
        assert_eq!(read.min_amount_out, Some(U256::from(5u64)));
        assert_eq!(read.output_recipient, Some(TRADER));
        assert_eq!(read.amount_out, None);
    }

    #[test]
    fn test_price_limit_declines() {
        // The live shape whose stated amount was 23.75x what the trader sent: a price limit with a
        // zero floor, so the swap stops early and nothing reverts.
        assert!(declared(&exact_in_single(TRADER, 33_848_328_364_692_955, 0, 1), &[]).is_none());
    }

    #[test]
    fn test_exact_input_path_ends() {
        let mid = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
        let call = exactInputCall {
            params: ExactInputParams {
                path: v3_path(&[USDC, mid, WETH]),
                recipient: TRADER,
                amountIn: U256::from(1_000u64),
                amountOutMinimum: U256::from(1u64),
            },
        }
        .abi_encode();
        let read = declared(&call, &[]).unwrap();
        assert_eq!(read.token_in, USDC);
        assert_eq!(read.token_out, WETH);
    }

    #[test]
    fn test_exact_output_path_is_read_backwards() {
        let call = exactOutputCall {
            params: ExactOutputParams {
                path: v3_path(&[WETH, USDC]),
                recipient: TRADER,
                amountOut: U256::from(500u64),
                amountInMaximum: U256::from(1_000u64),
            },
        }
        .abi_encode();
        let read = declared(&call, &[]).unwrap();
        assert_eq!(read.token_in, USDC);
        assert_eq!(read.token_out, WETH);
        assert_eq!(read.amount_out, Some(U256::from(500u64)));
        assert_eq!(read.max_amount_in, Some(U256::from(1_000u64)));
        assert_eq!(read.amount_in, None);
    }

    #[test]
    fn test_v2_entry() {
        let call = swapExactTokensForTokensCall {
            amountIn: U256::from(2_000u64),
            amountOutMin: U256::from(7u64),
            path: vec![USDC, WETH],
            to: TRADER,
        }
        .abi_encode();
        let read = declared(&call, &[]).unwrap();
        assert_eq!(read.token_in, USDC);
        assert_eq!(read.token_out, WETH);
        assert_eq!(read.amount_in, Some(U256::from(2_000u64)));
    }

    #[test]
    fn test_multicall_carries_the_swap() {
        let call = multicall_0Call {
            deadline: U256::from(1u64),
            data: vec![exact_in_single(TRADER, 1_000, 5, 0).into()],
        }
        .abi_encode();
        let read = declared(&call, &[]).unwrap();
        assert_eq!(read.token_in, USDC);
        assert_eq!(read.amount_in, Some(U256::from(1_000u64)));
    }

    #[test]
    fn test_multicall_unwrap_pays_a_native_output() {
        let router = Address::with_last_byte(2);
        let call = multicall_1Call {
            data: vec![
                exact_in_single(router, 1_000, 0, 0).into(),
                unwrapWETH9Call { amountMinimum: U256::from(990u64), recipient: TRADER }
                    .abi_encode()
                    .into(),
            ],
        }
        .abi_encode();
        let read = declared(&call, &[]).unwrap();
        assert_eq!(read.token_in, USDC);
        assert_eq!(read.token_out, Address::ZERO);
        assert_eq!(read.output_recipient, Some(TRADER));
        assert_eq!(read.min_amount_out, Some(U256::from(990u64)));
    }

    #[test]
    fn test_deposit_log_makes_the_input_native() {
        // The router wrapped the trader's ETH, so the calldata names WETH where the trader paid
        // native. 26 of 111 sampled live calls were this shape.
        let call = exactInputCall {
            params: ExactInputParams {
                path: v3_path(&[WETH, USDC]),
                recipient: TRADER,
                amountIn: U256::from(1_000u64),
                amountOutMinimum: U256::from(1u64),
            },
        }
        .abi_encode();
        let read = declared(&call, &[deposit_log(WETH, 1_000)]).unwrap();
        assert_eq!(read.token_in, Address::ZERO);
        assert_eq!(read.token_out, USDC);
    }

    #[test]
    fn test_deposit_of_another_amount_is_not_this_input() {
        let call = exactInputCall {
            params: ExactInputParams {
                path: v3_path(&[WETH, USDC]),
                recipient: TRADER,
                amountIn: U256::from(1_000u64),
                amountOutMinimum: U256::from(1u64),
            },
        }
        .abi_encode();
        let read = declared(&call, &[deposit_log(WETH, 7)]).unwrap();
        assert_eq!(read.token_in, WETH);
    }

    #[test]
    fn test_deposit_of_another_token_is_not_this_input() {
        let read =
            declared(&exact_in_single(TRADER, 1_000, 5, 0), &[deposit_log(WETH, 1_000)]).unwrap();
        assert_eq!(read.token_in, USDC);
    }

    #[test]
    fn test_split_across_calls_declines() {
        let call = multicall_1Call {
            data: vec![
                exact_in_single(TRADER, 600, 1, 0).into(),
                exact_in_single(TRADER, 400, 1, 0).into(),
            ],
        }
        .abi_encode();
        assert!(declared(&call, &[]).is_none());
    }

    #[test]
    fn test_round_trip_path_declines() {
        let call = exactInputCall {
            params: ExactInputParams {
                path: v3_path(&[USDC, WETH, USDC]),
                recipient: TRADER,
                amountIn: U256::from(1_000u64),
                amountOutMinimum: U256::from(1u64),
            },
        }
        .abi_encode();
        assert!(declared(&call, &[]).is_none());
    }

    #[test]
    fn test_router_sentinel_leaves_the_recipient_unset() {
        let read = declared(&exact_in_single(Address::with_last_byte(2), 100, 1, 0), &[]).unwrap();
        assert_eq!(read.output_recipient, None);
    }

    #[test]
    fn test_zero_amount_declines() {
        assert!(declared(&exact_in_single(TRADER, 0, 1, 0), &[]).is_none());
    }

    #[test]
    fn test_payout_before_the_swap_is_not_the_output() {
        let call = multicall_1Call {
            data: vec![
                unwrapWETH9Call { amountMinimum: U256::from(900u64), recipient: TRADER }
                    .abi_encode()
                    .into(),
                exact_in_single(Address::with_last_byte(2), 1_000, 0, 0).into(),
            ],
        }
        .abi_encode();
        let read = declared(&call, &[]).unwrap();
        assert_eq!(read.token_out, WETH);
        assert_eq!(read.output_recipient, None);
    }

    /// A live `SwapRouter02` transaction's full calldata.
    fn fixture(text: &str) -> Vec<u8> {
        let text = text.trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    #[test]
    fn test_live_exact_input_single_with_a_calldata_suffix() {
        // Base tx 0x17ecdeba8db2f3a0fc341f8d678afd3a963b9342d9d3e28e0fdabddbbe4daefb. Its caller
        // appends 29 bytes of its own tag after the params, so a decode that rejects trailing
        // bytes would decline a real trade. `amount_in` matches what netting recorded exactly.
        let read = declared(
            &fixture(include_str!("fixtures/swap_router_02_exact_in_single_input.txt")),
            &[],
        )
        .unwrap();
        assert_eq!(read.token_in, address!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"));
        assert_eq!(read.token_out, address!("0x2030534095a1fc00a893663b95d19dad5a59df7b"));
        assert_eq!(read.amount_in, Some(U256::from(637_627u64)));
        assert_eq!(read.min_amount_out, Some(U256::ZERO));
        assert_eq!(
            read.output_recipient,
            Some(address!("0x135799b33f176faa60e10bc9482c06d206d50cd0"))
        );
    }

    #[test]
    fn test_live_price_limit_call_declines() {
        // Base tx 0xab58fd101b7f46fcbc487b4138c088b460763d24f09ba1b5582f6d4f6ccf3b83: states
        // 33.85e18 in with a price limit and a zero floor, and the trader sent 1.43e18. Reading
        // the stated amount would ask a re-solve to sell 23.75x the trade.
        assert!(declared(
            &fixture(include_str!("fixtures/swap_router_02_price_limit_input.txt")),
            &[]
        )
        .is_none());
    }

    #[test]
    fn test_garbage_and_non_swap_calls_decline() {
        assert!(declared(&[], &[]).is_none());
        assert!(declared(&[0xde, 0xad, 0xbe, 0xef], &[]).is_none());
        let sweep_only = multicall_1Call {
            data: vec![sweepTokenCall {
                token: USDC,
                amountMinimum: U256::from(1u64),
                recipient: TRADER,
            }
            .abi_encode()
            .into()],
        }
        .abi_encode();
        assert!(declared(&sweep_only, &[]).is_none());
    }
}
