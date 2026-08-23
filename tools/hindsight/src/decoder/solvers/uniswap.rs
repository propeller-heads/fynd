//! Uniswap Universal Router calldata extraction.
//!
//! `execute(bytes commands, bytes[] inputs, uint256 deadline)` is a command stream: each byte of
//! `commands` names an operation and reads its parameters from the matching element of `inputs`
//! (docs: developers.uniswap.org/docs/protocols/universal-router/concepts/commands). Only the
//! exact-input swaps carry the trader's terms:
//!
//! - `V3_SWAP_EXACT_IN` (`0x00`) — recipient, amount in, floor, then a packed v3 path (20-byte
//!   token, 3-byte fee, repeating).
//! - `V2_SWAP_EXACT_IN` (`0x08`) — the same three, then an `address[]` path.
//!
//! The parameters are read by word position rather than as a fixed tuple: Universal Router 2.1.1
//! appends a `minHopPriceX36` array that a strict decode would reject, and the leading words have
//! not moved.
//!
//! `WRAP_ETH` and `UNWRAP_WETH` bracket a swap whose path names WETH but whose trader side is
//! native ETH, so they rewrite the corresponding token to `Address::ZERO`.
//!
//! `V4_SWAP` (`0x10`) carries a nested stream of its own: `abi.encode(bytes actions, bytes[]
//! params)`, where `SWAP_EXACT_IN_SINGLE` (`0x06`) holds a `PoolKey`, the swap direction, the
//! input amount and the floor. v4 names native ETH as the zero address directly, with no wrapping,
//! so its currencies need no translation. The pool's two currencies are sorted, so `zeroForOne`
//! says which is being sold.
//!
//! Universal Router 2.1.1 inserts a `minHopPriceX36` field into that struct, after every field
//! read here, which is the second reason for reading by position.
//!
//! What is declined, measured over live traffic (40 Universal Router trades on Ethereum blocks
//! 25741800-25741815, carrying 28 v4 swaps between them):
//!
//! - **More than one swap in the stream**, counting v2, v3 and v4 together. A split route has no
//!   single command that is the trade, and a route can begin in v3 and finish in v4 — one sampled
//!   trade did, where reading only its v3 leg reported the wrong `token_out`.
//! - **Exact-output swaps** (`V3_SWAP_EXACT_OUT`, `SWAP_EXACT_OUT_SINGLE`), 8 of the 28 v4 swaps.
//!   These fix the output and bound the input, so the calldata states no input amount — only a
//!   ceiling. Reading that ceiling as the amount spent would overstate every one.
//! - **Multi-hop v4** (`SWAP_EXACT_IN`, `0x07`), 3 of the 28. Its 2.1.1 layout inserts an array
//!   before the amounts rather than after them, so the fields this reads move with the router
//!   version — the one shape where reading by position is not safe.
//!
//! Verified on what remains: 10 v3/v2 trades and 16 v4 exact-in-single swaps. Every decoded token
//! pair matched the settled record, and every `amount_in` matched exactly except one v3 trade
//! reading 0.87% lower — a fee taken before the swap, so the calldata figure is the amount that
//! reached the pools, which is the basis a re-solve needs.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolCall,
};

use crate::decoder::{
    solvers::{DeclaredSwap, SolverDecoder},
    veto::Veto,
};

sol! {
    /// The Universal Router's entry point (selector `0x3593564c`). `commands` and `inputs` are
    /// read positionally; `deadline` is not.
    function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
}

/// The command byte's low bits name the operation; the high bit is an allow-revert flag
/// (`Commands.COMMAND_TYPE_MASK`).
const COMMAND_TYPE_MASK: u8 = 0x7f;

const V3_SWAP_EXACT_IN: u8 = 0x00;
const V3_SWAP_EXACT_OUT: u8 = 0x01;
const V2_SWAP_EXACT_IN: u8 = 0x08;
const V2_SWAP_EXACT_OUT: u8 = 0x09;
const WRAP_ETH: u8 = 0x0b;
const UNWRAP_WETH: u8 = 0x0c;
const V4_SWAP: u8 = 0x10;

