//! OKX `DexRouter` decoding.
//!
//! OKX states each settled order in its own `OrderRecord` event: both tokens, the trader, the
//! amount that entered the swap, and the amount returned. Nothing has to be recovered from the
//! ledger, so this is a log read rather than a calldata one — the same shape as `CoW`'s `Trade`
//! event, and it survives the router's several entry functions (`smartSwapByOrderId`,
//! `uniswapV3SwapTo`, …) because none of them changes the event.
//!
//! `fromAmount` is the amount that reached the pools, after OKX's own commission — the basis a
//! re-solve needs. Verified against four live Ethereum trades (blocks 25741800-25741815): every
//! `toToken`/`returnAmount` matched the settled record exactly, while `fromAmount` sat 0 to 85 bps
//! below the trader's gross spend, the commission OKX records in a separate event.

use alloy::{
    primitives::{address, Address},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    solvers::{DeclaredSwap, SolverDecoder},
    transfer_ledger::to_primitive_log,
    veto::Veto,
};

sol! {
    /// `DexRouter`'s per-order record. Every field is unindexed, so the whole trade sits in the
    /// log's data.
    event OrderRecord(
        address fromToken,
        address toToken,
        address sender,
        uint256 fromAmount,
        uint256 returnAmount
    );
}

/// OKX's sentinel for native ETH, normalized to the zero address like every other flow.
const OKX_NATIVE_ETH: Address = address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

fn normalize_native(token: Address) -> Address {
    if token == OKX_NATIVE_ETH {
        Address::ZERO
    } else {
        token
    }
}

/// The OKX solver.
pub(crate) struct Okx;

impl SolverDecoder for Okx {
    /// The settled trade, read from `OrderRecord`. Declines a transaction carrying more than one
    /// record: that is several orders in one transaction, and one record is not the trade.
    fn declared(&self, _input: &[u8], logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        let mut records = logs
            .iter()
            .filter(|log| log.topics().first() == Some(&OrderRecord::SIGNATURE_HASH));
        let Some(first) = records.next() else { return Ok(None) };
        if records.next().is_some() {
            return Ok(None);
        }
        let Ok(record) = OrderRecord::decode_log(&to_primitive_log(first)) else {
            return Ok(None);
        };
        if record.fromAmount.is_zero() || record.returnAmount.is_zero() {
            return Ok(None);
        }
        Ok(Some(DeclaredSwap::from_event(
            record.sender,
            normalize_native(record.fromToken),
            record.fromAmount,
            normalize_native(record.toToken),
            record.returnAmount,
        )))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{b256, Log as PrimitiveLog, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    /// The `DexRouter` address every sampled trade entered through.
    const ROUTER: Address = address!("0x28b1dc1a5e3699a428bc51d234dfab7c9cb2a183");

    fn order_record(
        from_token: Address,
        to_token: Address,
        sender: Address,
        from_amount: u128,
        return_amount: u128,
    ) -> Log {
        let event = OrderRecord {
            fromToken: from_token,
            toToken: to_token,
            sender,
            fromAmount: U256::from(from_amount),
            returnAmount: U256::from(return_amount),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(ROUTER, data.topics().to_vec(), data.data.clone());
        Log { inner: primitive, ..Default::default() }
    }

    fn settled(logs: &[Log]) -> Option<DeclaredSwap> {
        Okx.declared(&[], logs).ok().flatten()
    }

    #[test]
    fn test_event_signature_against_the_deployed_router() {
        // The topic0 observed on every sampled trade. A wrong `sol!` declaration would compile and
        // silently never match, so it is pinned here.
        assert_eq!(
            OrderRecord::SIGNATURE_HASH,
            b256!("0x1bb43f2da90e35f7b0cf38521ca95a49e68eb42fac49924930a5bd73cdf7576c")
        );
    }

    #[test]
    fn test_real_trade_amounts() {
        // Live tx 0x00532cf9…: USDT in, 0x423f4e61… out. `fromAmount` is 178,650 below the
        // trader's gross spend of 35,730,088 — OKX's commission, which never entered the swap.
        let usdt = address!("0xdac17f958d2ee523a2206206994597c13d831ec7");
        let token_out = address!("0x423f4e6138e475d85cf7ea071ac92097ed631eea");
        let trader = address!("0xe127a59e0290d038cf1b2a767f8d422451d95980");
        let flow = settled(&[order_record(
            usdt,
            token_out,
            trader,
            35_551_438,
            699_080_168_573_611_654_796_604_356,
        )])
        .unwrap();
        assert_eq!(flow.tracked, Some(trader));
        assert_eq!(flow.token_in, usdt);
        assert_eq!(flow.amount_in, Some(U256::from(35_551_438u64)));
        assert_eq!(flow.token_out, token_out);
        assert_eq!(flow.amount_out, Some(U256::from(699_080_168_573_611_654_796_604_356u128)));
    }

    #[test]
    fn test_native_sentinel_normalized() {
        // Live tx 0xceabae7f…: native ETH in, USDC out. OKX writes native as 0xeeee…ee.
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let flow = settled(&[order_record(
            OKX_NATIVE_ETH,
            usdc,
            addr(1),
            30_000_000_000_000_000,
            56_277_456,
        )])
        .unwrap();
        assert_eq!(
            flow,
            DeclaredSwap::from_event(
                addr(1),
                Address::ZERO,
                U256::from(30_000_000_000_000_000u64),
                usdc,
                U256::from(56_277_456u64),
            )
        );
    }

    #[test]
    fn test_several_orders_declined() {
        // Two records in one transaction: several orders, so no single one is the trade. Left to
        // the netting fallback.
        let logs = vec![
            order_record(addr(10), addr(11), addr(1), 1_000, 2_000),
            order_record(addr(11), addr(10), addr(2), 2_000, 1_000),
        ];
        assert!(settled(&logs).is_none());
    }

    #[test]
    fn test_no_record_declined() {
        assert!(
            settled(&[make_transfer_log(addr(10), addr(1), addr(2), U256::from(1_000))]).is_none()
        );
        assert!(settled(&[]).is_none());
    }

    #[test]
    fn test_zero_amounts_declined() {
        assert!(settled(&[order_record(addr(10), addr(11), addr(1), 0, 2_000)]).is_none());
        assert!(settled(&[order_record(addr(10), addr(11), addr(1), 1_000, 0)]).is_none());
    }
}
