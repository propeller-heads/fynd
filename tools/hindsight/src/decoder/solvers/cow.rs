//! `CoW` Protocol settlement decoding.
//!
//! `CoW` settles signed orders in a batch: `tx.to` is the settlement contract and `tx.from` is the
//! solver, so the trade is an order owner's — read here from the `GPv2` `Trade` event the
//! settlement emits per order. The event gives the exact executed amounts and the owner directly:
//! declared data, so these records carry `decode: "declared"` like calldata decodes.
//!
//! One trade is produced per transaction, so only single-order settlements are decoded; a batch
//! settling several orders is declined to the netting fallback (which nets one swapper's flow).
//! `CoW`'s fee is taken from the sell token and backed out of the input so a re-solve compares
//! like-for-like — modern `CoW` records a zero on-chain fee (it is priced into the order).

use alloy::{
    primitives::{address, Address, B256},
    rpc::types::Log,
    sol,
    sol_types::{SolCall, SolEvent},
};

use crate::decoder::{
    netting::TraderFlow,
    registry::Registry,
    transfer_ledger::{to_primitive_log, NetSwap},
};

sol! {
    /// `GPv2` per-order settlement event.
    event Trade(
        address indexed owner,
        address sellToken,
        address buyToken,
        uint256 sellAmount,
        uint256 buyAmount,
        uint256 feeAmount,
        bytes orderUid
    );

    /// `GPv2Settlement.settle`, decoded only for the per-order `appData` hash — the frontend tag
    /// (`appCode`) the event does not carry. The other fields are named to match the ABI so the
    /// decode lines up; only `trades[].appData` is read.
    struct SettleTrade {
        uint256 sellTokenIndex;
        uint256 buyTokenIndex;
        address receiver;
        uint256 sellAmount;
        uint256 buyAmount;
        uint32 validTo;
        bytes32 appData;
        uint256 feeAmount;
        uint256 flags;
        uint256 executedAmount;
        bytes signature;
    }
    struct SettleInteraction {
        address target;
        uint256 value;
        bytes callData;
    }
    function settle(
        address[] tokens,
        uint256[] clearingPrices,
        SettleTrade[] trades,
        SettleInteraction[][3] interactions
    );
}

/// The settled order's `appData` hash, when the entry point is a batch settler and the batch
/// settles exactly one order — the same single-order rule `settlement_trade` applies, since a
/// multi-order batch has no single frontend to attribute. Venue attribution maps the hash to a
/// venue (`[venue_appdata]`).
pub(crate) fn venue_tag(registry: &Registry, entry_point: Address, input: &[u8]) -> Option<B256> {
    if !registry.is_batch_settler(entry_point) {
        return None;
    }
    let call = settleCall::abi_decode(input).ok()?;
    let [trade] = call.trades.as_slice() else {
        return None;
    };
    Some(trade.appData)
}

