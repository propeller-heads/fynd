//! `LiFi` Diamond decoding.
//!
//! The Diamond settles both same-chain swaps and cross-chain bridge orders, and says which it did
//! in its own events. A same-chain swap emits a generic-swap event carrying the whole trade —
//! both assets, both amounts, and the address the output was paid to — so nothing has to be
//! recovered from the ledger. A bridge order emits `LiFiTransferStarted` instead and is vetoed:
//! its real output lands on another chain.
//!
//! `toAmount` is the swap's gross output, before any cut the Diamond or its integrator takes.
//! Verified against 9 live Ethereum trades: every `fromAssetId`/`fromAmount` matched the settled
//! record exactly, 8 of 9 `toAmount` did too, and the ninth read 0.5% above the trader's receipt —
//! a fee taken out of the output, which the gross figure is meant to include so a re-solve
//! compares gross against gross.
//!
//! Native ETH is the zero address in these events, already hindsight's convention.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    solvers::{DeclaredSwap, SolverDecoder},
    transfer_ledger::to_primitive_log,
    veto::Veto,
};

/// The `LiFi` solver.
pub(crate) struct Lifi;

impl SolverDecoder for Lifi {
    /// The trade the Diamond's own generic-swap event states, or a veto when its events say the
    /// order bridged to another chain instead.
    ///
    /// Declines a transaction carrying more than one generic-swap event: that is several swaps in
    /// one call, and no single event is the trade.
    fn declared(&self, _input: &[u8], logs: &[Log]) -> Result<Option<DeclaredSwap>, Veto> {
        if let Some(veto) = bridge_order(logs) {
            return Err(veto);
        }
        Ok(generic_swap(logs))
    }
}

/// The single generic swap in a transaction's logs, from either facet.
///
/// The current facet names the output recipient; the older one does not, so the caller falls back
/// to the transaction sender there.
fn generic_swap(logs: &[Log]) -> Option<DeclaredSwap> {
    let mut swaps = logs.iter().filter_map(swap_event);
    let first = swaps.next()?;
    if swaps.next().is_some() {
        return None;
    }
    Some(first)
}

/// One generic-swap event read as a settled trade, or `None` for any other log.
fn swap_event(log: &Log) -> Option<DeclaredSwap> {
    let topic = log.topics().first()?;
    if *topic == LiFiGenericSwapCompleted::SIGNATURE_HASH {
        let event = LiFiGenericSwapCompleted::decode_log(&to_primitive_log(log)).ok()?;
        return settled(
            event.receiver,
            event.fromAssetId,
            event.fromAmount,
            event.toAssetId,
            event.toAmount,
        );
    }
    if *topic == LiFiSwappedGeneric::SIGNATURE_HASH {
        let event = LiFiSwappedGeneric::decode_log(&to_primitive_log(log)).ok()?;
        // This facet names no recipient; the caller anchors on the transaction sender.
        return settled(
            Address::ZERO,
            event.fromAssetId,
            event.fromAmount,
            event.toAssetId,
            event.toAmount,
        );
    }
    None
}

/// A `DeclaredSwap` from one event's fields, declining the zero amounts a real swap cannot have.
fn settled(
    receiver: Address,
    token_in: Address,
    amount_in: U256,
    token_out: Address,
    amount_out: U256,
) -> Option<DeclaredSwap> {
    if amount_in.is_zero() || amount_out.is_zero() {
        return None;
    }
    let declared = DeclaredSwap::from_event(receiver, token_in, amount_in, token_out, amount_out);
    // A zero receiver is the older facet's "not stated", not an address that received anything.
    Some(if receiver.is_zero() { DeclaredSwap { tracked: None, ..declared } } else { declared })
}

/// Whether the transaction started a cross-chain bridge order.
///
/// A bridge deposit is not a same-chain swap: the real output lands on the destination chain, and
/// the trader's only same-chain receipt is a leftover refund. Netting that as a swap pairs the
/// full input with the refund — a trade that never happened, at an absurd rate.
fn bridge_order(logs: &[Log]) -> Option<Veto> {
    logs.iter()
        .any(|log| log.topics().first() == Some(&LiFiTransferStarted::SIGNATURE_HASH))
        .then_some(Veto::BridgeOrder)
}

