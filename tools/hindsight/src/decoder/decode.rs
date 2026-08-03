//! Decoding a matched transaction into a trader's flow.
//!
//! One decoder handles one matched transaction. Which decoder runs is chosen by the matched
//! entity (`decoders_for`): a direct sender, an intent order, or a specific venue. Each entity
//! maps to an ordered list of `TradeDecoder`s tried in turn — the first that returns a flow
//! wins, so a later one is the fallback for what the earlier ones cannot decode. That is where an
//! entity picks how its swaps are read, in the order it prefers.
//!
//! What a decoder reads is open — the value movements, the calldata, the event logs, a
//! combination, or a source not needed yet; all of it arrives in the `DecodeContext`, and a
//! decoder takes only what it needs. `netting` is the shared engine that exists today; a method
//! bespoke to one protocol lives in that protocol's module. Everything around decoding — matching,
//! vetoes, attribution, gas, quotes — stays in the orchestrator.

use std::collections::HashMap;

use alloy::{
    network::AnyTransactionReceipt,
    primitives::{Address, U256},
    rpc::types::trace::geth::CallFrame,
};
use async_trait::async_trait;

use crate::decoder::{
    intents,
    netting_decoders::SenderNetting,
    registry::{Registry, VenueAddresses},
    transfer_ledger::{NetSwap, TransferLedger},
    venues,
};

/// Decode one matched, traced transaction into the trader's flow, or `None` when this decoder
/// cannot. Async for trait-object ergonomics (`Box<dyn TradeDecoder>`); decoding itself issues no
/// RPC — every fact it needs, including the one that used to require a lookup (is a candidate
/// address a contract or an EOA), arrives pre-gathered in the `DecodeContext`.
#[async_trait]
pub(crate) trait TradeDecoder: Send + Sync {
    /// Label recorded on the trades this decoder produced, so the JSONL records say which decoder
    /// carried each trade (deliberately not a metric label).
    fn name(&self) -> &'static str;

    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow>;
}

/// Whose flow a matched transaction carries — the axis that selects the decoders.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TraderRole<'a> {
    /// The transaction sender (a direct solver swap).
    Sender,
    /// An intent fill: the sender is a solver or batch settler acting for the swapper.
    Intent,
    /// A venue the sender entered through, named by its address-book section.
    Venue(&'a str),
}

impl<'a> TraderRole<'a> {
    /// Classify the role from the entry point. Assumes the transaction already matched (see
    /// `matching`): an entry point that is neither a venue nor otherwise known can only have
    /// matched via a solver log, which is a solver-initiated intent fill.
    ///
    /// Pure and RPC-free, so the decoder driver can also call it ahead of decode — to know which
    /// transactions will need `IntentNetting`'s contract-flag prefetch (see
    /// `intents::netting::intent_candidates`) — without duplicating this logic.
    pub(crate) fn classify(entry_point: Address, registry: &'a Registry) -> Self {
        if let Some(name) = registry.venue_name(entry_point) {
            return TraderRole::Venue(name);
        }
        // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is
        // the swapper's net flow — decoded like a solver-initiated intent fill.
        if registry.is_batch_settler(entry_point) {
            return TraderRole::Intent;
        }
        if registry.is_known(entry_point) {
            return TraderRole::Sender;
        }
        TraderRole::Intent
    }
}

/// The decoders tried for a role, in order — the first to return a flow wins. This is the one
/// place the entity → decoder mapping lives: an entity lists its decoders, in the order it wants
/// them tried.
fn decoders_for(role: TraderRole<'_>) -> Vec<Box<dyn TradeDecoder>> {
    match role {
        TraderRole::Sender => vec![Box::new(SenderNetting)],
        TraderRole::Intent => intents::decoders_for(),
        TraderRole::Venue(name) => venues::decoders_for(name),
    }
}