/// `CoW`'s sentinel for native ETH in buy orders, mapped to the zero address like every other flow.
const COW_NATIVE_ETH: Address = address!("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

/// The single settled order's trade, read from the `GPv2` `Trade` event. `None` when no batch
/// settler emitted one, or the batch settles more than one order (left to the netting fallback).
pub(crate) fn settlement_trade(logs: &[Log], registry: &Registry) -> Option<TraderFlow> {
    let mut trades = logs.iter().filter(|log| {
        registry.is_batch_settler(log.address()) &&
            log.topics().first() == Some(&Trade::SIGNATURE_HASH)
    });
    let first = trades.next()?;
    if trades.next().is_some() {
        return None;
    }
    let trade = Trade::decode_log(&to_primitive_log(first)).ok()?;

    // CoW's fee is taken from the sell token, so the amount that actually reached the market is
    // the executed sell minus the fee.
    let amount_in = trade
        .sellAmount
        .saturating_sub(trade.feeAmount);
    Some(TraderFlow::new(
        trade.owner,
        NetSwap {
            token_in: normalize_native(trade.sellToken),
            amount_in,
            token_out: normalize_native(trade.buyToken),
            amount_out: trade.buyAmount,
        },
    ))
}

fn normalize_native(token: Address) -> Address {
    if token == COW_NATIVE_ETH {
        Address::ZERO
    } else {
        token
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256, Bytes, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, swap};

    /// The Ethereum `CoW` settlement contract (a registered batch settler).
    const COW_SETTLEMENT: Address = address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41");

    fn trade_log(
        settler: Address,
        owner: Address,
        sell_token: Address,
        buy_token: Address,
        sell_amount: u64,
        buy_amount: u64,
        fee_amount: u64,
    ) -> Log {
        let event = Trade {
            owner,
            sellToken: sell_token,
            buyToken: buy_token,
            sellAmount: U256::from(sell_amount),
            buyAmount: U256::from(buy_amount),
            feeAmount: U256::from(fee_amount),
            orderUid: Bytes::new(),
        };
        let data = event.encode_log_data();
        let primitive = alloy::primitives::Log::new_unchecked(
            settler,
            data.topics().to_vec(),
            data.data.clone(),
        );
        Log { inner: primitive, ..Default::default() }
    }

    fn decode(logs: &[Log]) -> Option<TraderFlow> {
        settlement_trade(logs, &Registry::ethereum())
    }

    #[test]
    fn test_single_order_reads_the_trade_event() {
        let owner = addr(100);
        let sell = addr(10);
        let buy = addr(11);
        // Fee is taken from the sell token: 10 of the 1000 sold is the fee, 990 reached the market.
        let flow = decode(&[trade_log(COW_SETTLEMENT, owner, sell, buy, 1000, 2000, 10)]).unwrap();
        assert_eq!(flow.tracked, owner);
        assert_eq!(flow.swap, swap(sell, 990, buy, 2000));
    }

    #[test]
    fn test_native_eth_sentinel_normalized() {
        let flow =
            decode(&[trade_log(COW_SETTLEMENT, addr(100), addr(10), COW_NATIVE_ETH, 1000, 5, 0)])
                .unwrap();
        assert_eq!(flow.swap.token_out, Address::ZERO);
    }

    #[test]
    fn test_multi_order_batch_declined() {
        // Two orders in one settlement: one trade per transaction, so this is left to the
        // netting fallback.
        let logs = vec![
            trade_log(COW_SETTLEMENT, addr(100), addr(10), addr(11), 1000, 2000, 0),
            trade_log(COW_SETTLEMENT, addr(101), addr(11), addr(10), 2000, 1000, 0),
        ];
        assert!(decode(&logs).is_none());
    }

    #[test]
    fn test_no_trade_event_declined() {
        // A non-CoW intent fill (no Trade event) is declined so the netting fallback runs instead.
        assert!(decode(&[]).is_none());
    }

    fn settle_trade(app_data: B256) -> SettleTrade {
        SettleTrade {
            sellTokenIndex: U256::ZERO,
            buyTokenIndex: U256::ZERO,
            receiver: Address::ZERO,
            sellAmount: U256::ZERO,
            buyAmount: U256::ZERO,
            validTo: 0,
            appData: app_data,
            feeAmount: U256::ZERO,
            flags: U256::ZERO,
            executedAmount: U256::ZERO,
            signature: Bytes::new(),
        }
    }

    fn settle_calldata(trades: Vec<SettleTrade>) -> Vec<u8> {
        settleCall {
            tokens: vec![],
            clearingPrices: vec![],
            trades,
            interactions: Default::default(),
        }
        .abi_encode()
    }

    #[test]
    fn test_single_order_reads_app_data() {
        let registry = Registry::ethereum();
        let app = b256!("0xf249b3db926aa5b5a1b18f3fec86b9cc99b9a8a99ad7e8034242d2838ae97422");
        assert_eq!(
            venue_tag(&registry, COW_SETTLEMENT, &settle_calldata(vec![settle_trade(app)])),
            Some(app)
        );
    }

    #[test]
    fn test_multi_order_batch_has_no_single_app_data() {
        let registry = Registry::ethereum();
        let trades = vec![settle_trade(B256::ZERO), settle_trade(B256::ZERO)];
        assert!(venue_tag(&registry, COW_SETTLEMENT, &settle_calldata(trades)).is_none());
    }

    #[test]
    fn test_non_settler_entry_has_no_app_data() {
        let registry = Registry::ethereum();
        let app = b256!("0xf249b3db926aa5b5a1b18f3fec86b9cc99b9a8a99ad7e8034242d2838ae97422");
        assert!(venue_tag(&registry, addr(9), &settle_calldata(vec![settle_trade(app)])).is_none());
    }
}
