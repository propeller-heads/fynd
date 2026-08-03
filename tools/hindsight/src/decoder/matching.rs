//! Which transactions in a block are solver trades.
//!
//! `select` is the cheap, receipt-only filter the decoder runs on every transaction before
//! anything costs a trace. It answers only "is this a solver trade at all" — how the trade is
//! then decoded is the decoders' job (see `decode`).

use alloy::{
    network::{AnyTransactionReceipt, ReceiptResponse},
    primitives::Address,
};
use tracing::debug;

use crate::decoder::{registry::Registry, solvers};

/// A transaction identified as a solver trade, ready to be traced and decoded — settled or
/// reverted, distinguished by `reverted`. A reverted match carries no logs to net a settled
/// amount from, so it is decoded differently (see `decoder::decode_transaction`), but it is the
/// same shape as a settled one otherwise: it was still a trade, just one that did not fill. Every
/// field is a reference or `Copy`, so the type itself is `Copy` — passed around freely rather than
/// threaded by move.
#[derive(Clone, Copy)]
pub(crate) struct MatchedSolverTrade<'a> {
    pub receipt: &'a AnyTransactionReceipt,
    /// The contract the transaction entered through (`tx.to`).
    pub entry_point: Address,
    pub reverted: bool,
}

/// Venues whose reverted swaps are worth decoding for the trader's `min_amount_out`. A reverted
/// transaction emits no logs, so entry-point matching is the only signal available — gated by
/// venue name (not chain), so a venue not listed here is simply not attempted.
const REVERT_DECODED_VENUES: &[&str] = &["relay"];

/// Match a receipt as a solver trade, settled or reverted.
///
/// A settled transaction qualifies two ways: its entry point (`tx.to`) is a known venue or
/// solver, or one of its logs was emitted by a known solver (filler-initiated intent fills,
/// where `tx.to` is a rotating filler). Matched transactions whose logs mark a non-swap order
/// shape are vetoed here (see `solvers::solver_veto`), before they cost a trace.
///
/// A reverted transaction emits no logs, so entry-point matching is the only signal available: it
/// matches only when `tx.to` is a venue in [`REVERT_DECODED_VENUES`]. There is no veto path for
/// reverts — a veto reads logs a revert never has.
pub(crate) fn select<'a>(
    receipt: &'a AnyTransactionReceipt,
    registry: &Registry,
) -> Option<MatchedSolverTrade<'a>> {
    if receipt.status() {
        let matched = match_settled(receipt, registry)?;
        if let Some(veto) =
            solvers::solver_veto(matched.receipt.logs(), matched.entry_point, registry)
        {
            debug!(
                tx = %matched.receipt.transaction_hash,
                venue = %registry.label(matched.entry_point),
                ?veto,
                "matched transaction is not a same-chain swap; skipping"
            );
            return None;
        }
        return Some(matched);
    }
    match_reverted(receipt, registry)
}

/// Match a settled receipt by its entry point or its solver logs.
fn match_settled<'a>(
    receipt: &'a AnyTransactionReceipt,
    registry: &Registry,
) -> Option<MatchedSolverTrade<'a>> {
    let entry_point = receipt.to?;
    if registry.is_known(entry_point) {
        return Some(MatchedSolverTrade { receipt, entry_point, reverted: false });
    }
    let via_log = receipt
        .logs()
        .iter()
        .any(|log| registry.is_solver(log.address()));
    via_log.then_some(MatchedSolverTrade { receipt, entry_point, reverted: false })
}

/// Match a reverted receipt as a candidate for revert decoding: `tx.to` is an entry point of a
/// venue in [`REVERT_DECODED_VENUES`].
fn match_reverted<'a>(
    receipt: &'a AnyTransactionReceipt,
    registry: &Registry,
) -> Option<MatchedSolverTrade<'a>> {
    let entry_point = receipt.to?;
    let venue_name = registry.venue_name(entry_point)?;
    REVERT_DECODED_VENUES
        .iter()
        .find(|&&name| name == venue_name)?;
    Some(MatchedSolverTrade { receipt, entry_point, reverted: true })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::test_utils::{addr, receipt, reverted_receipt, tx_hash};

    fn relay_entry_point(registry: &Registry) -> Address {
        *registry
            .venue("relay")
            .unwrap()
            .entry_points
            .iter()
            .next()
            .unwrap()
    }

    #[test]
    fn test_select_reverted_relay_tx_matches() {
        let registry = Registry::ethereum();
        let relay = relay_entry_point(&registry);
        let tx = reverted_receipt(tx_hash(1), addr(1), Some(relay));

        let matched = select(&tx, &registry).unwrap();
        assert_eq!(matched.entry_point, relay);
        assert!(matched.reverted);
    }

    #[test]
    fn test_select_reverted_non_venue_tx_does_not_match() {
        let registry = Registry::ethereum();
        // A registered solver (not a revert-decoded venue) reverting must not match: reverts are
        // only decoded for venues in REVERT_DECODED_VENUES.
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let tx = reverted_receipt(tx_hash(1), addr(1), Some(oneinch));
        assert!(select(&tx, &registry).is_none());

        // An address the registry knows nothing about.
        let unknown = reverted_receipt(tx_hash(2), addr(1), Some(addr(200)));
        assert!(select(&unknown, &registry).is_none());
    }

    #[test]
    fn test_select_reverted_requires_reverted_status() {
        let registry = Registry::ethereum();
        let relay = relay_entry_point(&registry);
        let successful = receipt(tx_hash(1), addr(1), Some(relay), vec![]);
        let matched = select(&successful, &registry).unwrap();
        assert!(!matched.reverted);
    }

    #[test]
    fn test_select_successful_path_unchanged() {
        // The existing successful-match path is untouched by the revert path.
        let registry = Registry::ethereum();
        let relay = relay_entry_point(&registry);
        let successful = receipt(tx_hash(1), addr(1), Some(relay), vec![]);
        let matched = select(&successful, &registry).unwrap();
        assert_eq!(matched.entry_point, relay);
        assert!(!matched.reverted);

        let reverted = reverted_receipt(tx_hash(2), addr(1), Some(relay));
        let matched = select(&reverted, &registry).unwrap();
        assert!(matched.reverted);
    }
}