/// Decode a matched transaction: pick the decoders for its role and try them in order. Returns
/// the winning decoder's name with the flow.
pub(crate) async fn recover(ctx: &mut DecodeContext<'_>) -> Option<(&'static str, TraderFlow)> {
    let role = TraderRole::classify(ctx.entry_point, ctx.registry);
    if let TraderRole::Venue(name) = role {
        let registry = ctx.registry;
        ctx.venue = registry.venue(name);
    }
    try_decoders(decoders_for(role), ctx).await
}

/// Try each decoder in order; the first flow wins and the rest are not consulted.
async fn try_decoders(
    decoders: Vec<Box<dyn TradeDecoder>>,
    ctx: &mut DecodeContext<'_>,
) -> Option<(&'static str, TraderFlow)> {
    for decoder in decoders {
        if let Some(flow) = decoder.decode(ctx).await {
            return Some((decoder.name(), flow));
        }
    }
    None
}

/// Everything a decoder may read from one matched transaction.
///
/// Every kind of evidence is gathered up front, for every matched transaction, regardless of
/// which decoder wins: the receipt and its logs, the root calldata, and the flattened transfer
/// ledger all arrive here. A decoder that starts needing another input extends this struct. Decode
/// is a pure function of this context — nothing here is fetched lazily, so no decoder issues RPC
/// of its own.
pub(crate) struct DecodeContext<'a> {
    pub registry: &'a Registry,
    /// Contract-or-EOA facts for this block's intent-fill candidates, gathered by the decoder
    /// driver before decode runs (see `Decoder::prefetch_contract_flags`) — the one fact decoding
    /// used to fetch lazily via RPC (`IntentNetting`'s `eth_getCode` check). An address absent
    /// from the map was never a candidate for this transaction.
    pub contract_flags: &'a HashMap<Address, bool>,
    /// The matched transaction's receipt (sender, logs).
    pub receipt: &'a AnyTransactionReceipt,
    /// The contract the transaction entered through (`tx.to`).
    pub entry_point: Address,
    /// The transaction's flattened value movements.
    pub transfer_ledger: &'a TransferLedger,
    /// The transaction's root calldata. Venues declare their solver in it; some solvers embed
    /// their quote.
    pub input: &'a [u8],
    /// The transaction's root trace frame. A decoder that must find the settling solver's own
    /// call (its calldata, its declared output recipient) walks this itself rather than netting
    /// the ledger — e.g. a packed calldata layout (Fly) only decodes inside its own frame.
    pub root: &'a CallFrame,
    /// The matched venue's address-book section (entry points, fee collectors, solver aliases),
    /// set when the transaction entered through a venue so venue decoders never look themselves
    /// up by name. `None` for direct and intent transactions.
    pub venue: Option<&'a VenueAddresses>,
}

/// Which part of the transaction's gas counts as the settled route's cost. Decided by the
/// decoder — only it knows who sent the transaction and what wraps the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GasScope {
    /// The trader sent the transaction: all of its gas is the route's cost.
    WholeTransaction,
    /// The trade runs inside a venue's contract: only the solver call's trace frame counts,
    /// keeping the venue's own overhead out of the comparison.
    SolverFrame,
    /// Someone other than the trader paid the gas (intent fills, solver rebalances): none of it
    /// is charged.
    NotCharged,
}

/// The trader's side of a matched transaction: the swap, plus the corrections that make it
/// comparable (venue fees backed out, gas scope).
pub(crate) struct TraderFlow {
    /// The address whose net flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Venue fee taken from the input token, already backed out of `swap.amount_in`.
    pub venue_fee_in: Option<U256>,
    /// Venue fee taken from the output token, already added back into `swap.amount_out`.
    pub venue_fee_out: Option<U256>,
    /// Solver label asserted by the decoder itself (e.g. `MetaMask` declares its solver in
    /// calldata), overriding trace-based attribution.
    pub solver_override: Option<String>,
    /// How the settled route's gas is charged against the settled output.
    pub gas_scope: GasScope,
}