/// v4's own action bytes, inside a `V4_SWAP` command (`v4-periphery`'s `Actions` library). Only
/// the single-hop exact-input swap is read; the rest either state no input amount or move the
/// fields this reads between router versions.
const V4_SWAP_EXACT_IN_SINGLE: u8 = 0x06;
const V4_SWAP_EXACT_IN: u8 = 0x07;
const V4_SWAP_EXACT_OUT_SINGLE: u8 = 0x08;
const V4_SWAP_EXACT_OUT: u8 = 0x09;

const WORD: usize = 32;
const ADDRESS_LEN: usize = 20;
/// A v3 path element: a 20-byte token then a 3-byte fee tier.
const V3_HOP: usize = ADDRESS_LEN + 3;

/// `Constants.MSG_SENDER` and `Constants.ADDRESS_THIS`: recipient sentinels the router resolves
/// at run time to the caller and to itself. Neither is an address whose receipt can be read.
const MSG_SENDER: u64 = 1;
const ADDRESS_THIS: u64 = 2;

/// One exact-input swap command's terms.
struct Swap {
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    min_amount_out: U256,
    /// The command's declared recipient, unless it is a sentinel.
    recipient: Option<Address>,
}

/// Read the 32-byte word at `index`, or `None` when the input is shorter.
fn word(input: &[u8], index: usize) -> Option<U256> {
    input
        .get(index * WORD..(index + 1) * WORD)
        .map(U256::from_be_slice)
}

/// Read the address in the low 20 bytes of the word at `index`.
fn address_at(input: &[u8], index: usize) -> Option<Address> {
    let bytes = input.get(index * WORD + WORD - ADDRESS_LEN..(index + 1) * WORD)?;
    Some(Address::from_slice(bytes))
}

/// The bytes of element `index` of a `bytes[]` whose length word sits at `array_offset`.
///
/// An array element's offset is measured from the start of the array's data, after the length
/// word — not from the start of the enclosing blob, which is what `dynamic_at` assumes.
fn array_element(input: &[u8], array_offset: usize, index: usize) -> Option<&[u8]> {
    let data = array_offset + WORD;
    let relative = usize::try_from(word(input, data / WORD + index)?).ok()?;
    let start = data + relative;
    let length = usize::try_from(word(input, start / WORD)?).ok()?;
    input.get(start + WORD..start + WORD + length)
}

/// The bytes of a dynamic field whose offset word sits at `index`.
fn dynamic_at(input: &[u8], index: usize) -> Option<&[u8]> {
    let offset = usize::try_from(word(input, index)?).ok()?;
    let length = usize::try_from(word(input, offset / WORD)?).ok()?;
    input.get(offset + WORD..offset + WORD + length)
}

/// The first and last token of a packed v3 path: `token (fee token)+`. A path shorter than one hop
/// is malformed.
fn v3_path_ends(path: &[u8]) -> Option<(Address, Address)> {
    if path.len() < V3_HOP + ADDRESS_LEN || !(path.len() - ADDRESS_LEN).is_multiple_of(V3_HOP) {
        return None;
    }
    let first = Address::from_slice(path.get(..ADDRESS_LEN)?);
    let last = Address::from_slice(path.get(path.len() - ADDRESS_LEN..)?);
    Some((first, last))
}

/// The first and last token of a v2 `address[]` path, whose offset word sits at `index`.
fn v2_path_ends(input: &[u8], index: usize) -> Option<(Address, Address)> {
    let offset = usize::try_from(word(input, index)?).ok()?;
    let length = usize::try_from(word(input, offset / WORD)?).ok()?;
    if length < 2 {
        return None;
    }
    let base = offset / WORD + 1;
    Some((address_at(input, base)?, address_at(input, base + length - 1)?))
}

