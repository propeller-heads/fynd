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
//! Two shapes are declined rather than guessed, both verified against live traffic (40 Universal
//! Router trades over Ethereum blocks 25741800-25741815):
//!
//! - Anything containing `V4_SWAP` (`0x10`), 27 of the 40. A v4 leg carries its own action
//!   encoding, and a route can begin in v3 and finish in v4 — reading only the v3 leg reports the
//!   wrong `token_out`, which is what one sampled trade did.
//! - More than one exact-input swap, 3 of the 40: a split route, where no single command is the
//!   trade.
//!
//! On the remaining 10, every decoded token pair matched the settled record. Nine matched
//! `amount_in` exactly; the tenth reads 0.87% lower, which is a fee taken before the swap — the
//! calldata figure is the amount that reached the pools, which is the basis a re-solve needs.

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
const V2_SWAP_EXACT_IN: u8 = 0x08;
const WRAP_ETH: u8 = 0x0b;
const UNWRAP_WETH: u8 = 0x0c;
const V4_SWAP: u8 = 0x10;

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
            match command & COMMAND_TYPE_MASK {
                V4_SWAP => return Ok(None),
                WRAP_ETH => wrapped = true,
                UNWRAP_WETH => unwrapped = true,
                command @ (V3_SWAP_EXACT_IN | V2_SWAP_EXACT_IN) => {
                    if swap.is_some() {
                        return Ok(None);
                    }
                    let Some(read) = read_swap(command, command_input) else {
                        return Ok(None);
                    };
                    swap = Some(read);
                }
                _ => {}
            }
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

    #[test]
    fn test_v4_swap_declined() {
        // A route can start in v3 and finish in v4, so the v3 leg's token_out is not the trade's.
        let trader = address!("0x000000000000000000000000000000000000dead");
        let call = execute_call(
            &[V3_SWAP_EXACT_IN, V4_SWAP],
            vec![v3_input(trader, 100, 1, &[USDC, WETH]), Bytes::default()],
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
