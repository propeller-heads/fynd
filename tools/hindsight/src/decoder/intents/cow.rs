//! `CoW` Protocol settlement decoding.
//!
//! `CoW` settles signed orders in a batch: `tx.to` is the settlement contract and `tx.from` is the
//! solver, so the trade is an order owner's — read here from the `GPv2` `Trade` event the
//! settlement emits per order. The event gives the exact executed amounts and the owner directly,
//! which is more precise than netting the settlement's transfers and names the owner for client
//! attribution (`kpk`).
//!
//! One trade is produced per transaction, so only single-order settlements are decoded; a batch
//! settling several orders is declined to the generic intent netting (which nets one swapper's
//! flow). `CoW`'s fee is taken from the sell token and backed out of the input so a re-solve
//! compares like-for-like — modern `CoW` records a zero on-chain fee (it is priced into the order).

use alloy::{
    primitives::{address, Address, B256},
    sol,
    sol_types::{SolCall, SolEvent},
};
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
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

/// The settled order's `appData` hash, read from the `settle` calldata. `None` unless the batch
/// settles exactly one order — the same single-order rule `CowSettlement` applies, since a
/// multi-order batch has no single frontend to attribute.
pub(crate) fn order_app_data(input: &[u8]) -> Option<B256> {
    let call = settleCall::abi_decode(input).ok()?;
    let [trade] = call.trades.as_slice() else {
        return None;
    };
    Some(trade.appData)
}

/// `CoW`'s sentinel for native ETH in buy orders, mapped to the zero address like every other flow.
const COW_NATIVE_ETH: Address = address!("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

/// `CoW`'s settlement decoder, reading the `GPv2` `Trade` event.
pub(crate) struct CowSettlement;

#[async_trait]
impl TradeDecoder for CowSettlement {
    fn name(&self) -> &'static str {
        "cow-trade"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        let mut trades = ctx.receipt.logs().iter().filter(|log| {
            ctx.registry
                .is_batch_settler(log.address()) &&
                log.topics().first() == Some(&Trade::SIGNATURE_HASH)
        });
        let first = trades.next()?;
        // One trade per transaction: a multi-order batch is left to the generic intent netting.
        if trades.next().is_some() {
            return None;
        }
        let trade = Trade::decode_log(&to_primitive_log(first)).ok()?;

        // CoW's fee is taken from the sell token, so the amount that actually reached the market is
        // the executed sell minus the fee.
        let amount_in = trade
            .sellAmount
            .saturating_sub(trade.feeAmount);
        let fee = (!trade.feeAmount.is_zero()).then_some(trade.feeAmount);
        Some(TraderFlow {
            tracked: trade.owner,
            swap: NetSwap {
                token_in: normalize_native(trade.sellToken),
                amount_in,
                token_out: normalize_native(trade.buyToken),
                amount_out: trade.buyAmount,
            },
            venue_fee_in: fee,
            venue_fee_out: None,
            solver_override: None,
        })
    }
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
    use alloy::{
        primitives::{address, b256, Bytes, U256},
        rpc::types::Log,
        sol_types::SolCall,
    };

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, swap, CtxFixture},
        transfer_ledger::TransferLedger,
    };

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

    async fn decode(logs: Vec<Log>) -> Option<TraderFlow> {
        let registry = Registry::ethereum();
        let transfer_ledger = TransferLedger::from_transaction(&[], &[]);
        let mut fixture = CtxFixture::new(addr(2), COW_SETTLEMENT);
        fixture.set_logs(logs);
        let mut ctx = fixture.ctx(&registry, &transfer_ledger, &[]);
        CowSettlement.decode(&mut ctx).await
    }

    #[tokio::test]
    async fn test_single_order_reads_the_trade_event() {
        let owner = addr(100);
        let sell = addr(10);
        let buy = addr(11);
        // Fee is taken from the sell token: 10 of the 1000 sold is the fee, 990 reached the market.
        let flow = decode(vec![trade_log(COW_SETTLEMENT, owner, sell, buy, 1000, 2000, 10)])
            .await
            .unwrap();
        assert_eq!(flow.tracked, owner);
        assert_eq!(flow.swap, swap(sell, 990, buy, 2000));
        assert_eq!(flow.venue_fee_in, Some(U256::from(10)));
    }

    #[tokio::test]
    async fn test_native_eth_sentinel_normalized() {
        let flow = decode(vec![trade_log(
            COW_SETTLEMENT,
            addr(100),
            addr(10),
            COW_NATIVE_ETH,
            1000,
            5,
            0,
        )])
        .await
        .unwrap();
        assert_eq!(flow.swap.token_out, Address::ZERO);
        assert_eq!(flow.venue_fee_in, None);
    }

    #[tokio::test]
    async fn test_multi_order_batch_declined() {
        // Two orders in one settlement: one trade per transaction, so this is left to intent
        // netting.
        let logs = vec![
            trade_log(COW_SETTLEMENT, addr(100), addr(10), addr(11), 1000, 2000, 0),
            trade_log(COW_SETTLEMENT, addr(101), addr(11), addr(10), 2000, 1000, 0),
        ];
        assert!(decode(logs).await.is_none());
    }

    #[tokio::test]
    async fn test_no_trade_event_declined() {
        // A non-CoW intent fill (no Trade event) is declined so intent netting runs instead.
        assert!(decode(vec![]).await.is_none());
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
        let app = b256!("0xf249b3db926aa5b5a1b18f3fec86b9cc99b9a8a99ad7e8034242d2838ae97422");
        assert_eq!(order_app_data(&settle_calldata(vec![settle_trade(app)])), Some(app));
    }

    #[test]
    fn test_multi_order_batch_has_no_single_app_data() {
        let trades = vec![settle_trade(B256::ZERO), settle_trade(B256::ZERO)];
        assert!(order_app_data(&settle_calldata(trades)).is_none());
    }

    #[test]
    fn test_non_settle_calldata_has_no_app_data() {
        assert!(order_app_data(&[0u8; 4]).is_none());
    }
}