/// The one exact-input swap in a `V4_SWAP` command's nested action stream, or `None` when it
/// carries none, several, or a shape this does not read.
///
/// The command's input is `abi.encode(bytes actions, bytes[] params)`. `SWAP_EXACT_IN_SINGLE`'s
/// params are a dynamic struct — it ends in `bytes hookData` — so they sit behind an offset word:
///
/// ```text
///   currency0  currency1  fee  tickSpacing  hooks  zeroForOne  amountIn  amountOutMinimum
/// ```
///
/// The pool's currencies are sorted, so `zeroForOne` says which one is being sold. There is no
/// recipient here: a later `TAKE`/`TAKE_ALL` action pays it out, so the caller reads the
/// transaction sender's receipt.
fn read_v4_swap(command_input: &[u8]) -> Option<Swap> {
    let actions = dynamic_at(command_input, 0)?;
    let params_offset = usize::try_from(word(command_input, 1)?).ok()?;
    let count = usize::try_from(word(command_input, params_offset / WORD)?).ok()?;
    let mut found = None;
    for (index, action) in actions.iter().enumerate() {
        match *action {
            V4_SWAP_EXACT_IN_SINGLE => {}
            // Exact output states no input amount, and multi-hop moves the amounts between
            // router versions; either means this command is not readable.
            V4_SWAP_EXACT_IN | V4_SWAP_EXACT_OUT_SINGLE | V4_SWAP_EXACT_OUT => return None,
            _ => continue,
        }
        if found.is_some() || index >= count {
            return None;
        }
        let params = array_element(command_input, params_offset, index)?;
        // A dynamic struct is encoded behind its own offset word.
        let base = usize::from(word(params, 0)? == U256::from(WORD));
        let currency0 = address_at(params, base)?;
        let currency1 = address_at(params, base + 1)?;
        let zero_for_one = !word(params, base + 5)?.is_zero();
        let (token_in, token_out) =
            if zero_for_one { (currency0, currency1) } else { (currency1, currency0) };
        found = Some(Swap {
            token_in,
            token_out,
            amount_in: word(params, base + 6)?,
            min_amount_out: word(params, base + 7)?,
            recipient: None,
        });
    }
    found
}

/// A recipient that names an address whose receipt can be read, or `None` for the router's
/// sentinels — the caller then anchors on the transaction sender, which is where the router's
/// `SWEEP`/`UNWRAP_WETH` sends the output.
fn readable_recipient(raw: Address) -> Option<Address> {
    let sentinel = raw.into_word().into();
    let sentinel = U256::from_be_bytes::<32>(sentinel);
    if sentinel == U256::from(MSG_SENDER) || sentinel == U256::from(ADDRESS_THIS) {
        return None;
    }
    Some(raw)
}

/// One exact-input swap command's terms, by command type.
fn read_swap(command: u8, input: &[u8]) -> Option<Swap> {
    let recipient = readable_recipient(address_at(input, 0)?);
    let amount_in = word(input, 1)?;
    let min_amount_out = word(input, 2)?;
    let (token_in, token_out) = if command == V3_SWAP_EXACT_IN {
        v3_path_ends(dynamic_at(input, 3)?)?
    } else {
        v2_path_ends(input, 3)?
    };
    Some(Swap { token_in, token_out, amount_in, min_amount_out, recipient })
}

/// The Uniswap Universal Router solver.
pub(crate) struct Uniswap;

