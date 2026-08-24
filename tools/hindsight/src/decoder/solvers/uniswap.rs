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
//! params)`, with two exact-input actions. `SWAP_EXACT_IN_SINGLE` (`0x06`) holds a `PoolKey`, the
//! swap direction, the input amount and the floor; `SWAP_EXACT_IN` (`0x07`) holds the input
//! currency, a `PathKey[]` of hops, then the same two amounts. v4 names native ETH as the zero
//! address directly, with no wrapping, so its currencies need no translation. A single pool's two
//! currencies are sorted, so `zeroForOne` says which is being sold.
//!
//! Universal Router 2.1.1 inserts a `minHopPriceX36` field into both structs. In
//! `ExactInputSingleParams` it lands after every field read here. In `ExactInputParams` it lands
//! as an array *before* the amounts, moving both one word later — so the amounts are found by
//! locating the end of the struct's head rather than at fixed positions. See
//! `read_v4_multi_hop`.
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
//! - **A route that ends in the token it started from.** Two of the four sampled multi-hop v4 swaps
//!   were USDC to USDC. That is a bot cycling a pool, not a trade a re-solve can price.
//!
//! Verified on what remains: 10 v3/v2 trades, 16 v4 exact-in-single swaps, and the 4 multi-hop v4
//! swaps (two Universal Router v2, one 2.1.1, one carrying 97 bytes of hook data per hop). Every
//! decoded token pair matched the settled record, and every `amount_in` matched exactly except one
//! v3 trade reading 0.87% lower — a fee taken before the swap, so the calldata figure is the
//! amount that reached the pools, which is the basis a re-solve needs.

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

    /// The same entry without a deadline (selector `0x24856bc3`), which integrators also call.
    /// The command stream is identical, so both decode through one path.
    function execute(bytes commands, bytes[] inputs) external payable;
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

/// v4's own action bytes, inside a `V4_SWAP` command (`v4-periphery`'s `Actions` library). The two
/// exact-input swaps are read; the exact-output pair states no input amount, only a ceiling.
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

