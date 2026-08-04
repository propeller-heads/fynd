//! Transfer-netting: recover a swap from what actually moved.
//!
//! The evidence is the ERC-20 `Transfer` events plus the native transfers recovered from the
//! trace (see `transfer_ledger`) — what actually moved, not what any contract or calldata
//! declared. It needs no knowledge of any router's format.
//!
//! This module is a toolkit plus one decoder. The toolkit — `sender_flow` and `venue_flow` — is
//! the shared netting engine the venue decoders build on. The decoder is `SenderNetting`, for
//! direct solver swaps; intent fills and batch settlements are decoded in `super::intents`.
//!
//! Netting requires the trader to both pay and receive. When the swap's output is delivered to a
//! different receiver, nothing nets against the trader's input and the transaction is declined —
//! a coverage miss, never wrong amounts (see `transfer_ledger` for the model's assumptions).

use std::collections::HashSet;

use alloy::{primitives::Address, providers::Provider};
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, GasScope, TradeDecoder, TraderFlow},
    transfer_ledger::{NetSwap, TransferLedger},
};

/// Net the sender's flow. When the sender nets nothing, fall back to the contract the transaction
/// entered through (`tx.to`), for the rare shape where the swap output is delivered to that
/// contract rather than back to the sender.
///
/// A sender-tracked flow charges the whole receipt's gas (the trader sent the transaction); the
/// fallback charges nothing, since the tracked contract and the gas-paying sender differ.
pub(crate) fn sender_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<TraderFlow> {
    transfer_ledger
        .net_swap(sender)
        .map(|swap| TraderFlow {
            gas_scope: GasScope::WholeTransaction,
            ..TraderFlow::without_fees(sender, swap)
        })
        .or_else(|| {
            transfer_ledger
                .net_swap(entry_point)
                .map(|swap| TraderFlow::without_fees(entry_point, swap))
        })
}

/// Net the sender's flow and back the venue's fee out of it — the shared shape of every
/// fee-taking venue entry. Venue decoders call this, then add what is specific to them.
///
/// A trader-paid flow's gas scope narrows to the solver call's trace frame: inside a venue's
/// contract the receipt's gas includes the venue's own overhead, which is charged whichever solver
/// the venue picks and must stay out of the comparison.
///
/// One exception to the fee back-out: when the tracked trader IS a fee collector, the transaction
/// is a treasury operation — the collector's receipts are its own output, not a fee, and backing
/// them "out" would add the output to itself and double it.
pub(crate) fn venue_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
    fee_collectors: &HashSet<Address>,
) -> Option<TraderFlow> {
    let mut flow = sender_flow(transfer_ledger, sender, entry_point)?;
    if flow.gas_scope == GasScope::WholeTransaction {
        flow.gas_scope = GasScope::SolverFrame;
    }
    if fee_collectors.contains(&flow.tracked) {
        return Some(flow);
    }
    Some(back_out_venue_fees(flow, transfer_ledger, fee_collectors))
}

/// Back a venue fee out of a decoded user flow.
///
/// The venue can take its fee on either side. An input-side fee is subtracted from `amount_in`
/// (the user's gross spend included money that never entered the swap) and an output-side fee is
/// added back into `amount_out` (the swap produced more than the user kept), so both sides are the
/// amounts actually swapped — the like-for-like basis vs Fynd.
fn back_out_venue_fees(
    flow: TraderFlow,
    transfer_ledger: &TransferLedger,
    fee_collectors: &HashSet<Address>,
) -> TraderFlow {
    let fees = transfer_ledger.received_by(fee_collectors);
    let venue_fee_in = fees
        .get(&flow.swap.token_in)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_in =
        venue_fee_in.map_or(flow.swap.amount_in, |fee| flow.swap.amount_in.saturating_sub(fee));
    let venue_fee_out = fees
        .get(&flow.swap.token_out)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_out =
        venue_fee_out.map_or(flow.swap.amount_out, |fee| flow.swap.amount_out.saturating_add(fee));
    TraderFlow {
        tracked: flow.tracked,
        swap: NetSwap { amount_in, amount_out, ..flow.swap },
        venue_fee_in,
        venue_fee_out,
        solver_override: flow.solver_override,
        min_amount_out: flow.min_amount_out,
        gas_scope: flow.gas_scope,
    }
}

/// Direct solver swaps: the sender is the trader, so net the sender's flow.
pub(crate) struct SenderNetting;

#[async_trait]
impl<P: Provider> TradeDecoder<P> for SenderNetting {
    fn name(&self) -> &'static str {
        "sender-netting"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow> {
        sender_flow(ctx.transfer_ledger, ctx.receipt.from, ctx.entry_point)
    }
}
