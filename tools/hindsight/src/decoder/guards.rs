//! Post-decode vetoes: netted "swaps" that are not comparable trades.
//!
//! Netting can pair value legs that were never a swap — the payment side of an NFT purchase, or
//! a cross-chain deposit's dust refund. Each guard here recognizes one such shape from the
//! transaction itself (no prices, no external data) so the decoder drops the record instead of
//! publishing a phantom trade.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::ledger::{NetSwap, Transfer};

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
pub(crate) fn received_nft(logs: &[Log], recipient: Address) -> bool {
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
/// wrapping is exactly 1:1 by construction, and fee skims only shave a few percent, so nothing
/// legitimate strays this far.
const WRAP_PAIR_MAX_RATIO: u64 = 2;

/// Whether a "swap" between the native token and its wrapped form has amounts a wrap or unwrap
/// cannot produce.
///
/// Seen with cross-chain deposits where the trader sends WETH and the only same-chain receipt is
/// a dust remainder refund in native ETH — netting pairs the two into a phantom trade orders of
/// magnitude off parity.
pub(crate) fn wrap_pair_mispaired(swap: &NetSwap, wrapped_native: Address) -> bool {
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
    fn received_nft_detects_erc721() {
        // The NFT purchase shape: buyer pays a token and receives an ERC-721, not a token amount.
        let buyer = addr(1);
        let seller = addr(2);
        let collection = addr(60);

        let logs = vec![make_nft_transfer_log(collection, seller, buyer, 4002)];
        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn received_nft_detects_erc1155_single() {
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
    fn received_nft_ignores_erc20_transfers() {
        // A plain ERC-20 Transfer (three topics, amount in data) must not read as an NFT even
        // though it shares the event signature.
        let user = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(2), user, U256::from(1000))];
        assert!(!received_nft(&logs, user));
    }

    #[test]
    fn wrap_pair_mispaired_flags_dust_refund() {
        // Relay cross-chain deposit shape (tx 0xc9de04eb…): 0.02 WETH in, a billionth of it
        // refunded back as native ETH — not an unwrap.
        let weth = addr(20);
        let deposit = swap(weth, 20_129_551_554_664_188, Address::ZERO, 1_554_664_188);
        assert!(wrap_pair_mispaired(&deposit, weth));

        let reversed = swap(Address::ZERO, 1_000_000, weth, 100);
        assert!(wrap_pair_mispaired(&reversed, weth));
    }

    #[test]
    fn wrap_pair_near_parity_kept() {
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 1000), weth));
        // A fee-skimmed unwrap stays within the 2x band.
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 900), weth));
    }

    #[test]
    fn non_wrap_pair_never_flagged() {
        // Ordinary token pairs legitimately trade at any rate (decimals differ), and a
        // token <-> wrapped-native trade without the native side is a real swap too.
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, addr(11), 5), weth));
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, weth, 5), weth));
        assert!(!wrap_pair_mispaired(&swap(Address::ZERO, 1_000_000_000, addr(11), 5), weth));
    }
}
