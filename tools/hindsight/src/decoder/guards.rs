//! Post-decode vetoes: netted "swaps" that are not comparable trades.
//!
//! Netting can pair value legs that were never a swap — the payment side of an NFT purchase, or
//! a cross-chain deposit's dust refund. Each guard recognizes one such shape from the
//! transaction itself (no prices, no external data); [`veto`] is the single check the decoder
//! runs on every decoded flow, so adding a guard never touches the orchestrator.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    registry::Registry,
    strategies::TraderFlow,
    transfer_ledger::{NetSwap, Transfer},
};

/// A decoded flow rejected as not a comparable trade, by the shape that disqualified it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Veto {
    /// The trader received an NFT: the netted token flow is the payment side of a purchase
    /// (e.g. an NFT sweep through Relay + Seaport), and the real consideration is invisible to
    /// ERC-20 netting — recording it would pair the payment with the change as a swap that never
    /// happened.
    NftPurchase,
    /// A native <-> wrapped-native "swap" far off 1:1, which a wrap or unwrap cannot produce: a
    /// mis-paired cross-chain deposit whose only same-chain receipt is a dust remainder refund.
    MispairedWrapPair,
}

/// Check a decoded flow against every guard, returning the first veto.
pub(crate) fn veto(flow: &TraderFlow, logs: &[Log], registry: &Registry) -> Option<Veto> {
    if received_nft(logs, flow.tracked) {
        return Some(Veto::NftPurchase);
    }
    if wrap_pair_mispaired(&flow.swap, registry.wrapped_native()) {
        return Some(Veto::MispairedWrapPair);
    }
    None
}

sol! {
    event TransferSingle(
        address indexed operator, address indexed from, address indexed to, uint256 id,
        uint256 value
    );
    event TransferBatch(
        address indexed operator, address indexed from, address indexed to, uint256[] ids,
        uint256[] values
    );
}

/// Whether `recipient` received an NFT (ERC-721 or ERC-1155) in the transaction.
///
/// An ERC-721 `Transfer` shares the ERC-20 event signature but indexes all three parameters
/// (four topics, empty data), so it is invisible to ERC-20 netting; ERC-1155 uses its own
/// events with the recipient as the third indexed parameter. A trader who received an NFT was
/// buying, not swapping: the netted token flow is the payment side of a purchase and the real
/// consideration is invisible, so recording it would pair the payment with the change.
fn received_nft(logs: &[Log], recipient: Address) -> bool {
    for log in logs {
        let topics = log.topics();
        let Some(&signature) = topics.first() else {
            continue;
        };
        let to = if signature == Transfer::SIGNATURE_HASH && topics.len() == 4 {
            topics[2]
        } else if (signature == TransferSingle::SIGNATURE_HASH ||
            signature == TransferBatch::SIGNATURE_HASH) &&
            topics.len() == 4
        {
            topics[3]
        } else {
            continue;
        };
        if Address::from_word(to) == recipient {
            return true;
        }
    }
    false
}

/// A wrap-pair trade (native <-> wrapped native) more than this factor off 1:1 is mis-paired:
/// wrapping is exactly 1:1 by construction, and venue fees only remove a few percent, so nothing
/// legitimate strays this far.
const WRAP_PAIR_MAX_RATIO: u64 = 2;

/// Whether a "swap" between the native token and its wrapped form has amounts a wrap or unwrap
/// cannot produce.
///
/// Seen with cross-chain deposits where the trader sends WETH and the only same-chain receipt is
/// a dust remainder refund in native ETH — netting pairs the two into a trade that never happened,
/// orders of magnitude off parity.
fn wrap_pair_mispaired(swap: &NetSwap, wrapped_native: Address) -> bool {
    let pair = [swap.token_in, swap.token_out];
    if !(pair.contains(&Address::ZERO) && pair.contains(&wrapped_native)) {
        return false;
    }
    let max = U256::from(WRAP_PAIR_MAX_RATIO);
    swap.amount_in > swap.amount_out.saturating_mul(max) ||
        swap.amount_out > swap.amount_in.saturating_mul(max)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Log as PrimitiveLog;

    use super::*;
    use crate::decoder::test_utils::{addr, make_nft_transfer_log, make_transfer_log, swap};

    #[test]
    fn received_nft_erc721() {
        // The NFT purchase shape: buyer pays a token and receives an ERC-721, not a token amount.
        let buyer = addr(1);
        let seller = addr(2);
        let collection = addr(60);

        let logs = vec![make_nft_transfer_log(collection, seller, buyer, 4002)];
        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn received_nft_erc1155_single() {
        let buyer = addr(1);
        let operator = addr(3);
        let seller = addr(2);
        let collection = addr(60);

        let event = TransferSingle {
            operator,
            from: seller,
            to: buyer,
            id: U256::from(7),
            value: U256::from(1),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(collection, data.topics().to_vec(), data.data.clone());
        let logs = vec![Log { inner: primitive, ..Default::default() }];

        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn received_nft_erc20_only() {
        // A plain ERC-20 Transfer (three topics, amount in data) must not read as an NFT even
        // though it shares the event signature.
        let user = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(2), user, U256::from(1000))];
        assert!(!received_nft(&logs, user));
    }

    #[test]
    fn wrap_pair_dust_refund() {
        // Relay cross-chain deposit shape (tx 0xc9de04eb…): 0.02 WETH in, a billionth of it
        // refunded back as native ETH — not an unwrap.
        let weth = addr(20);
        let deposit = swap(weth, 20_129_551_554_664_188, Address::ZERO, 1_554_664_188);
        assert!(wrap_pair_mispaired(&deposit, weth));

        let reversed = swap(Address::ZERO, 1_000_000, weth, 100);
        assert!(wrap_pair_mispaired(&reversed, weth));
    }

    #[test]
    fn wrap_pair_near_parity() {
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 1000), weth));
        // An unwrap with a fee taken stays within the 2x band.
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 900), weth));
    }

    #[test]
    fn non_wrap_pair() {
        // Ordinary token pairs legitimately trade at any rate (decimals differ), and a
        // token <-> wrapped-native trade without the native side is a real swap too.
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, addr(11), 5), weth));
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, weth, 5), weth));
        assert!(!wrap_pair_mispaired(&swap(Address::ZERO, 1_000_000_000, addr(11), 5), weth));
    }
}
