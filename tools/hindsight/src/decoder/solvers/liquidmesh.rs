//! `LiquidMesh` log extraction.
//!
//! `LiquidMesh` settles Trust Wallet's swap flow and states each settled trade in one event: both
//! tokens, the trader, the amount that entered the swap, and the amount returned. Nothing is left
//! to recover from the ledger, the same shape as OKX's `OrderRecord`.
//!
//! The event is read by position rather than declared with `sol!`, which every other log read
//! here uses. The router is a proxy whose implementation is verified nowhere we checked
//! (Sourcify, Blockscout), and neither its entry selector nor this event's signature appears in
//! any public 4-byte database, so the name the hash was computed from is unknown — and a `sol!`
//! declaration with a guessed name would produce a different hash and silently never match. The
//! topic is pinned from live traffic instead, and the six unindexed words are read where they sit.
//!
//! The layout was recovered over 37 swaps across Ethereum and Base: word 3 was the transaction
//! sender in all 37, and word 5 was exactly the amount that address received of word 2's token in
//! all 37. Word 4 is the input after `LiquidMesh`'s own fee — the amount that reached the pools,
//! which is the basis a re-solve needs. The fee itself is stated in a second event, paid to Trust
//! Wallet's fee wallet in the 32 of those swaps that carried one, and is not read here: a venue
//! fee is not modelled unless the address book lists the wallet.
//!
//! Word 0 is an identifier that also appears in the fee event, and is not read.

use alloy::{
    primitives::{b256, Address, B256, U256},
    rpc::types::Log,
};

use crate::decoder::{
    solvers::{normalize_native, DeclaredSwap, SolverDecoder},
    veto::Veto,
};

/// The settled-trade event's topic, pinned from live traffic (see the module docs for why it is
/// not derived from a signature).
const SWAP_TOPIC: B256 =
    b256!("0xe5b9f85c5caca875a8b78e5b2d88de86d7793cbff3d81ea4ecbec4c2b9ad7beb");

/// The event's unindexed words: identifier, sell token, buy token, trader, amount in, amount out.
const SWAP_WORDS: usize = 6;
const WORD: usize = 32;

const TOKEN_IN_WORD: usize = 1;
const TOKEN_OUT_WORD: usize = 2;
const TRADER_WORD: usize = 3;
const AMOUNT_IN_WORD: usize = 4;
const AMOUNT_OUT_WORD: usize = 5;

fn amount_at(data: &[u8], index: usize) -> U256 {
    U256::from_be_slice(&data[index * WORD..(index + 1) * WORD])
}

/// The low 20 bytes of a word, which is where an ABI-encoded address sits.
fn address_at(data: &[u8], index: usize) -> Address {
    Address::from_slice(&data[(index + 1) * WORD - Address::len_bytes()..(index + 1) * WORD])
}

/// The `LiquidMesh` solver.
pub(crate) struct LiquidMesh;

impl SolverDecoder for LiquidMesh {
    /// The settled trade, read from the router's own event. Declines a transaction carrying more
    /// than one: that is several orders in one transaction, and one event is not the trade.
    fn declared(&self, _input: &[u8], logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let mut swaps = logs
            .iter()
            .filter(|log| log.topics().first() == Some(&SWAP_TOPIC));
        let Some(first) = swaps.next() else { return Ok(None) };
        if swaps.next().is_some() {
            return Ok(None);
        }
        let data = first.data().data.as_ref();
        if data.len() != SWAP_WORDS * WORD {
            return Ok(None);
        }
        let amount_in = amount_at(data, AMOUNT_IN_WORD);
        let amount_out = amount_at(data, AMOUNT_OUT_WORD);
        if amount_in.is_zero() || amount_out.is_zero() {
            return Ok(None);
        }
        Ok(Some(DeclaredSwap::from_event(
            address_at(data, TRADER_WORD),
            normalize_native(address_at(data, TOKEN_IN_WORD)),
            amount_in,
            normalize_native(address_at(data, TOKEN_OUT_WORD)),
            amount_out,
        )))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog};

    use super::*;
    use crate::decoder::solvers::NATIVE_TOKEN_SENTINEL;

    /// The router, at the same address on Ethereum and Base.
    const ROUTER: Address = address!("0x3d90f66b534dd8482b181e24655a9e8265316be9");

