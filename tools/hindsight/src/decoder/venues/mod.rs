//! Per-venue decode strategies.
//!
//! Matching decides *which* transactions are aggregator trades; the strategy
//! decides *how* to recover the user's swap from one. Venue-specific behavior
//! (Relay's fee skim and rebalancing fills, intent-fill maker-finding) lives
//! in its own module so venues evolve independently.

pub(crate) mod intent;
pub(crate) mod relay;

use alloy::{
    primitives::{Address, U256},
    rpc::types::{Log, TransactionReceipt},
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    net::{decode_trade, NetSwap},
    registry::Registry,
};

sol! {
    /// Emitted by the LiFi Diamond only when an order bridges to another chain (the tuple is
    /// LiFi's `BridgeData`); same-chain LiFi swaps emit `LiFiGenericSwapCompleted` instead.
    event LiFiTransferStarted(
        (bytes32, string, string, address, address, address, uint256, uint256, bool, bool)
            bridgeData
    );
}

/// Whether the transaction started a cross-chain bridge order.
///
/// A bridge deposit is not a same-chain swap: the real output lands on the destination chain,
/// and the trader's only same-chain receipt is a leftover refund. Netting that as a swap pairs
/// the full input with the refund — a phantom trade with an absurd rate. A matching-time
/// concern, like [`select`]: rejected transactions never cost a trace.
pub(crate) fn started_bridge_order(logs: &[Log]) -> bool {
    logs.iter()
        .any(|log| log.topics().first() == Some(&LiFiTransferStarted::SIGNATURE_HASH))
}

/// How to recover the swap from a matched transaction.
pub(crate) enum Strategy {
    /// The sender is the trader: net its flow (direct aggregator swaps).
    Sender,
    /// The trader is an order maker, not the sender: either the tx was
    /// discovered via a known aggregator log (`tx.to` is a rotating filler) or
    /// `tx.to` is a batch settler entered by a solver.
    Maker,
    /// Relay client entry: sender netting with client-fee back-out, falling
    /// back to solver-rebalance decoding.
    Relay,
}

/// A matched transaction and the strategy to decode it.
pub(crate) struct Matched<'a> {
    pub receipt: &'a TransactionReceipt,
    pub entry_point: Address,
    pub strategy: Strategy,
}

/// The decoded user flow of a matched transaction.
pub(crate) struct Flow {
    /// The address whose net flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Client fee skimmed from the input token, already backed out of
    /// `swap.amount_in`.
    pub client_fee: Option<U256>,
    /// Client fee skimmed from the output token, already added back into
    /// `swap.amount_out`.
    pub client_fee_out: Option<U256>,
}

impl Flow {
    fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self { tracked, swap, client_fee: None, client_fee_out: None }
    }
}

/// Match a receipt and choose its decode strategy.
///
/// A transaction qualifies two ways: its entry point (`tx.to`) is a known
/// client or aggregator, or one of its logs was emitted by a known aggregator
/// (filler-initiated intent fills, where `tx.to` is a rotating filler).
pub(crate) fn select<'a>(
    receipt: &'a TransactionReceipt,
    registry: &Registry,
) -> Option<Matched<'a>> {
    if !receipt.status() {
        return None;
    }
    let entry_point = receipt.to?;
    if registry
        .relay()
        .routers
        .contains(&entry_point)
    {
        return Some(Matched { receipt, entry_point, strategy: Strategy::Relay });
    }
    if registry.is_known(entry_point) {
        // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is
        // an order maker's net flow — decode it like a filler-initiated intent fill.
        let strategy =
            if registry.is_batch_settler(entry_point) { Strategy::Maker } else { Strategy::Sender };
        return Some(Matched { receipt, entry_point, strategy });
    }
    let via_log = receipt
        .logs()
        .iter()
        .any(|log| registry.is_aggregator(log.address()));
    via_log.then_some(Matched { receipt, entry_point, strategy: Strategy::Maker })
}

/// Net the sender's flow, falling back to the entry point for the rare case
/// where output is delivered there.
pub(crate) fn sender_flow(
    logs: &[Log],
    native: &[(Address, Address, U256)],
    sender: Address,
    entry_point: Address,
) -> Option<Flow> {
    decode_trade(logs, native, sender)
        .map(|swap| Flow::without_fees(sender, swap))
        .or_else(|| {
            decode_trade(logs, native, entry_point)
                .map(|swap| Flow::without_fees(entry_point, swap))
        })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Log as PrimitiveLog;

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    #[test]
    fn bridge_order_detected() {
        // The LiFi bridge shape (tx 0x72b71802…): 7.2 ETH in, swapped to USDT, 99.5% bridged out,
        // and only the leftover refunded to the trader — flagged by LiFiTransferStarted.
        let diamond = addr(70);
        let primitive = PrimitiveLog::new_unchecked(
            diamond,
            vec![LiFiTransferStarted::SIGNATURE_HASH],
            Default::default(),
        );
        let logs = vec![Log { inner: primitive, ..Default::default() }];
        assert!(started_bridge_order(&logs));

        let swap_logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1000))];
        assert!(!started_bridge_order(&swap_logs));
    }
}
