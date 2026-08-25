//! `ButterSwap` router decoding.
//!
//! Butter is an aggregator that other aggregators route through: `ShapeShift` hands it the order,
//! and Butter hands it down again — a sampled trade ran `ShapeShift` to Butter to `LiFi` to Fly
//! before reaching the pools. Butter is the outermost router in those traces, so registering it
//! makes it the settling solver, and it has to state the trade itself or the inner routers'
//! reports stop being read.
//!
//! It does state it. `ButterRouterV31` (verified source) emits `SwapAndCall` on a same-chain swap,
//! carrying both assets, both amounts and the trader, so nothing is recovered from the ledger.
//! `SwapAndBridge` is the cross-chain counterpart and is vetoed the same way `LiFi`'s bridge
//! orders are: the output lands on another chain.
//!
//! Verified against the 27 sampled `ShapeShift` trades on Ethereum and Base: `from` was the
//! transaction sender in all 27, `originAmount` was exactly what the sender paid in all 27, and
//! `swapAmount` was exactly what `receiver` received in all 15 whose output was a token — the
//! other 12 paid out native ETH, which emits no transfer to compare against.
//!
//! Native ETH is the zero address in these events, already hindsight's convention.
//!
//! `referrer` names the frontend that sent the order — `ShapeShift`'s own address on every sampled
//! trade — and is not read here: naming a venue from it needs an address-keyed venue tag, which
//! the tag type does not have.

use alloy::{rpc::types::Log, sol, sol_types::SolEvent};

use crate::decoder::{
    solvers::{normalize_native, DeclaredSwap, SolverDecoder},
    transfer_ledger::to_primitive_log,
    veto::Veto,
};

sol! {
    /// A settled same-chain swap. `target`/`callAmount` describe the optional call Butter makes
    /// with the proceeds and are not part of the trade.
    event SwapAndCall(
        address indexed referrer,
        address indexed initiator,
        address indexed from,
        bytes32 transferId,
        address originToken,
        address swapToken,
        uint256 originAmount,
        uint256 swapAmount,
        address receiver,
        address target,
        uint256 callAmount
    );

    /// The cross-chain counterpart: the proceeds are bridged, so the output this transaction
    /// shows is an intermediate, not the trade's.
    event SwapAndBridge(
        address indexed referrer,
        address indexed initiator,
        address indexed from,
        bytes32 transferId,
        bytes32 orderId,
        address originToken,
        address bridgeToken,
        uint256 originAmount,
        uint256 bridgeAmount,
        uint256 toChain,
        bytes to
    );
}

/// The Butter solver.
pub(crate) struct Butter;

impl SolverDecoder for Butter {
    /// The trade Butter's own swap event states, or a veto when the order bridged instead.
    ///
    /// Declines a transaction carrying more than one swap event: that is several swaps in one
    /// call, and no single event is the trade.
    fn declared(&self, _input: &[u8], logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        if logs
            .iter()
            .any(|log| log.topics().first() == Some(&SwapAndBridge::SIGNATURE_HASH))
        {
            return Err(Veto::BridgeOrder);
        }
        let mut swaps = logs
            .iter()
            .filter(|log| log.topics().first() == Some(&SwapAndCall::SIGNATURE_HASH));
        let Some(first) = swaps.next() else { return Ok(None) };
        if swaps.next().is_some() {
            return Ok(None);
        }
        let Ok(swap) = SwapAndCall::decode_log(&to_primitive_log(first)) else {
            return Ok(None);
        };
        if swap.originAmount.is_zero() || swap.swapAmount.is_zero() {
            return Ok(None);
        }
        Ok(Some(DeclaredSwap::from_event(
            swap.from,
            normalize_native(swap.originToken),
            swap.originAmount,
            normalize_native(swap.swapToken),
            swap.swapAmount,
        )))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256, Address, Bytes, Log as PrimitiveLog, B256, U256};

    use super::*;
    use crate::decoder::test_utils::addr;

    /// The router, at the same address on Ethereum and Base.
    const ROUTER: Address = address!("0xee0319cf0bca5d09333f9f6277743e8de31bd69a");

    /// A real settled Base trade: `0xacfe6019…` in, USDC out, entered through `ShapeShift`, whose
    /// own address is the `referrer`.
    const REFERRER: Address = address!("0xf5aa59151be6515c4ca68a0282cf68b3ea4846fc");
    const TRADER: Address = address!("0x842d90d5395fd68050adf8d7934462854a75c591");
    const TOKEN_IN: Address = address!("0xacfe6019ed1a7dc6f7b508c02d1b04ec88cc21bf");
    const TOKEN_OUT: Address = address!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const AMOUNT_IN: u128 = 81_772_675_775_438_551_705;
    const AMOUNT_OUT: u128 = 962_670_267;