    /// The event data of a real settled Base trade (tx `0x494c52d0…`): USDC in, `0xf5f11bc9…`
    /// out. The trader's gross spend was 3,171,629, so the 3,149,428 below is 22,201 less — the
    /// fee the second event states, which never entered the swap.
    fn real_data() -> Vec<u8> {
        let text = include_str!("fixtures/liquidmesh_swap_log.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    const TOKEN_IN: Address = address!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const TOKEN_OUT: Address = address!("0xf5f11bc9be9d6690f795d04d2fc9bdd097008a2b");
    const TRADER: Address = address!("0xd87b9a133bd6a90613821665799e6e9d7a160dbe");
    const AMOUNT_IN: u128 = 3_149_428;
    const AMOUNT_OUT: u128 = 34_299_129_800_723_082_679_493;

    fn swap_log(data: Vec<u8>) -> Log {
        let primitive = PrimitiveLog::new_unchecked(ROUTER, vec![SWAP_TOPIC], Bytes::from(data));
        Log { inner: primitive, ..Default::default() }
    }

    /// An event carrying arbitrary terms, for the cases a real fixture cannot express.
    fn built(token_in: Address, token_out: Address, amount_in: u128, amount_out: u128) -> Log {
        let mut data = vec![0u8; SWAP_WORDS * WORD];
        data[TOKEN_IN_WORD * WORD + 12..(TOKEN_IN_WORD + 1) * WORD]
            .copy_from_slice(token_in.as_slice());
        data[TOKEN_OUT_WORD * WORD + 12..(TOKEN_OUT_WORD + 1) * WORD]
            .copy_from_slice(token_out.as_slice());
        data[TRADER_WORD * WORD + 12..(TRADER_WORD + 1) * WORD].copy_from_slice(TRADER.as_slice());
        data[(AMOUNT_IN_WORD + 1) * WORD - 16..(AMOUNT_IN_WORD + 1) * WORD]
            .copy_from_slice(&amount_in.to_be_bytes());
        data[(AMOUNT_OUT_WORD + 1) * WORD - 16..(AMOUNT_OUT_WORD + 1) * WORD]
            .copy_from_slice(&amount_out.to_be_bytes());
        swap_log(data)
    }

    fn settled(logs: &[Log]) -> Option<DeclaredSwap> {
        LiquidMesh
            .declared(&[], logs)
            .ok()
            .flatten()
    }

    #[test]
    fn test_real_fixture_states_the_whole_trade() {
        let flow = settled(&[swap_log(real_data())]).unwrap();
        assert_eq!(flow.tracked, Some(TRADER));
        assert_eq!(flow.token_in, TOKEN_IN);
        assert_eq!(flow.token_out, TOKEN_OUT);
        assert_eq!(flow.amount_in, Some(U256::from(AMOUNT_IN)));
        assert_eq!(flow.amount_out, Some(U256::from(AMOUNT_OUT)));
        // An event reports what happened, so there is no floor and nothing to recover.
        assert_eq!(flow.min_amount_out, None);
        assert_eq!(flow.output_recipient, None);
    }

    #[test]
    fn test_native_sentinel_normalized() {
        // Trades with native ETH on either side write it as 0xeeee…ee, as the sampled
        // Base trades did on both the sell and the buy side.
        let sold = settled(&[built(NATIVE_TOKEN_SENTINEL, TOKEN_OUT, 1_000, 2_000)]).unwrap();
        assert_eq!(sold.token_in, Address::ZERO);
        let bought = settled(&[built(TOKEN_IN, NATIVE_TOKEN_SENTINEL, 1_000, 2_000)]).unwrap();
        assert_eq!(bought.token_out, Address::ZERO);
    }

    #[test]
    fn test_several_orders_declined() {
        // Two settled trades in one transaction: neither event is the trade.
        assert!(settled(&[swap_log(real_data()), swap_log(real_data())]).is_none());
    }

    #[test]
    fn test_zero_amounts_declined() {
        assert!(settled(&[built(TOKEN_IN, TOKEN_OUT, 0, 2_000)]).is_none());
        assert!(settled(&[built(TOKEN_IN, TOKEN_OUT, 1_000, 0)]).is_none());
    }

    #[test]
    fn test_wrong_length_and_other_events_declined() {
        // A shorter or longer payload is not this event, whatever the topic says.
        assert!(settled(&[swap_log(real_data()[..5 * WORD].to_vec())]).is_none());
        let mut longer = real_data();
        longer.extend_from_slice(&[0u8; WORD]);
        assert!(settled(&[swap_log(longer)]).is_none());
        // Another contract's log in the same transaction.
        let other = PrimitiveLog::new_unchecked(
            ROUTER,
            vec![b256!("0x154ab36932f6d9fd6543c4b5dd4df19a2aef3f401431bce07c4fdc426c2adf9d")],
            Bytes::from(real_data()),
        );
        assert!(settled(&[Log { inner: other, ..Default::default() }]).is_none());
    }

    #[test]
    fn test_no_logs_declined() {
        assert!(settled(&[]).is_none());
    }
}
