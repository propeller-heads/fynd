//! Uniswap Universal Router calldata extraction.
//!
//! `execute(bytes commands, bytes[] inputs, uint256 deadline)` is a command stream: each byte of
//! `commands` names an operation and reads its parameters from the matching element of `inputs`
//! (docs: developers.uniswap.org/docs/protocols/universal-router/concepts/commands). Four commands
//! carry the trader's terms:
//!
//! - `V3_SWAP_EXACT_IN` (`0x00`) — recipient, amount in, floor, then a packed v3 path (20-byte
//!   token, 3-byte fee, repeating).
//! - `V3_SWAP_EXACT_OUT` (`0x01`) — recipient, amount out, ceiling, then the same path encoded
//!   output-first.
//! - `V2_SWAP_EXACT_IN` (`0x08`) and `V2_SWAP_EXACT_OUT` (`0x09`) — the same three, then an
//!   `address[]` path, which always runs input to output.
//!
//! The parameters are read by word position rather than as a fixed tuple: Universal Router 2.1.1
//! appends a `minHopPriceX36` array that a strict decode would reject, and the leading words have
//! not moved.
//!
//! An exact-output command states the settled output outright and only bounds the input, so the
//! caller recovers what was spent from the payer's net payment. That is the mirror of the
//! exact-input case, where the input is stated and the output is recovered from the recipient's
//! receipt.
//!
//! `WRAP_ETH` and `UNWRAP_WETH` bracket a swap whose path names WETH but whose trader side is
//! native ETH, so they rewrite the corresponding token to `Address::ZERO`. See `trader_tokens`.
//!
//! `V4_SWAP` (`0x10`) carries a nested stream of its own: `abi.encode(bytes actions, bytes[]
//! params)`, with four swap actions. `SWAP_EXACT_IN_SINGLE` (`0x06`) and `SWAP_EXACT_OUT_SINGLE`
//! (`0x08`) hold a `PoolKey`, the swap direction, and the two amounts; `SWAP_EXACT_IN` (`0x07`)
//! and `SWAP_EXACT_OUT` (`0x09`) hold one named currency, a `PathKey[]` of hops, then the same
//! two amounts. v4 names native ETH as the zero address directly, with no wrapping, so its
//! currencies need no translation. A single pool's two currencies are sorted, so `zeroForOne` says
//! which is being sold.
//!
//! Universal Router 2.1.1 inserts a `minHopPriceX36` field into all four structs. In the
//! single-pool pair it lands after every field read here. In the multi-hop pair it lands as an
//! array *before* the amounts, moving both one word later — so the amounts are found by locating
//! the end of the struct's head rather than at fixed positions. See `read_v4_multi_hop`.
//!
//! What is declined, measured over live traffic (40 Universal Router trades on Ethereum blocks
//! 25741800-25741815, carrying 28 v4 swaps between them):
//!
//! - **More than one swap in the stream**, counting v2, v3 and v4 together. A split route has no
//!   single command that is the trade, and a route can begin in v3 and finish in v4 — one sampled
//!   trade did, where reading only its v3 leg reported the wrong `token_out`.
//! - **A route that ends in the token it started from.** Two of the four sampled multi-hop v4 swaps
//!   were USDC to USDC. That is a bot cycling a pool, not a trade a re-solve can price.
//! - **A swap whose input the calldata cannot name**: an `UNWRAP_WETH` before the swap means the
//!   trader paid wrapped native and the pool wants native, so the params name `Address::ZERO` where
//!   the trader's own token was the wrapped one. Every exact-output v4 swap in the sample was this
//!   shape, as `PERMIT2_TRANSFER_FROM UNWRAP_WETH V4_SWAP WRAP_ETH`.
//!
//! Verified on the sample: 10 v3/v2 trades, 16 v4 exact-in-single swaps, and the 4 multi-hop v4
//! swaps (two Universal Router v2, one 2.1.1, one carrying 97 bytes of hook data per hop). Every
//! decoded token pair matched the settled record, and every `amount_in` matched exactly except one
//! v3 trade reading 0.87% lower — a fee taken before the swap, so the calldata figure is the
//! amount that reached the pools, which is the basis a re-solve needs.
//!
//! Exact output was measured over a second range, Ethereum blocks 25826759-25826908: 26 records
//! moved from netted to declared, 23 of them on amounts identical to netting's, and none was lost.

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
const SWEEP: u8 = 0x04;
const WRAP_ETH: u8 = 0x0b;
const UNWRAP_WETH: u8 = 0x0c;
const V4_SWAP: u8 = 0x10;

