//! How a matched transaction's swap is recovered.
//!
//! [`select`] decides *which* transactions are solver trades and where their trader sits
//! ([`TraderStrategy`]); [`TraderStrategy::decode`] is the single seam the orchestrator calls.
//! This module is tier-neutral — venue-specific behavior lives in `venues/`, solver-specific
//! knowledge in `solvers/`, and maker-finding for intent fills in `intent`.

use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::TransactionReceipt,
};
use tracing::debug;

use crate::decoder::{
    intent,
    ledger::{NetSwap, TransferLedger},
    registry::Registry,
    solvers, venues,
};

/// Everything a decode strategy may need from one matched transaction, so every strategy is
/// called through the same seam ([`TraderStrategy::decode`]) regardless of which inputs it uses.
pub(crate) struct DecodeContext<'a, P> {
    /// RPC access, for strategies that must look beyond the transaction (maker EOA checks).
    pub provider: &'a P,
    pub registry: &'a Registry,
    /// Cross-block contract-code cache, owned by the decoder.
    pub code_cache: &'a mut HashMap<Address, bool>,
    /// The transaction's flattened value movements.
    pub ledger: &'a TransferLedger,
    /// Root calldata of the transaction (venue declarations, embedded quotes).
    pub input: &'a [u8],
    pub sender: Address,
    pub entry_point: Address,
}

/// Where the trader sits in a matched transaction, and therefore how to recover its swap.
pub(crate) enum TraderStrategy {
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

impl TraderStrategy {
    /// Recover the user flow from a matched transaction. Each variant owns its trader shape;
    /// the orchestrator only sequences.
    pub(crate) async fn decode<P: Provider>(&self, ctx: DecodeContext<'_, P>) -> Option<Flow> {
        match self {
            Self::Sender => sender_flow(ctx.ledger, ctx.sender, ctx.entry_point),
            Self::Maker => {
                intent::find_maker_trade(
                    ctx.provider,
                    ctx.ledger,
                    &[ctx.entry_point, ctx.sender],
                    ctx.registry,
                    ctx.code_cache,
                )
                .await
            }
            Self::Venue(venue) => venue.decode(&venues::VenueContext {
                ledger: ctx.ledger,
                sender: ctx.sender,
                entry_point: ctx.entry_point,
                input: ctx.input,
                registry: ctx.registry,
            }),
        }
    }

    /// Whether the trade runs inside a venue's own contract (see [`TraderStrategy::Venue`]).
    /// The receipt's gas then includes the venue's overhead — charged whichever solver the
    /// venue picks — so gas accounting reads the solver call's trace frame instead of the
    /// whole receipt.
    pub(crate) fn routes_via_wrapper(&self) -> bool {
        match self {
            Self::Venue(_) => true,
            Self::Sender | Self::Maker => false,
        }
    }
}

/// A matched transaction and the strategy to decode it.
pub(crate) struct Matched<'a> {
    pub receipt: &'a TransactionReceipt,
    pub entry_point: Address,
    pub strategy: TraderStrategy,
}

/// The decoded user flow of a matched transaction.
pub(crate) struct Flow {
    /// The address whose net flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Venue fee skimmed from the input token, already backed out of `swap.amount_in`.
    pub venue_fee: Option<U256>,
    /// Venue fee skimmed from the output token, already added back into `swap.amount_out`.
    pub venue_fee_out: Option<U256>,
    /// Solver label asserted by the strategy itself (e.g. `MetaMask` declares its
    /// solver in calldata), overriding trace-based attribution.
    pub solver_override: Option<String>,
    /// Whether the tracked trader sent the transaction and therefore paid its gas. Decides if the
    /// settled route's gas may be charged against the settled output — a maker or a
    /// solver-rebalance trader had its gas paid by someone else, so nothing is deducted there.
    pub trader_paid_gas: bool,
}

impl Flow {
    pub(crate) fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self {
            tracked,
            swap,
            venue_fee: None,
            venue_fee_out: None,
            solver_override: None,
            trader_paid_gas: false,
        }
    }
}

/// Match a receipt and choose its decode strategy.
///
/// A transaction qualifies two ways: its entry point (`tx.to`) is a known
/// venue or solver, or one of its logs was emitted by a known solver
/// (filler-initiated intent fills, where `tx.to` is a rotating filler).
/// Matched transactions whose logs mark a non-swap order shape are vetoed
/// here (see [`solvers::match_veto`]), before they cost a trace.
pub(crate) fn select<'a>(
    receipt: &'a TransactionReceipt,
    registry: &Registry,
) -> Option<Matched<'a>> {
    let matched = match_entry(receipt, registry)?;
    if let Some(reason) = solvers::match_veto(matched.receipt.logs()) {
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

/// The strategy for a receipt's entry point, before any veto.
fn match_entry<'a>(receipt: &'a TransactionReceipt, registry: &Registry) -> Option<Matched<'a>> {
    if !receipt.status() {
        return None;
    }
    let entry_point = receipt.to?;
    if let Some(venue) = registry
        .venue_name(entry_point)
        .and_then(venues::Venue::from_name)
    {
        return Some(Matched { receipt, entry_point, strategy: TraderStrategy::Venue(venue) });
    }
    if registry.is_known(entry_point) {
        // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is
        // an order maker's net flow — decode it like a filler-initiated intent fill.
        let strategy = if registry.is_batch_settler(entry_point) {
            TraderStrategy::Maker
        } else {
            TraderStrategy::Sender
        };
        return Some(Matched { receipt, entry_point, strategy });
    }
    let via_log = receipt
        .logs()
        .iter()
        .any(|log| registry.is_solver(log.address()));
    via_log.then_some(Matched { receipt, entry_point, strategy: TraderStrategy::Maker })
}

/// Net the sender's flow, falling back to the entry point for the rare case
/// where output is delivered there.
///
/// A sender-tracked flow marks the trader as the gas payer; the entry-point fallback does not
/// (the tracked address and the gas-paying sender differ, so charging the gas is not clear-cut).
pub(crate) fn sender_flow(
    ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<Flow> {
    ledger
        .net_swap(sender)
        .map(|swap| Flow { trader_paid_gas: true, ..Flow::without_fees(sender, swap) })
        .or_else(|| {
            ledger
                .net_swap(entry_point)
                .map(|swap| Flow::without_fees(entry_point, swap))
        })
}
