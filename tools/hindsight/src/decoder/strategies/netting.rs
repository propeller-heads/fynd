//! The transfer-netting strategy: recover the swap from the transaction's value movements.
//!
//! Its evidence is the ERC-20 `Transfer` events plus the native transfers recovered from the
//! trace (see `transfer_ledger`) — what actually moved, not what any contract or calldata
//! declared. It needs no knowledge of any router's format. Its one transaction-shape question
//! is whose net flow is the trade ([`TraderRole`]): the sender's, an order maker's, or a
//! venue-entered sender's with that venue's corrections applied.
//!
//! Netting requires the trader to both pay and receive. When the swap's output is delivered to
//! a different receiver, nothing nets against the trader's input and the transaction is
//! declined — a coverage miss, never wrong amounts (see `transfer_ledger` for the model's
//! assumptions).

use alloy::{primitives::Address, providers::Provider};
use async_trait::async_trait;

use crate::decoder::{
    intent,
    registry::{Registry, VenueAddresses},
    strategies::{DecodeContext, DecodeStrategy, GasScope, TraderFlow},
    transfer_ledger::TransferLedger,
    venues::{self, VenueContext, VenueKnowledge},
};

/// Recovers the swap by netting the trader's value movements.
pub(crate) struct TransferNetting;

#[async_trait]
impl<P: Provider> DecodeStrategy<P> for TransferNetting {
    fn name(&self) -> &'static str {
        "netting"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow> {
        let sender = ctx.receipt.from;
        match trader_role(ctx.entry_point, ctx.registry) {
            TraderRole::Sender => sender_flow(ctx.transfer_ledger, sender, ctx.entry_point),
            TraderRole::Maker => {
                intent::find_maker_trade(
                    ctx.provider,
                    ctx.transfer_ledger,
                    &[ctx.entry_point, sender],
                    ctx.registry,
                    ctx.code_cache,
                )
                .await
            }
            TraderRole::Venue(venue, addresses) => venue.decode(&VenueContext::new(ctx, addresses)),
        }
    }
}

/// Whose net flow is the trade. Fillers and batch settlers send on a maker's behalf, and
/// venue routers wrap the trade in their own contract, so netting first picks the address
/// to net.
enum TraderRole<'a> {
    /// The transaction sender (direct solver swaps).
    Sender,
    /// An order maker — the sender is a filler or batch settler acting on its behalf.
    Maker,
    /// The sender, entered through a venue's contract: sender netting plus that venue's
    /// corrections (fee back-out, gas scoping), with its address-book section resolved.
    Venue(&'static dyn VenueKnowledge, &'a VenueAddresses),
}

/// Classify whose net flow is the trade. Assumes the transaction already matched (see
/// `matching`): an entry point that is neither a venue nor otherwise known can only have
/// matched via a solver log, which is a filler-initiated intent fill.
fn trader_role(entry_point: Address, registry: &Registry) -> TraderRole<'_> {
    if let Some(name) = registry.venue_name(entry_point) {
        if let (Some(venue), Some(addresses)) = (venues::from_name(name), registry.venue(name)) {
            return TraderRole::Venue(venue, addresses);
        }
    }
    // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is an
    // order maker's net flow — decoded like a filler-initiated intent fill.
    if registry.is_batch_settler(entry_point) {
        return TraderRole::Maker;
    }
    if registry.is_known(entry_point) {
        return TraderRole::Sender;
    }
    TraderRole::Maker
}

/// Net the sender's flow. When the sender nets nothing, fall back to the contract the
/// transaction entered through (`tx.to`), for the rare shape where the swap output is
/// delivered to that contract rather than back to the sender.
///
/// A sender-tracked flow charges the whole receipt's gas (the trader sent the transaction);
/// the fallback charges nothing, since the tracked contract and the gas-paying sender differ.
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