    fn swap_log(token_in: Address, token_out: Address, amount_in: u128, amount_out: u128) -> Log {
        let event = SwapAndCall {
            referrer: REFERRER,
            initiator: TRADER,
            from: TRADER,
            transferId: B256::ZERO,
            originToken: token_in,
            swapToken: token_out,
            originAmount: U256::from(amount_in),
            swapAmount: U256::from(amount_out),
            receiver: TRADER,
            target: Address::ZERO,
            callAmount: U256::ZERO,
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(ROUTER, data.topics().to_vec(), data.data.clone());
        Log { inner: primitive, ..Default::default() }
    }

    fn real_swap() -> Log {
        swap_log(TOKEN_IN, TOKEN_OUT, AMOUNT_IN, AMOUNT_OUT)
    }

    fn bridge_log() -> Log {
        let event = SwapAndBridge {
            referrer: REFERRER,
            initiator: TRADER,
            from: TRADER,
            transferId: B256::ZERO,
            orderId: B256::ZERO,
            originToken: TOKEN_IN,
            bridgeToken: TOKEN_OUT,
            originAmount: U256::from(AMOUNT_IN),
            bridgeAmount: U256::from(AMOUNT_OUT),
            toChain: U256::from(8_453u64),
            to: Bytes::default(),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(ROUTER, data.topics().to_vec(), data.data.clone());
        Log { inner: primitive, ..Default::default() }
    }

    fn settled(logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        Butter.declared(&[], logs)
    }

    #[test]
    fn test_event_signatures_against_the_deployed_router() {
        // The topics observed on live trades. A wrong `sol!` declaration would compile and
        // silently never match, so both are pinned here.
        assert_eq!(
            SwapAndCall::SIGNATURE_HASH,
            b256!("0x60656aafa8d4c0a705aeb148b167d7db921d08852cd2261b270d5c7a2e655f83")
        );
        assert_eq!(
            SwapAndBridge::SIGNATURE_HASH,
            b256!("0xba828651bf4de06e53231285961e555fd7dfe17a3e39d64b09fbaa8ebc0166c6")
        );
    }

    #[test]
    fn test_real_trade_amounts() {
        let flow = settled(&[real_swap()])
            .unwrap()
            .unwrap();
        assert_eq!(flow.tracked, Some(TRADER));
        assert_eq!(flow.token_in, TOKEN_IN);
        assert_eq!(flow.token_out, TOKEN_OUT);
        assert_eq!(flow.amount_in, Some(U256::from(AMOUNT_IN)));
        assert_eq!(flow.amount_out, Some(U256::from(AMOUNT_OUT)));
        // An event reports what happened, so there is no floor and nothing to recover.
        assert_eq!(flow.min_amount_out, None);
    }

    #[test]
    fn test_native_output_is_the_zero_address() {
        // Twelve of the sampled trades paid out native ETH, which Butter writes as the zero
        // address rather than a sentinel.
        let flow = settled(&[swap_log(TOKEN_IN, Address::ZERO, AMOUNT_IN, AMOUNT_OUT)])
            .unwrap()
            .unwrap();
        assert_eq!(flow.token_out, Address::ZERO);
    }

    #[test]
    fn test_bridge_order_vetoes_the_transaction() {
        // The proceeds leave the chain, so netting must not pair the input with whatever
        // intermediate this transaction shows.
        assert_eq!(settled(&[bridge_log()]).err(), Some(Veto::BridgeOrder));
        // Even alongside a swap event, the bridge order wins: the swap is the leg that funds it.
        assert_eq!(settled(&[real_swap(), bridge_log()]).err(), Some(Veto::BridgeOrder));
    }

    #[test]
    fn test_several_swaps_declined() {
        assert!(settled(&[real_swap(), real_swap()])
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_zero_amounts_declined() {
        assert!(settled(&[swap_log(TOKEN_IN, TOKEN_OUT, 0, AMOUNT_OUT)])
            .unwrap()
            .is_none());
        assert!(settled(&[swap_log(TOKEN_IN, TOKEN_OUT, AMOUNT_IN, 0)])
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_another_solvers_logs_declined() {
        // A transaction Butter did not settle: its logs say nothing this decoder can read.
        let other = PrimitiveLog::new_unchecked(addr(9), vec![B256::ZERO], Bytes::default());
        assert!(settled(&[Log { inner: other, ..Default::default() }])
            .unwrap()
            .is_none());
        assert!(settled(&[]).unwrap().is_none());
    }
}