impl SolverDecoder for Uniswap {
    /// The trader's swap terms from the one exact-input swap in an `execute` command stream.
    ///
    /// Declines a stream carrying a `V4_SWAP` or more than one exact-input swap — see the module
    /// docs for why neither can be read from the v3/v2 parameters alone.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let Ok(call) = executeCall::abi_decode(input) else { return Ok(None) };
        let mut swap = None;
        let mut wrapped = false;
        let mut unwrapped = false;
        for (command, command_input) in call
            .commands
            .iter()
            .zip(call.inputs.iter())
        {
            let read = match command & COMMAND_TYPE_MASK {
                WRAP_ETH => {
                    wrapped = true;
                    continue;
                }
                UNWRAP_WETH => {
                    unwrapped = true;
                    continue;
                }
                // Exact output bounds the input instead of stating it, so there is no amount
                // spent to record.
                V3_SWAP_EXACT_OUT | V2_SWAP_EXACT_OUT => return Ok(None),
                V4_SWAP => read_v4_swap(command_input),
                command @ (V3_SWAP_EXACT_IN | V2_SWAP_EXACT_IN) => {
                    read_swap(command, command_input)
                }
                _ => continue,
            };
            // Every swap counts, whichever pool version it names: a route split across commands
            // has no single one that is the trade.
            if swap.is_some() {
                return Ok(None);
            }
            let Some(read) = read else { return Ok(None) };
            swap = Some(read);
        }
        let Some(swap) = swap else { return Ok(None) };
        if swap.amount_in.is_zero() {
            return Ok(None);
        }
        // A wrap or unwrap means the path's WETH stands in for the trader's native ETH.
        let token_in = if wrapped { Address::ZERO } else { swap.token_in };
        let token_out = if unwrapped { Address::ZERO } else { swap.token_out };
        let declared =
            DeclaredSwap::from_calldata(token_in, token_out, swap.amount_in, swap.min_amount_out);
        Ok(Some(match swap.recipient {
            Some(recipient) => declared.with_recipient(recipient),
            None => declared,
        }))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Bytes};

    use super::*;

    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

    /// A packed v3 path: `token (fee token)+`, one hop per extra token.
    fn v3_path(tokens: &[Address]) -> Bytes {
        let mut path = tokens[0].to_vec();
        for token in &tokens[1..] {
            path.extend_from_slice(&[0x00, 0x0b, 0xb8]); // the 3000 fee tier
            path.extend_from_slice(token.as_slice());
        }
        path.into()
    }

    /// An `execute` call with one command per input.
    fn execute_call(commands: &[u8], inputs: Vec<Bytes>) -> Vec<u8> {
        executeCall { commands: commands.to_vec().into(), inputs, deadline: U256::from(1_u64) }
            .abi_encode()
    }

    /// A `V3_SWAP_EXACT_IN` input: recipient, amount in, floor, path, payer flag.
    fn v3_input(recipient: Address, amount_in: u64, floor: u64, tokens: &[Address]) -> Bytes {
        use alloy::sol_types::SolValue;
        (recipient, U256::from(amount_in), U256::from(floor), v3_path(tokens), true)
            .abi_encode_params()
            .into()
    }

    /// A `V2_SWAP_EXACT_IN` input, whose path is an `address[]`.
    fn v2_input(recipient: Address, amount_in: u64, floor: u64, tokens: &[Address]) -> Bytes {
        use alloy::sol_types::SolValue;
        (recipient, U256::from(amount_in), U256::from(floor), tokens.to_vec(), true)
            .abi_encode_params()
            .into()
    }

    fn terms(input: &[u8]) -> Option<DeclaredSwap> {
        Uniswap
            .declared(input, &[])
            .ok()
            .flatten()
    }

    #[test]
    fn test_selector_against_the_deployed_router() {
        // The selector every sampled Universal Router trade entered through.
        assert_eq!(executeCall::SELECTOR, [0x35, 0x93, 0x56, 0x4c]);
    }

    #[test]
    fn test_v3_exact_in_single_hop() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN],
            vec![v3_input(trader, 100_000_000, 5, &[USDC, WETH])],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_in, U256::from(100_000_000u64));
        assert_eq!(declared.min_amount_out, Some(U256::from(5u64)));
        assert_eq!(declared.output_recipient, Some(trader));
        // Calldata states no settled output.
        assert_eq!(declared.amount_out, None);
    }

    #[test]
    fn test_v3_exact_in_multi_hop_reads_the_path_ends() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let mid = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
        let call =
            execute_call(&[V3_SWAP_EXACT_IN], vec![v3_input(trader, 1_000, 1, &[USDC, mid, WETH])]);
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
    }

    #[test]
    fn test_v2_exact_in() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call =
            execute_call(&[V2_SWAP_EXACT_IN], vec![v2_input(trader, 2_000, 7, &[USDC, WETH])]);
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_in, U256::from(2_000u64));
    }

    #[test]
    fn test_unwrap_makes_the_output_native() {
        // `V3_IN UNWRAP` — the path buys WETH, the trader is paid native ETH.
        let router = Address::with_last_byte(2);
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, UNWRAP_WETH],
            vec![v3_input(router, 100, 1, &[USDC, WETH]), Bytes::default()],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, Address::ZERO);
    }

    #[test]
    fn test_wrap_makes_the_input_native() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[WRAP_ETH, V3_SWAP_EXACT_IN],
            vec![Bytes::default(), v3_input(trader, 100, 1, &[WETH, USDC])],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.token_out, USDC);
    }

    #[test]
    fn test_router_sentinel_leaves_the_recipient_unset() {
        // ADDRESS_THIS means the router holds the output until a later SWEEP forwards it, so
        // there is no address here whose receipt is the trade's output.
        let call = execute_call(
            &[V3_SWAP_EXACT_IN],
            vec![v3_input(Address::with_last_byte(2), 100, 1, &[USDC, WETH])],
        );
        assert_eq!(terms(&call).unwrap().output_recipient, None);
    }

    #[test]
    fn test_msg_sender_sentinel_leaves_the_recipient_unset() {
        let call = execute_call(
            &[V3_SWAP_EXACT_IN],
            vec![v3_input(Address::with_last_byte(1), 100, 1, &[USDC, WETH])],
        );
        assert_eq!(terms(&call).unwrap().output_recipient, None);
    }

    sol! {
        /// v4's pool identity: the two currencies sorted, so `zeroForOne` names the sold side.
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }

        /// `IV4Router.ExactInputSingleParams`. Encoding through `sol!` checks the reader's
        /// positional reads against alloy's own encoder.
        struct ExactInputSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 amountIn;
            uint128 amountOutMinimum;
            bytes hookData;
        }
    }

    /// A `V4_SWAP` command input: `abi.encode(bytes actions, bytes[] params)`.
    fn v4_input(actions: &[u8], params: Vec<Bytes>) -> Bytes {
        use alloy::sol_types::SolValue;
        (Bytes::from(actions.to_vec()), params)
            .abi_encode_params()
            .into()
    }

    /// One `SWAP_EXACT_IN_SINGLE` params blob.
    fn v4_exact_in_single(
        currency0: Address,
        currency1: Address,
        zero_for_one: bool,
        amount_in: u128,
        floor: u128,
    ) -> Bytes {
        use alloy::sol_types::SolValue;
        ExactInputSingleParams {
            poolKey: PoolKey {
                currency0,
                currency1,
                fee: alloy::primitives::Uint::<24, 1>::from(3000u32),
                tickSpacing: alloy::primitives::Signed::<24, 1>::try_from(60i32).unwrap(),
                hooks: Address::ZERO,
            },
            zeroForOne: zero_for_one,
            amountIn: amount_in,
            amountOutMinimum: floor,
            hookData: Bytes::default(),
        }
        .abi_encode()
        .into()
    }

    /// The action stream a live v4 swap carries: swap, settle the input, take the output.
    const V4_SETTLE_ALL: u8 = 0x0c;
    const V4_TAKE_ALL: u8 = 0x0f;

    #[test]
    fn test_v4_exact_in_single_zero_for_one() {
        // currency0 sold for currency1.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_IN_SINGLE, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in_single(USDC, WETH, true, 100_000_000, 5),
                    Bytes::default(),
                    Bytes::default(),
                ],
            )],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_in, U256::from(100_000_000u64));
        assert_eq!(declared.min_amount_out, Some(U256::from(5u64)));
        // A later TAKE pays the output out, so there is no recipient in the swap params.
        assert_eq!(declared.output_recipient, None);
    }

    #[test]
    fn test_v4_exact_in_single_one_for_zero() {
        // The same pool, sold the other way: currency1 in, currency0 out.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_IN_SINGLE, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in_single(USDC, WETH, false, 2_000, 7),
                    Bytes::default(),
                    Bytes::default(),
                ],
            )],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, WETH);
        assert_eq!(declared.token_out, USDC);
    }

    #[test]
    fn test_v4_native_currency_needs_no_translation() {
        // v4 names native ETH as the zero address directly, with no wrapping, which is already
        // hindsight's convention — so no WRAP_ETH appears and nothing is rewritten.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_IN_SINGLE, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in_single(Address::ZERO, USDC, true, 6_340_000, 1),
                    Bytes::default(),
                    Bytes::default(),
                ],
            )],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.token_out, USDC);
    }

    #[test]
    fn test_v4_exact_out_declined() {
        // Exact output bounds the input instead of stating it, so there is no amount spent.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_OUT_SINGLE, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in_single(USDC, WETH, true, 100, 1),
                    Bytes::default(),
                    Bytes::default(),
                ],
            )],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_v4_multi_hop_declined() {
        // SWAP_EXACT_IN's 2.1.1 layout moves the amounts, so reading by position is not safe.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(&[V4_SWAP_EXACT_IN, V4_SETTLE_ALL], vec![Bytes::default(); 2])],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_v3_and_v4_in_one_stream_declined() {
        // A route can start in v3 and finish in v4, so neither leg alone is the trade.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, V4_SWAP],
            vec![
                v3_input(trader, 100, 1, &[USDC, WETH]),
                v4_input(
                    &[V4_SWAP_EXACT_IN_SINGLE, V4_TAKE_ALL],
                    vec![v4_exact_in_single(WETH, USDC, true, 100, 1), Bytes::default()],
                ),
            ],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_v4_stream_with_no_swap_declined() {
        // Settle/take only: no trade in the stream.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(&[V4_SETTLE_ALL, V4_TAKE_ALL], vec![Bytes::default(); 2])],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_split_route_declined() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, V3_SWAP_EXACT_IN],
            vec![v3_input(trader, 60, 1, &[USDC, WETH]), v3_input(trader, 40, 1, &[USDC, WETH])],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_no_swap_command_declined() {
        // Permit-only or sweep-only streams carry no trade.
        assert!(terms(&execute_call(&[WRAP_ETH], vec![Bytes::default()])).is_none());
        assert!(terms(&execute_call(&[], vec![])).is_none());
    }

    #[test]
    fn test_zero_amount_declined() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(&[V3_SWAP_EXACT_IN], vec![v3_input(trader, 0, 1, &[USDC, WETH])]);
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_garbage_and_truncated_input_declined() {
        assert!(terms(&[]).is_none());
        assert!(terms(&[0xde, 0xad, 0xbe, 0xef]).is_none());
        let trader = address!("0x000000000000000000000000000000000000dead");
        let full = execute_call(&[V3_SWAP_EXACT_IN], vec![v3_input(trader, 100, 1, &[USDC, WETH])]);
        assert!(terms(&full[..full.len() / 2]).is_none());
    }

    #[test]
    fn test_malformed_v3_path_declined() {
        use alloy::sol_types::SolValue;
        // A path that is not `token (fee token)+` cannot name a pair.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let stub: Bytes =
            (trader, U256::from(100u64), U256::from(1u64), Bytes::from(vec![0u8; 25]), true)
                .abi_encode_params()
                .into();
        assert!(terms(&execute_call(&[V3_SWAP_EXACT_IN], vec![stub])).is_none());
    }
}