/// The integrator tag a same-chain `LiFi` swap declares, from either generic-swap event. This is
/// the only fingerprint of a `LiFi` frontend (Infinex, Robinhood's `LiFi` leg), which routes
/// through the shared Diamond.
///
/// A venue fingerprint, not a claim about the trade, so venue attribution calls it directly
/// rather than through `SolverDecoder`.
pub(crate) fn integrator(logs: &[Log]) -> Option<String> {
    logs.iter()
        .find_map(|log| match log.topics().first() {
            Some(topic) if *topic == LiFiGenericSwapCompleted::SIGNATURE_HASH => {
                LiFiGenericSwapCompleted::decode_log(&to_primitive_log(log))
                    .ok()
                    .map(|event| event.integrator.clone())
            }
            Some(topic) if *topic == LiFiSwappedGeneric::SIGNATURE_HASH => {
                LiFiSwappedGeneric::decode_log(&to_primitive_log(log))
                    .ok()
                    .map(|event| event.integrator.clone())
            }
            _ => None,
        })
}

sol! {
    /// Emitted by the `LiFi` Diamond only when an order bridges to another chain (the tuple is
    /// `LiFi`'s `BridgeData`); same-chain `LiFi` swaps emit `LiFiGenericSwapCompleted` instead.
    event LiFiTransferStarted(
        (bytes32, string, string, address, address, address, uint256, uint256, bool, bool)
            bridgeData
    );

    /// Same-chain `LiFi` swap, current facet — carries the integrator tag as its first string.
    event LiFiGenericSwapCompleted(
        bytes32 indexed transactionId,
        string integrator,
        string referrer,
        address receiver,
        address fromAssetId,
        address toAssetId,
        uint256 fromAmount,
        uint256 toAmount
    );

    /// Same-chain `LiFi` swap, older facet — same integrator tag, no `receiver` field.
    event LiFiSwappedGeneric(
        bytes32 indexed transactionId,
        string integrator,
        string referrer,
        address fromAssetId,
        address toAssetId,
        uint256 fromAmount,
        uint256 toAmount
    );
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog, B256, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    #[test]
    fn test_bridge_order_vetoes_the_transaction() {
        // The LiFi bridge shape (tx 0x72b71802…): 7.2 ETH in, swapped to USDT, 99.5% bridged out,
        // and only the leftover refunded to the trader — flagged by LiFiTransferStarted.
        let diamond = addr(70);
        let primitive = PrimitiveLog::new_unchecked(
            diamond,
            vec![LiFiTransferStarted::SIGNATURE_HASH],
            Bytes::default(),
        );
        let logs = vec![Log { inner: primitive, ..Default::default() }];
        assert_eq!(Lifi.declared(&[], &logs).err(), Some(Veto::BridgeOrder));

        // A same-chain LiFi swap is not vetoed; it carries no terms this decoder reads, so it
        // decodes by netting.
        let swap_logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1000))];
        assert!(Lifi
            .declared(&[], &swap_logs)
            .is_ok_and(|declared| declared.is_none()));
    }

    /// A `LiFiGenericSwapCompleted` log with the given trade.
    fn generic_swap_log(
        receiver: Address,
        token_in: Address,
        amount_in: u128,
        token_out: Address,
        amount_out: u128,
    ) -> Log {
        let event = LiFiGenericSwapCompleted {
            transactionId: B256::ZERO,
            integrator: "jumper.exchange".to_string(),
            referrer: String::new(),
            receiver,
            fromAssetId: token_in,
            toAssetId: token_out,
            fromAmount: U256::from(amount_in),
            toAmount: U256::from(amount_out),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(addr(70), data.topics().to_vec(), data.data.clone());
        Log { inner: primitive, ..Default::default() }
    }

    fn declared_of(logs: &[Log]) -> Option<DeclaredSwap> {
        Lifi.declared(&[], logs).ok().flatten()
    }

    #[test]
    fn test_generic_swap_reads_the_whole_trade() {
        // Live tx 0xccbe500b…: native ETH in, USDT out, both amounts stated by the event.
        let usdt = address!("0xdac17f958d2ee523a2206206994597c13d831ec7");
        let trader = addr(1);
        let declared = declared_of(&[generic_swap_log(
            trader,
            Address::ZERO,
            326_595_334_876_135_158,
            usdt,
            802_642_717,
        )])
        .unwrap();
        assert_eq!(declared.tracked, Some(trader));
        assert_eq!(declared.token_in, Address::ZERO);
        assert_eq!(declared.amount_in, Some(U256::from(326_595_334_876_135_158u64)));
        assert_eq!(declared.token_out, usdt);
        assert_eq!(declared.amount_out, Some(U256::from(802_642_717u64)));
        // An event states the trade outright, so there is no floor to enforce.
        assert_eq!(declared.min_amount_out, None);
    }

    #[test]
    fn test_older_facet_states_no_recipient() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let event = LiFiSwappedGeneric {
            transactionId: B256::ZERO,
            integrator: "infinex".to_string(),
            referrer: String::new(),
            fromAssetId: Address::ZERO,
            toAssetId: usdc,
            fromAmount: U256::from(1_000u64),
            toAmount: U256::from(2_000u64),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(addr(70), data.topics().to_vec(), data.data.clone());
        let declared = declared_of(&[Log { inner: primitive, ..Default::default() }]).unwrap();
        // The facet names no receiver, so the caller anchors on the transaction sender.
        assert_eq!(declared.tracked, None);
        assert_eq!(declared.amount_out, Some(U256::from(2_000u64)));
    }

    #[test]
    fn test_several_generic_swaps_declined() {
        // Two swaps in one call: no single event is the trade.
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let logs = vec![
            generic_swap_log(addr(1), Address::ZERO, 1_000, usdc, 2_000),
            generic_swap_log(addr(2), usdc, 2_000, Address::ZERO, 1_000),
        ];
        assert!(declared_of(&logs).is_none());
    }

    #[test]
    fn test_zero_amounts_declined() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        assert!(declared_of(&[generic_swap_log(addr(1), Address::ZERO, 0, usdc, 2_000)]).is_none());
        assert!(declared_of(&[generic_swap_log(addr(1), Address::ZERO, 1_000, usdc, 0)]).is_none());
    }

    #[test]
    fn test_a_bridge_order_still_vetoes_over_a_swap_event() {
        // Both events present: the bridge veto wins, because the real output left the chain.
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let primitive = PrimitiveLog::new_unchecked(
            addr(70),
            vec![LiFiTransferStarted::SIGNATURE_HASH],
            Bytes::default(),
        );
        let logs = vec![
            Log { inner: primitive, ..Default::default() },
            generic_swap_log(addr(1), Address::ZERO, 1_000, usdc, 2_000),
        ];
        assert_eq!(Lifi.declared(&[], &logs).err(), Some(Veto::BridgeOrder));
    }

    #[test]
    fn test_integrator_from_generic_swap() {
        use alloy::primitives::{Address, B256};

        let event = LiFiGenericSwapCompleted {
            transactionId: B256::ZERO,
            integrator: "infinex".to_string(),
            referrer: String::new(),
            receiver: Address::ZERO,
            fromAssetId: addr(10),
            toAssetId: addr(11),
            fromAmount: U256::from(1000),
            toAmount: U256::from(2000),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(addr(70), data.topics().to_vec(), data.data.clone());
        let logs = vec![Log { inner: primitive, ..Default::default() }];
        assert_eq!(integrator(&logs).as_deref(), Some("infinex"));

        // A non-LiFi log carries no integrator.
        let other = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1))];
        assert_eq!(integrator(&other), None);
    }
}
