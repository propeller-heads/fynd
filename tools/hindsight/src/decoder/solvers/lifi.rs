//! LiFi-specific matching rules.
//!
//! `LiFi`'s Diamond settles both same-chain swaps (decoded like any solver) and cross-chain
//! bridge orders, which must never decode as swaps.

use alloy::{rpc::types::Log, sol, sol_types::SolEvent};

use crate::decoder::solvers::SolverKnowledge;

/// The `LiFi` solver.
pub(crate) struct Lifi;

impl SolverKnowledge for Lifi {
    /// Veto transactions that started a cross-chain bridge order.
    ///
    /// A bridge deposit is not a same-chain swap: the real output lands on the destination
    /// chain, and the trader's only same-chain receipt is a leftover refund. Netting that as a
    /// swap pairs the full input with the refund — a trade that never happened, at an absurd rate.
    fn match_veto(&self, logs: &[Log]) -> Option<&'static str> {
        logs.iter()
            .any(|log| log.topics().first() == Some(&LiFiTransferStarted::SIGNATURE_HASH))
            .then_some("cross-chain bridge order")
    }
}

sol! {
    /// Emitted by the `LiFi` Diamond only when an order bridges to another chain (the tuple is
    /// `LiFi`'s `BridgeData`); same-chain `LiFi` swaps emit `LiFiGenericSwapCompleted` instead.
    event LiFiTransferStarted(
        (bytes32, string, string, address, address, address, uint256, uint256, bool, bool)
            bridgeData
    );
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Bytes, Log as PrimitiveLog, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    #[test]
    fn bridge_order() {
        // The LiFi bridge shape (tx 0x72b71802…): 7.2 ETH in, swapped to USDT, 99.5% bridged out,
        // and only the leftover refunded to the trader — flagged by LiFiTransferStarted.
        let diamond = addr(70);
        let primitive = PrimitiveLog::new_unchecked(
            diamond,
            vec![LiFiTransferStarted::SIGNATURE_HASH],
            Bytes::default(),
        );
        let logs = vec![Log { inner: primitive, ..Default::default() }];
        assert_eq!(Lifi.match_veto(&logs), Some("cross-chain bridge order"));

        let swap_logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1000))];
        assert_eq!(Lifi.match_veto(&swap_logs), None);
    }
}
