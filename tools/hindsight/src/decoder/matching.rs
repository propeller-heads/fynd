//! Which transactions in a block are solver trades.
//!
//! [`select`] is the cheap, receipt-only filter the decoder runs on every transaction before
//! anything costs a trace. It answers only "is this a solver trade at all" — how the trade is
//! then decoded is the strategies' job (see `strategies`).

use alloy::{primitives::Address, rpc::types::TransactionReceipt};
use tracing::debug;

use crate::decoder::{registry::Registry, solvers};

/// A transaction identified as a solver trade, ready to be traced and decoded.
pub(crate) struct MatchedSolverTrade<'a> {
    pub receipt: &'a TransactionReceipt,
    /// The contract the transaction entered through (`tx.to`).
    pub entry_point: Address,
}

/// Match a receipt as a solver trade.
///
/// A transaction qualifies two ways: its entry point (`tx.to`) is a known
/// venue or solver, or one of its logs was emitted by a known solver
/// (filler-initiated intent fills, where `tx.to` is a rotating filler).
/// Matched transactions whose logs mark a non-swap order shape are vetoed
/// here (see [`solvers::match_veto`]), before they cost a trace.
pub(crate) fn select<'a>(
    receipt: &'a TransactionReceipt,
    registry: &Registry,
) -> Option<MatchedSolverTrade<'a>> {
    let matched = match_entry(receipt, registry)?;
    if let Some(reason) = solvers::match_veto(matched.receipt.logs(), matched.entry_point, registry)
    {
        debug!(
            tx = %matched.receipt.transaction_hash,
            venue = %registry.label(matched.entry_point),
            reason,
            "matched transaction is not a same-chain swap; skipping"
        );
        return None;
    }
    Some(matched)
}

/// Match a receipt by its entry point or its solver logs.
fn match_entry<'a>(
    receipt: &'a TransactionReceipt,
    registry: &Registry,
) -> Option<MatchedSolverTrade<'a>> {
    if !receipt.status() {
        return None;
    }
    let entry_point = receipt.to?;
    if registry.is_known(entry_point) {
        return Some(MatchedSolverTrade { receipt, entry_point });
    }
    let via_log = receipt
        .logs()
        .iter()
        .any(|log| registry.is_solver(log.address()));
    via_log.then_some(MatchedSolverTrade { receipt, entry_point })
}