impl TraderFlow {
    pub(crate) fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self {
            tracked,
            swap,
            venue_fee_in: None,
            venue_fee_out: None,
            solver_override: None,
            gas_scope: GasScope::NotCharged,
        }
    }

    /// Record `fee` as an output-token venue fee and gross it back into `swap.amount_out`, so the
    /// settled output stays comparable to Fynd's gross re-solve. A no-op when an output fee was
    /// already accounted, so a second matching fee leg cannot double-count.
    pub(crate) fn gross_output_fee(&mut self, fee: U256) {
        if self.venue_fee_out.is_some() {
            return;
        }
        self.venue_fee_out = Some(fee);
        self.swap.amount_out = self.swap.amount_out.saturating_add(fee);
    }

    /// Record `fee` as an input-token venue fee and net it out of `swap.amount_in`, so the settled
    /// input is what actually reached the pools rather than the user's gross spend. A no-op when an
    /// input fee was already accounted (a venue decoder ran first and knows better).
    ///
    /// Without this, a venue skimming its fee off the input makes the settled trade look bigger
    /// than it was, and Fynd — re-solved on that inflated size — appears to beat it.
    pub(crate) fn net_input_fee(&mut self, fee: U256) {
        if self.venue_fee_in.is_some() {
            return;
        }
        self.venue_fee_in = Some(fee);
        self.swap.amount_in = self.swap.amount_in.saturating_sub(fee);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;
    use crate::decoder::test_utils::{addr, frame, receipt, swap, tx_hash};

    /// Always declines.
    struct Declines;

    #[async_trait]
    impl TradeDecoder for Declines {
        fn name(&self) -> &'static str {
            "declines"
        }

        async fn decode(&self, _ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
            None
        }
    }

    /// Always decodes a fixed flow.
    struct Wins;

    #[async_trait]
    impl TradeDecoder for Wins {
        fn name(&self) -> &'static str {
            "wins"
        }

        async fn decode(&self, _ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
            Some(TraderFlow::without_fees(addr(1), swap(addr(10), 1, addr(11), 2)))
        }
    }

    /// Declines, counting how often it was consulted.
    struct CountsCalls(Arc<AtomicUsize>);

    #[async_trait]
    impl TradeDecoder for CountsCalls {
        fn name(&self) -> &'static str {
            "counts"
        }

        async fn decode(&self, _ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }
    }

    async fn try_with(decoders: Vec<Box<dyn TradeDecoder>>) -> Option<(&'static str, TraderFlow)> {
        let registry = Registry::ethereum();
        let contract_flags = HashMap::new();
        let receipt = receipt(tx_hash(1), addr(1), Some(addr(2)), vec![]);
        let transfer_ledger = TransferLedger::from_transaction(&[], &[]);
        let root = frame("CALL", addr(1), addr(2), 0);
        let mut ctx = DecodeContext {
            registry: &registry,
            contract_flags: &contract_flags,
            receipt: &receipt,
            entry_point: addr(2),
            transfer_ledger: &transfer_ledger,
            input: &[],
            root: &root,
            venue: None,
        };
        try_decoders(decoders, &mut ctx).await
    }

    #[tokio::test]
    async fn test_first_decoder_declines() {
        let (name, flow) = try_with(vec![Box::new(Declines), Box::new(Wins)])
            .await
            .unwrap();
        assert_eq!(name, "wins");
        assert_eq!(flow.tracked, addr(1));
    }

    #[tokio::test]
    async fn test_first_decoder_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (name, _) = try_with(vec![Box::new(Wins), Box::new(CountsCalls(Arc::clone(&calls)))])
            .await
            .unwrap();
        assert_eq!(name, "wins");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_no_decoder_answers() {
        assert!(try_with(vec![Box::new(Declines)])
            .await
            .is_none());
    }
}