/// v4's own action bytes, inside a `V4_SWAP` command (`v4-periphery`'s `Actions` library). All
/// four swap actions are read; the rest move value without stating terms.
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

/// Which side of the trade a swap command's calldata fixes. The other side is only bounded, and
/// the two swap the meaning of a params struct's named currency and of its two amounts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    ExactIn,
    ExactOut,
}

/// The two amounts a swap command states: the side it fixes, and the bound it enforces on the
/// other side.
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

/// One swap command's terms.
struct Swap {
    token_in: Address,
    token_out: Address,
    amounts: Amounts,
    /// The command's declared recipient, unless it is a sentinel.
    recipient: Option<Address>,
}

/// A reader of one v4 swap action's params blob: single-pool or multi-hop, either side fixed.
type ReadV4Params = fn(&[u8], Side) -> Option<Swap>;

/// The command that pays the trade's output out to the trader, when the swap sent it to the router
/// instead of naming an address.
///
/// A swap that pays the router names `ADDRESS_THIS` and enforces **no floor at all** — the floor
/// moves to this command, along with the trader's address. Reading it matters twice over: without
/// the address, `recover_output` falls back to the transaction sender, which for an ERC-4337
/// bundle is the bundler; and without the floor there is nothing to reject the bundler's gas
/// refund with.
#[derive(Clone, Copy)]
struct Payout {
    /// The token paid out. `Address::ZERO` for `UNWRAP_WETH`, which always pays native ETH.
    token: Address,
    recipient: Address,
    min_amount_out: U256,
}

/// `UNWRAP_WETH(address recipient, uint256 amountMinimum)`.
fn read_unwrap(input: &[u8]) -> Option<Payout> {
    Some(Payout {
        token: Address::ZERO,
        recipient: readable_recipient(address_at(input, 0)?)?,
        min_amount_out: word(input, 1)?,
    })
}

