//! LiFi-specific matching rules.
//!
//! `LiFi`'s Diamond settles both same-chain swaps (decoded like any solver) and cross-chain
//! bridge orders, which must never decode as swaps.

use alloy::{rpc::types::Log, sol, sol_types::SolEvent};

use crate::decoder::{solvers::SolverDecoder, transfer_ledger::to_primitive_log, veto::Veto};

/// The `LiFi` solver.
pub(crate) struct Lifi;

impl SolverDecoder for Lifi {
    /// Veto transactions that started a cross-chain bridge order.
    ///
    /// A bridge deposit is not a same-chain swap: the real output lands on the destination
    /// chain, and the trader's only same-chain receipt is a leftover refund. Netting that as a
    /// swap pairs the full input with the refund — a trade that never happened, at an absurd rate.
    fn veto(&self, logs: &[Log]) -> Option<Veto> {
        logs.iter()
            .any(|log| log.topics().first() == Some(&LiFiTransferStarted::SIGNATURE_HASH))
            .then_some(Veto::BridgeOrder)
    }

    /// The integrator tag a same-chain `LiFi` swap declares, from either generic-swap event. This
    /// is the only fingerprint of a `LiFi` frontend (Infinex, Robinhood's `LiFi` leg), which routes
    /// through the shared Diamond.
    fn integrator(&self, logs: &[Log]) -> Option<String> {
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
    use alloy::primitives::{Bytes, Log as PrimitiveLog, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    #[test]
    fn test_bridge_order() {
        // The LiFi bridge shape (tx 0x72b71802…): 7.2 ETH in, swapped to USDT, 99.5% bridged out,
        // and only the leftover refunded to the trader — flagged by LiFiTransferStarted.
        let diamond = addr(70);
        let primitive = PrimitiveLog::new_unchecked(
            diamond,
            vec![LiFiTransferStarted::SIGNATURE_HASH],
            Bytes::default(),
        );
        let logs = vec![Log { inner: primitive, ..Default::default() }];
        assert_eq!(Lifi.veto(&logs), Some(Veto::BridgeOrder));

        let swap_logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1000))];
        assert_eq!(Lifi.veto(&swap_logs), None);
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
        assert_eq!(Lifi.integrator(&logs).as_deref(), Some("infinex"));

        // A non-LiFi log carries no integrator.
        let other = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1))];
        assert_eq!(Lifi.integrator(&other), None);
    }
}
