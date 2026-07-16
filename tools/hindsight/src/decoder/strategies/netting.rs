//! The transfer-netting strategy: recover the swap from the transaction's value movements.
//!
//! Its evidence is the ERC-20 `Transfer` events plus the native transfers recovered from the
//! trace (see `transfer_ledger`) — what actually moved, not what any contract or calldata
//! declared. It needs no knowledge of any router's format. Its one transaction-shape question
//! is whose net flow is the trade ([`TraderShape`]): the sender's, an order maker's, or a
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
    registry::Registry,
    strategies::{DecodeContext, DecodeStrategy, Flow, GasScope},
    transfer_ledger::TransferLedger,
    venues,
};

/// Recovers the swap by netting the trader's value movements.
pub(crate) struct TransferNetting;

#[async_trait]
impl<P: Provider> DecodeStrategy<P> for TransferNetting {
    fn name(&self) -> &'static str {
        "netting"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_, P>) -> Option<Flow> {
        let sender = ctx.receipt.from;
        match trader_shape(ctx.entry_point, ctx.registry) {
            TraderShape::Sender => sender_flow(ctx.transfer_ledger, sender, ctx.entry_point),
            TraderShape::Maker => {
                intent::find_maker_trade(
                    ctx.provider,
                    ctx.transfer_ledger,
                    &[ctx.entry_point, sender],
                    ctx.registry,
                    ctx.code_cache,
                )
                .await
            }
            TraderShape::Venue(venue) => venue.decode(&venues::VenueContext {
                transfer_ledger: ctx.transfer_ledger,
                sender,
                entry_point: ctx.entry_point,
                input: ctx.input,
                registry: ctx.registry,
            }),
        }
    }
}

/// Which address's net flow is the trade. A matched transaction is not always sent by the
/// trader — fillers and batch settlers send on an order maker's behalf, and venue routers wrap
/// the trade in their own contract — so netting must first decide whose flow to read.
enum TraderShape {
    /// The transaction sender is the trader: net its flow (direct solver swaps).
    Sender,
    /// The trader is an order maker, not the sender: either the tx was
    /// discovered via a known solver log (`tx.to` is a rotating filler) or
    /// `tx.to` is a batch settler entered by a solver.
    Maker,
    /// The sender is the trader, but it entered through a venue platform's own contract
    /// (Relay's router, `MetaMask`'s Swap Router) which then calls the solver inside the same
    /// transaction. Decoding is still sender netting, plus that venue's corrections — its fee
    /// skim is backed out, and its contract overhead is excluded from gas accounting — so the
    /// recovered swap is what the venue actually asked the solver for.
    Venue(venues::Venue),
}

/// Classify whose net flow is the trade. Assumes the transaction already matched (see
/// `matching`): an entry point that is neither a venue nor otherwise known can only have
/// matched via a solver log, which is a filler-initiated intent fill.
fn trader_shape(entry_point: Address, registry: &Registry) -> TraderShape {
    if let Some(venue) = registry
        .venue_name(entry_point)
        .and_then(venues::Venue::from_name)
    {
        return TraderShape::Venue(venue);
    }
    // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is an
    // order maker's net flow — decoded like a filler-initiated intent fill.
    if registry.is_batch_settler(entry_point) {
        return TraderShape::Maker;
    }
    if registry.is_known(entry_point) {
        return TraderShape::Sender;
    }
    TraderShape::Maker
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
) -> Option<Flow> {
    transfer_ledger
        .net_swap(sender)
        .map(|swap| Flow { gas_scope: GasScope::Receipt, ..Flow::without_fees(sender, swap) })
        .or_else(|| {
            transfer_ledger
                .net_swap(entry_point)
                .map(|swap| Flow::without_fees(entry_point, swap))
        })
}