/// `SWEEP(address token, address recipient, uint256 amountMinimum)`.
fn read_sweep(input: &[u8]) -> Option<Payout> {
    Some(Payout {
        token: address_at(input, 0)?,
        recipient: readable_recipient(address_at(input, 1)?)?,
        min_amount_out: word(input, 2)?,
    })
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

/// `IV4Router.ExactInputSingleParams` and `ExactOutputSingleParams` — one pool, both amounts in
/// the same two positions:
///
/// ```text
///   currency0  currency1  fee  tickSpacing  hooks  zeroForOne  fixed  bound
/// ```
///
/// The pool's currencies are sorted, so `zeroForOne` says which one is being sold, whichever side
/// is fixed. Universal Router 2.1.1 appends `minHopPriceX36` after the bound, past every read
/// here.
///
/// There is no recipient: a later `TAKE`/`TAKE_ALL` action pays the output out, so the caller
/// reads the transaction sender's receipt.
fn read_v4_single(params: &[u8], side: Side) -> Option<Swap> {
    let base = struct_base(params)?;
    let currency0 = address_at(params, base)?;
    let currency1 = address_at(params, base + 1)?;
    let zero_for_one = !word(params, base + 5)?.is_zero();
    let (token_in, token_out) =
        if zero_for_one { (currency0, currency1) } else { (currency1, currency0) };
    let fixed = word(params, base + 6)?;
    let bound = word(params, base + 7)?;
    Some(Swap {
        token_in,
        token_out,
        amounts: match side {
            Side::ExactIn => Amounts::ExactIn { amount_in: fixed, min_amount_out: bound },
            Side::ExactOut => Amounts::ExactOut { amount_out: fixed, max_amount_in: bound },
        },
        recipient: None,
    })
}

/// The head length of `ExactInputParams` and `ExactOutputParams` in the two deployed layouts: four
/// words on Universal Router v2, five on 2.1.1, which inserts a `minHopPricesX36` array offset.
const V4_MULTI_HOP_HEAD_WORDS: std::ops::RangeInclusive<usize> = 4..=5;

/// `IV4Router.ExactInputParams` and `ExactOutputParams` — a path of hops:
///
/// ```text
///   currencyIn   path[]  amountIn   amountOutMinimum
///   currencyOut  path[]  amountOut  amountInMaximum
/// ```
///
/// Universal Router 2.1.1 inserts a `minHopPricesX36` array between `path` and the amounts, so
/// both amounts sit one word later there. Rather than branch on the router address, this finds
/// where the struct's head ends: `path[]`'s offset is measured from the struct's start, so it *is*
/// the head length, and the two amounts are always the head's last two words.
///
/// The named currency is the fixed side, and the far end of the path is the other side: the last
/// hop's `intermediateCurrency` for exact input, the first hop's for exact output, since
/// `_swapExactOutput` walks the path backwards from `currencyOut`. `intermediateCurrency` is
/// `PathKey`'s own first field in both layouts, and `PathKey` ends in `bytes hookData`, so each
/// hop sits behind its own offset and a hop carrying hook data does not move the hops after it.
fn read_v4_multi_hop(params: &[u8], side: Side) -> Option<Swap> {
    let base = struct_base(params)?;
    let named = address_at(params, base)?;
    let path_offset = usize::try_from(word(params, base + 1)?).ok()?;
    let head_words = path_offset / WORD;
    if !path_offset.is_multiple_of(WORD) || !V4_MULTI_HOP_HEAD_WORDS.contains(&head_words) {
        return None;
    }
    let fixed = word(params, base + head_words - 2)?;
    let bound = word(params, base + head_words - 1)?;
    // Both amounts are `uint128`. A word too large to be one is an offset or an address, so the
    // head does not end where `path[]`'s offset says it does.
    if fixed > U256::from(u128::MAX) || bound > U256::from(u128::MAX) {
        return None;
    }
    let path = base * WORD + path_offset;
    let hops = usize::try_from(word(params, path / WORD)?).ok()?;
    // A path with more hops than the blob has words is malformed, and the bound keeps the element
    // index below the width `struct_element` adds it at.
    if hops == 0 || hops > params.len() / WORD {
        return None;
    }
    let far = struct_element(params, path, if side == Side::ExactIn { hops - 1 } else { 0 })?;
    if !far.is_multiple_of(WORD) {
        return None;
    }
    let far = address_at(params, far / WORD)?;
    Some(match side {
        Side::ExactIn => Swap {
            token_in: named,
            token_out: far,
            amounts: Amounts::ExactIn { amount_in: fixed, min_amount_out: bound },
            recipient: None,
        },
        Side::ExactOut => Swap {
            token_in: far,
            token_out: named,
            amounts: Amounts::ExactOut { amount_out: fixed, max_amount_in: bound },
            recipient: None,
        },
    })
}

/// The one swap in a `V4_SWAP` command's nested action stream, or `None` when it carries none,
/// several, or a shape this does not read.
///
/// The command's input is `abi.encode(bytes actions, bytes[] params)`: action `index` reads
/// element `index` of `params`.
fn read_v4_swap(command_input: &[u8]) -> Option<Swap> {
    let actions = dynamic_at(command_input, 0)?;
    let params_offset = usize::try_from(word(command_input, 1)?).ok()?;
    let count = usize::try_from(word(command_input, params_offset / WORD)?).ok()?;
    let mut found = None;
    for (index, action) in actions.iter().enumerate() {
        let (read, side): (ReadV4Params, Side) = match *action {
            V4_SWAP_EXACT_IN_SINGLE => (read_v4_single, Side::ExactIn),
            V4_SWAP_EXACT_IN => (read_v4_multi_hop, Side::ExactIn),
            V4_SWAP_EXACT_OUT_SINGLE => (read_v4_single, Side::ExactOut),
            V4_SWAP_EXACT_OUT => (read_v4_multi_hop, Side::ExactOut),
            _ => continue,
        };
        if found.is_some() || index >= count {
            return None;
        }
        found = Some(read(array_element(command_input, params_offset, index)?, side)?);
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

/// One v3 or v2 swap command's terms, by command type. All four share the same leading three
/// parameters — recipient, the fixed amount, the bound — and differ in how the path is encoded.
///
/// Uniswap encodes an exact-output v3 path output-first, so its ends are read the other way round.
/// A v2 path always runs input to output.
fn read_swap(command: u8, input: &[u8]) -> Option<Swap> {
    let recipient = readable_recipient(address_at(input, 0)?);
    let fixed = word(input, 1)?;
    let bound = word(input, 2)?;
    let (first, last) = match command {
        V3_SWAP_EXACT_IN | V3_SWAP_EXACT_OUT => v3_path_ends(dynamic_at(input, 3)?)?,
        _ => v2_path_ends(input, 3)?,
    };
    Some(match command {
        V3_SWAP_EXACT_OUT => Swap {
            token_in: last,
            token_out: first,
            amounts: Amounts::ExactOut { amount_out: fixed, max_amount_in: bound },
            recipient,
        },
        V2_SWAP_EXACT_OUT => Swap {
            token_in: first,
            token_out: last,
            amounts: Amounts::ExactOut { amount_out: fixed, max_amount_in: bound },
            recipient,
        },
        _ => Swap {
            token_in: first,
            token_out: last,
            amounts: Amounts::ExactIn { amount_in: fixed, min_amount_out: bound },
            recipient,
        },
    })
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

/// The trader's own two tokens, once the wrap commands bracketing the swap are accounted for: a
/// path names WETH where the trader's side is native ETH.
///
/// Where the command sits relative to the swap is what makes it readable, because both commands
/// appear on either side of a trade:
///
/// - A `WRAP_ETH` **before** the swap wraps what the trader sent, so the input is native ETH. One
///   after the swap re-wraps change and says nothing about the trader's tokens — seen live as
///   `PERMIT2_TRANSFER_FROM UNWRAP_WETH V4_SWAP WRAP_ETH`, where the trader pays WETH, the router
///   unwraps it for a native-ETH v4 pool, and re-wraps the remainder to return it.
/// - An `UNWRAP_WETH` **after** the swap pays a native output. One before the swap is feeding the
///   pool, as in that same stream.
///
/// A native input wins over a later unwrap: an exact-output swap is funded up to its ceiling and
/// sweeps the remainder back, so it carries both, and that unwrap returns the input.
fn trader_tokens(swap: &Swap, wrapped_before: bool, unwrapped_after: bool) -> (Address, Address) {
    if wrapped_before {
        return (Address::ZERO, swap.token_out);
    }
    if unwrapped_after {
        return (swap.token_in, Address::ZERO);
    }
    (swap.token_in, swap.token_out)
}

/// Everything one `execute` command stream says about the trade.
struct Stream {
    swap: Swap,
    /// A `WRAP_ETH` before the swap: the trader paid native ETH.
    wrapped_before: bool,
    /// An `UNWRAP_WETH` before the swap: the trader paid wrapped native into a native pool.
    unwrapped_before: bool,
    /// An `UNWRAP_WETH` after the swap: the trader is paid native ETH.
    unwrapped_after: bool,
    /// The first command after the swap that pays the output out.
    payout: Option<Payout>,
}

/// Walk an `execute` command stream for its one swap and the commands around it.
///
/// `None` when the stream carries no swap, or more than one: a route split across commands has no
/// single one that is the trade, whichever pool versions they name.
fn read_stream(commands: &[u8], inputs: &[alloy::primitives::Bytes]) -> Option<Stream> {
    let mut swap = None;
    let mut wrapped_before = false;
    let mut unwrapped_before = false;
    let mut unwrapped_after = false;
    let mut payout = None;
    for (command, command_input) in commands.iter().zip(inputs.iter()) {
        let read = match command & COMMAND_TYPE_MASK {
            WRAP_ETH => {
                wrapped_before |= swap.is_none();
                continue;
            }
            UNWRAP_WETH => {
                if swap.is_some() {
                    unwrapped_after = true;
                    payout = payout.or_else(|| read_unwrap(command_input));
                } else {
                    unwrapped_before = true;
                }
                continue;
            }
            SWEEP => {
                if swap.is_some() {
                    payout = payout.or_else(|| read_sweep(command_input));
                }
                continue;
            }
            V4_SWAP => read_v4_swap(command_input),
            command @ (V3_SWAP_EXACT_IN | V2_SWAP_EXACT_IN | V3_SWAP_EXACT_OUT |
            V2_SWAP_EXACT_OUT) => read_swap(command, command_input),
            _ => continue,
        };
        if swap.is_some() {
            return None;
        }
        swap = Some(read?);
    }
    Some(Stream { swap: swap?, wrapped_before, unwrapped_before, unwrapped_after, payout })
}

impl SolverDecoder for Uniswap {
    /// The trader's swap terms from the one swap in an `execute` command stream, whether it names a
    /// v2, v3 or v4 pool and whether it fixes the input or the output.
    ///
    /// An exact-output swap states the settled output and only a ceiling on the input, so the
    /// caller recovers what was spent from the payer's net payment.
    ///
    /// A swap that pays the router rather than the trader names neither a recipient nor a floor, so
    /// both are taken from the `UNWRAP_WETH` or `SWEEP` that pays the output out.
    ///
    /// Declines a stream carrying more than one swap, or a path that ends in the token it started
    /// from — see the module docs for why neither can be priced.
    fn declared(&self, input: &[u8], _logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let Some((commands, inputs)) = command_stream(input) else { return Ok(None) };
        let Some(stream) = read_stream(&commands, &inputs) else { return Ok(None) };
        let swap = stream.swap;
        if swap.amounts.is_zero() {
            return Ok(None);
        }
        // An `UNWRAP_WETH` before the swap turns the trader's wrapped native into the native ETH
        // the pool wants, so the swap names native where the trader paid the wrapped token. Naming
        // that token needs the chain's wrapped-native address, which this reader does not have, so
        // the transaction goes to netting instead of guessing.
        if stream.unwrapped_before && !stream.wrapped_before {
            return Ok(None);
        }
        let (token_in, token_out) =
            trader_tokens(&swap, stream.wrapped_before, stream.unwrapped_after);
        // A path that ends in the token it started from is a bot cycling pools, not a trade a
        // re-solve can price.
        if token_in == token_out {
            return Ok(None);
        }
        // Only a payout of this trade's own output token says anything about it.
        let payout = stream
            .payout
            .filter(|payout| payout.token == token_out);
        let declared = match swap.amounts {
            Amounts::ExactIn { amount_in, min_amount_out } => DeclaredSwap::from_calldata(
                token_in,
                token_out,
                amount_in,
                // The stricter of the two floors: a swap paying the router leaves its own at zero.
                min_amount_out.max(payout.map_or(U256::ZERO, |payout| payout.min_amount_out)),
            ),
            Amounts::ExactOut { amount_out, max_amount_in } => {
                DeclaredSwap::from_calldata_exact_out(
                    token_in,
                    token_out,
                    amount_out,
                    max_amount_in,
                )
            }
        };
        let recipient = swap
            .recipient
            .or_else(|| payout.map(|payout| payout.recipient));
        Ok(Some(match recipient {
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
        assert_eq!(declared.amount_in, Some(U256::from(100_000_000u64)));
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
        assert_eq!(declared.amount_in, Some(U256::from(100_000_000u64)));
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
        assert_eq!(declared.amount_in, Some(U256::from(2_000u64)));
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

    /// An `UNWRAP_WETH` input: recipient, then the floor it enforces on the payout.
    fn unwrap_input(recipient: Address, amount_min: u64) -> Bytes {
        use alloy::sol_types::SolValue;
        (recipient, U256::from(amount_min))
            .abi_encode_params()
            .into()
    }

    /// A `SWEEP` input: token, recipient, then the floor.
    fn sweep_input(token: Address, recipient: Address, amount_min: u64) -> Bytes {
        use alloy::sol_types::SolValue;
        (token, recipient, U256::from(amount_min))
            .abi_encode_params()
            .into()
    }

    #[test]
    fn test_payout_names_the_trader_the_swap_left_as_a_sentinel() {
        // The live shape that produced five records claiming a bundler's gas refund as the settled
        // output: `V3_SWAP_EXACT_IN(ADDRESS_THIS, floor 0) UNWRAP_WETH(trader, real floor) SWEEP`.
        // The floor and the address both live on the payout command.
        let trader = address!("0x541a02f8685db041ba872bc5c0bb336377b8be35");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, UNWRAP_WETH, SWEEP],
            vec![
                v3_input(Address::with_last_byte(2), 1_736_236_160, 0, &[USDC, WETH]),
                unwrap_input(trader, 693_216_921),
                sweep_input(Address::ZERO, trader, 0),
            ],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, Address::ZERO);
        assert_eq!(declared.output_recipient, Some(trader));
        // Without this the recovered output is whatever the transaction sender happened to
        // receive, with a zero floor to reject it.
        assert_eq!(declared.min_amount_out, Some(U256::from(693_216_921u64)));
    }

    #[test]
    fn test_sweep_names_a_token_payout() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, SWEEP],
            vec![
                v3_input(Address::with_last_byte(2), 1_000, 0, &[WETH, USDC]),
                sweep_input(USDC, trader, 990),
            ],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.output_recipient, Some(trader));
        assert_eq!(declared.min_amount_out, Some(U256::from(990u64)));
    }

    #[test]
    fn test_payout_of_another_token_is_ignored() {
        // A sweep of leftover input, not of the trade's output.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, SWEEP],
            vec![
                v3_input(Address::with_last_byte(2), 1_000, 0, &[WETH, USDC]),
                sweep_input(WETH, trader, 5),
            ],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.output_recipient, None);
        assert_eq!(declared.min_amount_out, Some(U256::ZERO));
    }

    #[test]
    fn test_the_swaps_own_recipient_and_floor_win() {
        // A swap that names the trader directly does not need the payout command, and the stricter
        // of the two floors is kept.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let other = address!("0x00000000000000000000000000000000000000ff");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, SWEEP],
            vec![v3_input(trader, 1_000, 995, &[WETH, USDC]), sweep_input(USDC, other, 5)],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.output_recipient, Some(trader));
        assert_eq!(declared.min_amount_out, Some(U256::from(995u64)));
    }

    #[test]
    fn test_payout_before_the_swap_is_not_the_output() {
        // A sweep that runs before the swap is clearing a previous balance.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[SWEEP, V3_SWAP_EXACT_IN],
            vec![
                sweep_input(USDC, trader, 900),
                v3_input(Address::with_last_byte(2), 1_000, 0, &[WETH, USDC]),
            ],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.output_recipient, None);
        assert_eq!(declared.min_amount_out, Some(U256::ZERO));
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

        /// `IV4Router.ExactOutputParams` — the mirror of `ExactInputParams`: the struct names the
        /// output currency and the path runs backwards from it.
        struct ExactOutputParams {
            address currencyOut;
            PathKey[] path;
            uint128 amountOut;
            uint128 amountInMaximum;
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

    /// One `SWAP_EXACT_OUT` params blob. `currency_out` is the struct's own currency and the path
    /// runs backwards from it, so `hops[0]` names the input token.
    fn v4_exact_out(
        currency_out: Address,
        hops: &[Address],
        amount_out: u128,
        ceiling: u128,
    ) -> Bytes {
        use alloy::sol_types::SolValue;
        ExactOutputParams {
            currencyOut: currency_out,
            path: hops
                .iter()
                .map(|token| path_key(*token, Bytes::default()))
                .collect(),
            amountOut: amount_out,
            amountInMaximum: ceiling,
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
        assert_eq!(declared.amount_in, Some(U256::from(100_000_000u64)));
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
    fn test_v4_exact_out_single_states_the_output_and_bounds_the_input() {
        // `ExactOutputSingleParams` puts the two amounts where the exact-input struct puts them,
        // so the same read serves both — with their meanings swapped.
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_OUT_SINGLE, V4_SETTLE_ALL, V4_TAKE_ALL],
                vec![
                    v4_exact_in_single(USDC, WETH, true, 500, 1_000),
                    Bytes::default(),
                    Bytes::default(),
                ],
            )],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_out, Some(U256::from(500u64)));
        assert_eq!(declared.max_amount_in, Some(U256::from(1_000u64)));
        // The amount spent is not in the calldata: the caller recovers it from what was paid.
        assert_eq!(declared.amount_in, None);
        assert_eq!(declared.min_amount_out, None);
    }

    #[test]
    fn test_v4_exact_out_multi_hop_reads_the_path_backwards() {
        // `_swapExactOutput` walks the path backwards from `currencyOut`, so `path[0]` names the
        // input and the struct's own currency is the output.
        let mid = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
        let call = execute_call(
            &[V4_SWAP],
            vec![v4_input(
                &[V4_SWAP_EXACT_OUT, V4_SETTLE_ALL],
                vec![v4_exact_out(WETH, &[USDC, mid], 500, 1_000), Bytes::default()],
            )],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_out, Some(U256::from(500u64)));
        assert_eq!(declared.max_amount_in, Some(U256::from(1_000u64)));
        assert_eq!(declared.amount_in, None);
    }

    #[test]
    fn test_v3_exact_out_reads_the_reversed_path() {
        // Uniswap encodes an exact-output v3 path output-first, so the path's first token is the
        // one the trader receives.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call =
            execute_call(&[V3_SWAP_EXACT_OUT], vec![v3_input(trader, 500, 1_000, &[WETH, USDC])]);
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_out, Some(U256::from(500u64)));
        assert_eq!(declared.max_amount_in, Some(U256::from(1_000u64)));
        assert_eq!(declared.output_recipient, Some(trader));
    }

    #[test]
    fn test_v2_exact_out_path_runs_input_to_output() {
        // A v2 path is never reversed, whichever side is fixed.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call =
            execute_call(&[V2_SWAP_EXACT_OUT], vec![v2_input(trader, 500, 1_000, &[USDC, WETH])]);
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, WETH);
        assert_eq!(declared.amount_out, Some(U256::from(500u64)));
    }

    #[test]
    fn test_exact_out_wrap_and_unwrap_keeps_the_token_output() {
        // A native-input exact-output swap is funded up to its ceiling and sweeps the remainder
        // back, so it carries both commands. The unwrap returns the input, not the output.
        let router = Address::with_last_byte(2);
        let call = execute_call(
            &[WRAP_ETH, V3_SWAP_EXACT_OUT, UNWRAP_WETH],
            vec![Bytes::default(), v3_input(router, 500, 1_000, &[USDC, WETH]), Bytes::default()],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.token_out, USDC);
    }

    #[test]
    fn test_unwrap_before_the_swap_declines() {
        // The live shape of all seven exact-output v4 swaps in the sample:
        // `PERMIT2_TRANSFER_FROM UNWRAP_WETH V4_SWAP WRAP_ETH`. The trader pays WETH, the router
        // unwraps it for a native-ETH pool, and re-wraps the remainder to return it. The pool
        // names native ETH, so the calldata never names the token the trader actually paid.
        const PERMIT2_TRANSFER_FROM: u8 = 0x02;
        let call = execute_call(
            &[PERMIT2_TRANSFER_FROM, UNWRAP_WETH, V4_SWAP, WRAP_ETH],
            vec![
                Bytes::default(),
                Bytes::default(),
                v4_input(
                    &[V4_SWAP_EXACT_OUT_SINGLE, V4_SETTLE_ALL, V4_TAKE_ALL],
                    vec![
                        v4_exact_in_single(Address::ZERO, USDC, true, 500, 1_000),
                        Bytes::default(),
                        Bytes::default(),
                    ],
                ),
                Bytes::default(),
            ],
        );
        assert!(terms(&call).is_none());
    }

    #[test]
    fn test_exact_out_unwrap_alone_pays_a_native_output() {
        // No wrap, so the unwrap is what pays the trader.
        let router = Address::with_last_byte(2);
        let call = execute_call(
            &[V3_SWAP_EXACT_OUT, UNWRAP_WETH],
            vec![v3_input(router, 500, 1_000, &[WETH, USDC]), Bytes::default()],
        );
        let declared = terms(&call).unwrap();
        assert_eq!(declared.token_in, USDC);
        assert_eq!(declared.token_out, Address::ZERO);
    }

    #[test]
    fn test_zero_exact_output_declined() {
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call =
            execute_call(&[V3_SWAP_EXACT_OUT], vec![v3_input(trader, 0, 1_000, &[WETH, USDC])]);
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
        assert_eq!(declared.amount_in, Some(U256::from(100_000_000u64)));
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
        assert_eq!(declared.amount_in, Some(U256::from(100_000_000u64)));
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
        assert_eq!(declared.amount_in, Some(U256::from(27_636_981_441_190_000_621_809u128)));
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
        assert_eq!(declared.amount_in, Some(U256::from(1_000_000_000_000_000u64)));
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