/// The blob position of element `index` of an array of dynamic structs, whose length word sits at
/// `array_offset`.
///
/// A struct element carries no length word — unlike a `bytes[]` element, which `array_element`
/// reads — so the offset lands on the struct's own first head word.
fn struct_element(input: &[u8], array_offset: usize, index: usize) -> Option<usize> {
    let data = array_offset + WORD;
    let relative = usize::try_from(word(input, data / WORD + index)?).ok()?;
    Some(data + relative)
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

/// The first head word of a params struct. A dynamic struct — every one read here ends in `bytes
/// hookData` — is encoded behind its own offset word when it is `abi.encode`d alone, and without
/// one when it is already the payload of a `bytes[]` element.
fn struct_base(params: &[u8]) -> Option<usize> {
    Some(usize::from(word(params, 0)? == U256::from(WORD)))
}

/// `IV4Router.ExactInputSingleParams` — one pool, exact input:
///
/// ```text
///   currency0  currency1  fee  tickSpacing  hooks  zeroForOne  amountIn  amountOutMinimum
/// ```
///
/// The pool's currencies are sorted, so `zeroForOne` says which one is being sold. Universal
/// Router 2.1.1 appends `minHopPriceX36` after `amountOutMinimum`, past every read here.
///
/// There is no recipient: a later `TAKE`/`TAKE_ALL` action pays the output out, so the caller
/// reads the transaction sender's receipt.
fn read_v4_single(params: &[u8]) -> Option<Swap> {
    let base = struct_base(params)?;
    let currency0 = address_at(params, base)?;
    let currency1 = address_at(params, base + 1)?;
    let zero_for_one = !word(params, base + 5)?.is_zero();
    let (token_in, token_out) =
        if zero_for_one { (currency0, currency1) } else { (currency1, currency0) };
    Some(Swap {
        token_in,
        token_out,
        amount_in: word(params, base + 6)?,
        min_amount_out: word(params, base + 7)?,
        recipient: None,
    })
}

/// The head length of `ExactInputParams` in the two deployed layouts: four words on Universal
/// Router v2, five on 2.1.1, which inserts a `minHopPricesX36` array offset.
const V4_MULTI_HOP_HEAD_WORDS: std::ops::RangeInclusive<usize> = 4..=5;

/// `IV4Router.ExactInputParams` — a path of hops, exact input:
///
/// ```text
///   currencyIn  path[]  amountIn  amountOutMinimum
/// ```
///
/// Universal Router 2.1.1 inserts a `minHopPricesX36` array between `path` and the amounts, so
/// both amounts sit one word later there. Rather than branch on the router address, this finds
/// where the struct's head ends: `path[]`'s offset is measured from the struct's start, so it *is*
/// the head length, and the two amounts are always the head's last two words.
///
/// `token_out` is the last `PathKey`'s `intermediateCurrency`, that struct's own first field in
/// both layouts. `PathKey` ends in `bytes hookData`, so each hop sits behind its own offset and a
/// hop carrying hook data does not move the hops after it.
fn read_v4_multi_hop(params: &[u8]) -> Option<Swap> {
    let base = struct_base(params)?;
    let token_in = address_at(params, base)?;
    let path_offset = usize::try_from(word(params, base + 1)?).ok()?;
    let head_words = path_offset / WORD;
    if !path_offset.is_multiple_of(WORD) || !V4_MULTI_HOP_HEAD_WORDS.contains(&head_words) {
        return None;
    }
    let amount_in = word(params, base + head_words - 2)?;
    let min_amount_out = word(params, base + head_words - 1)?;
    // Both amounts are `uint128`. A word too large to be one is an offset or an address, so the
    // head does not end where `path[]`'s offset says it does.
    if amount_in > U256::from(u128::MAX) || min_amount_out > U256::from(u128::MAX) {
        return None;
    }
    let path = base * WORD + path_offset;
    let hops = usize::try_from(word(params, path / WORD)?).ok()?;
    // A path with more hops than the blob has words is malformed, and the bound keeps the element
    // index below the width `struct_element` adds it at.
    if hops == 0 || hops > params.len() / WORD {
        return None;
    }
    let last = struct_element(params, path, hops - 1)?;
    if !last.is_multiple_of(WORD) {
        return None;
    }
    Some(Swap {
        token_in,
        token_out: address_at(params, last / WORD)?,
        amount_in,
        min_amount_out,
        recipient: None,
    })
}

/// The one exact-input swap in a `V4_SWAP` command's nested action stream, or `None` when it
/// carries none, several, or a shape this does not read.
///
/// The command's input is `abi.encode(bytes actions, bytes[] params)`: action `index` reads
/// element `index` of `params`.
fn read_v4_swap(command_input: &[u8]) -> Option<Swap> {
    let actions = dynamic_at(command_input, 0)?;
    let params_offset = usize::try_from(word(command_input, 1)?).ok()?;
    let count = usize::try_from(word(command_input, params_offset / WORD)?).ok()?;
    let mut found = None;
    for (index, action) in actions.iter().enumerate() {
        let read: fn(&[u8]) -> Option<Swap> = match *action {
            V4_SWAP_EXACT_IN_SINGLE => read_v4_single,
            V4_SWAP_EXACT_IN => read_v4_multi_hop,
            // Exact output states no input amount, only a ceiling, so there is nothing to record
            // as the amount spent.
            V4_SWAP_EXACT_OUT_SINGLE | V4_SWAP_EXACT_OUT => return None,
            _ => continue,
        };
        if found.is_some() || index >= count {
            return None;
        }
        found = Some(read(array_element(command_input, params_offset, index)?)?);
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

/// The command stream of either `execute` overload: with a deadline or without. The two carry
/// the same commands and inputs, so the rest of the read does not care which was called.
fn command_stream(
    input: &[u8],
) -> Option<(alloy::primitives::Bytes, Vec<alloy::primitives::Bytes>)> {
    if let Ok(call) = execute_0Call::abi_decode(input) {
        return Some((call.commands, call.inputs));
    }
    let call = execute_1Call::abi_decode(input).ok()?;
    Some((call.commands, call.inputs))
}

/// The Uniswap Universal Router solver.
pub(crate) struct Uniswap;

impl SolverDecoder for Uniswap {
    /// The trader's swap terms from the one exact-input swap in an `execute` command stream,
    /// whether it names a v2, v3 or v4 pool.
    ///
    /// Declines a stream carrying more than one swap, an exact-output swap, or a path that ends in
    /// the token it started from — see the module docs for why none of the three can be priced.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let Some((commands, inputs)) = command_stream(input) else { return Ok(None) };
        let mut swap = None;
        let mut wrapped = false;
        let mut unwrapped = false;
        for (command, command_input) in commands.iter().zip(inputs.iter()) {
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
        // A path that ends in the token it started from is a bot cycling pools, not a trade a
        // re-solve can price.
        if token_in == token_out {
            return Ok(None);
        }
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
        execute_0Call { commands: commands.to_vec().into(), inputs, deadline: U256::from(1_u64) }
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
    fn test_selectors_against_the_deployed_router() {
        // Both overloads observed in live traffic: with a deadline and without.
        assert_eq!(execute_0Call::SELECTOR, [0x35, 0x93, 0x56, 0x4c]);
        assert_eq!(execute_1Call::SELECTOR, [0x24, 0x85, 0x6b, 0xc3]);
    }

    #[test]
    fn test_deadline_free_overload_reads_the_same_stream() {
        use alloy::sol_types::SolCall;
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_1Call {
            commands: vec![V3_SWAP_EXACT_IN].into(),
            inputs: vec![v3_input(trader, 100_000_000, 5, &[USDC, WETH])],
        }
        .abi_encode();
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_in, U256::from(100_000_000u64));
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

        /// `PathKey` — one hop of a multi-hop route. It ends in `bytes hookData`, so a `PathKey[]`
        /// element sits behind its own offset.
        struct PathKey {
            address intermediateCurrency;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
            bytes hookData;
        }

        /// `IV4Router.ExactInputParams` as Universal Router v2 encodes it: a four-word head.
        struct ExactInputParams {
            address currencyIn;
            PathKey[] path;
            uint128 amountIn;
            uint128 amountOutMinimum;
        }

        /// The same params as Universal Router 2.1.1 encodes them. A per-hop `minHopPricesX36`
        /// array lands between the path and the amounts, making a five-word head and moving both
        /// amounts one word later.
        struct ExactInputParamsV211 {
            address currencyIn;
            PathKey[] path;
            uint256[] minHopPricesX36;
            uint128 amountIn;
            uint128 amountOutMinimum;
        }
    }

    /// One hop of a multi-hop route, at the 3000 fee tier.
    fn path_key(token: Address, hook_data: Bytes) -> PathKey {
        PathKey {
            intermediateCurrency: token,
            fee: alloy::primitives::Uint::<24, 1>::from(3000u32),
            tickSpacing: alloy::primitives::Signed::<24, 1>::try_from(60i32).unwrap(),
            hooks: Address::ZERO,
            hookData: hook_data,
        }
    }

    /// One `SWAP_EXACT_IN` params blob in Universal Router v2's layout.
    fn v4_exact_in(currency_in: Address, hops: &[Address], amount_in: u128, floor: u128) -> Bytes {
        use alloy::sol_types::SolValue;
        ExactInputParams {
            currencyIn: currency_in,
            path: hops
                .iter()
                .map(|token| path_key(*token, Bytes::default()))
                .collect(),
            amountIn: amount_in,
            amountOutMinimum: floor,
        }
        .abi_encode()
        .into()
    }

    /// The same blob in Universal Router 2.1.1's layout.
    fn v4_exact_in_211(
        currency_in: Address,
        hops: &[Address],
        amount_in: u128,
        floor: u128,
    ) -> Bytes {
        use alloy::sol_types::SolValue;
        ExactInputParamsV211 {
            currencyIn: currency_in,
            path: hops
                .iter()
                .map(|token| path_key(*token, Bytes::default()))
                .collect(),
            minHopPricesX36: vec![U256::from(1u64); hops.len()],
            amountIn: amount_in,
            amountOutMinimum: floor,
        }
        .abi_encode()
        .into()
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
    fn test_v4_multi_hop_reads_the_path_ends() {
        let mid = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_IN, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in(USDC, &[mid, WETH], 100_000_000, 5),
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
    fn test_v4_multi_hop_211_layout_reads_the_same_amounts() {
        // The same route, encoded with `minHopPricesX36` before the amounts. Reading the head's
        // last two words finds them where a fixed position would read the path offset instead.
        let mid = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_IN, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in_211(USDC, &[mid, WETH], 100_000_000, 5),
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
    }

    /// A live Universal Router transaction's full calldata.
    fn fixture(text: &str) -> Vec<u8> {
        let text = text.trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    #[test]
    fn test_v4_multi_hop_live_universal_router_v2() {
        // Two hops, each carrying 97 bytes of hook data, so the hops sit at unequal offsets and
        // the second cannot be found by striding from the first.
        let declared =
            terms(&fixture(include_str!("fixtures/uniswap_v4_multi_hop_input.txt"))).unwrap();
        assert_eq!(declared.token_in, address!("0x19640000000ba88d36206beb10d0e86011c8d08c"));
        assert_eq!(declared.token_out, address!("0x1223334444a7466fbf985b14e1f4edaf3883bca6"));
        assert_eq!(declared.amount_in, U256::from(27_636_981_441_190_000_621_809u128));
        assert_eq!(declared.min_amount_out, Some(U256::from(36_449_320_026_157_160_385_760u128)));
    }

    #[test]
    fn test_v4_multi_hop_live_universal_router_211() {
        // The 2.1.1 layout, live: native ETH in, one hop, a `minHopPricesX36` array before the
        // amounts.
        let declared =
            terms(&fixture(include_str!("fixtures/uniswap_v4_multi_hop_211_input.txt"))).unwrap();
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.token_out, address!("0x651e5ea84e2c8ef30ddbf62d716fb2bf37535ffe"));
        assert_eq!(declared.amount_in, U256::from(1_000_000_000_000_000u64));
        assert_eq!(declared.min_amount_out, Some(U256::from(90_445_718_574_559_218_595u128)));
    }

    #[test]
    fn test_round_trip_path_declined() {
        // USDC to USDC: two of the four sampled live multi-hop v4 swaps were this. A bot cycling
        // a pool, not a trade a re-solve can price.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_IN, V4_SETTLE_ALL],
                vec![v4_exact_in(USDC, &[WETH, USDC], 1_000, 1), Bytes::default()],
            )],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_v4_multi_hop_without_hops_declined() {
        // An empty path names no output token.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(&[V4_SWAP_EXACT_IN], vec![v4_exact_in(USDC, &[], 1_000, 1)])],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_v4_multi_hop_malformed_params_declined() {
        // An empty params blob has no head to read.
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
