//! Decoding a matched transaction into a trader's flow.
//!
//! One decoder handles one matched transaction. Which decoder runs is chosen by the matched
//! entity: a direct sender, an intent order, or a specific venue. Each entity holds an ordered
//! list of `TradeDecoder`s tried in turn — the first that returns a flow wins, so a later one is
//! the fallback for what the earlier ones cannot decode. That is where an entity picks how its
//! swaps are read, in the order it prefers. Every decoder is constructed once, with the state it
//! needs: a venue's decoders are built at registry load and live on its `Venue` entry; the
//! sender and intent lists are built once per `Decoder` (see [`EntityDecoders`]). The
//! per-transaction path only resolves an address and calls trait objects.
//!
//! What a decoder reads is open — the value movements, the calldata, the event logs, a
//! combination, or a source not needed yet; all of it arrives in the `DecodeContext`, and a
//! decoder takes only what it needs. `netting` is the shared engine that exists today; a method
//! bespoke to one protocol lives in that protocol's module. Everything around decoding — matching,
//! vetoes, attribution, gas, quotes — stays in the orchestrator.

use alloy::{
    network::AnyTransactionReceipt,
    primitives::{Address, U256},
    rpc::types::trace::geth::CallFrame,
};
use async_trait::async_trait;

use crate::decoder::{
    intents,
    netting::SenderNetting,
    registry::{Registry, Venue},
    transfer_ledger::{NetSwap, TransferLedger},
};

/// The one question a decoder may ask beyond the transaction: does this address hold contract
/// code? A port owned by the decode layer, so decoders depend on the question rather than on an
/// RPC client; the RPC-backed adapter — with its cross-block cache — lives with the `Decoder`.
///
/// An implementation that cannot answer must say `true`: treating an unknown address as a
/// contract declines a trade, while treating it as an EOA records a wrong one.
#[async_trait]
pub(crate) trait ContractCode: Send {
    async fn is_contract(&mut self, address: Address) -> bool;
}

/// Decode one matched, traced transaction into the trader's flow, or `None` when this decoder
/// cannot. Async because a decoder may need lookups beyond the transaction (e.g. checking an
/// address for contract code).
#[async_trait]
pub(crate) trait TradeDecoder: Send + Sync {
    /// Label recorded on the trades this decoder produced, so the JSONL records say which decoder
    /// carried each trade (deliberately not a metric label).
    fn name(&self) -> &'static str;

    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow>;

    /// Whether this decoder's flow *is* the calldata-recovered intent, so the orchestrator's
    /// intent-vs-flow disagreement warning has nothing independent to compare it against.
    /// Declared here so the orchestrator never identifies a decoder by its name string.
    fn flow_is_the_intent(&self) -> bool {
        false
    }
}

/// Whose flow a matched transaction carries — the axis that selects the decoders.
#[derive(Clone, Copy)]
pub(crate) enum TraderRole<'a> {
    /// The transaction sender (a direct solver swap).
    Sender,
    /// An intent fill: the sender is a solver or batch settler acting for the swapper.
    Intent,
    /// A venue the sender entered through: its registry entry, carrying its addresses and its
    /// decoders.
    Venue(&'a Venue),
}

impl<'a> TraderRole<'a> {
    /// Classify the role from the entry point. Assumes the transaction already matched (see
    /// `matching`): an entry point that is neither a venue nor otherwise known can only have
    /// matched via a solver log, which is a solver-initiated intent fill.
    pub(crate) fn classify(entry_point: Address, registry: &'a Registry) -> Self {
        if let Some(venue) = registry.venue_for(entry_point) {
            return TraderRole::Venue(venue);
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

/// The decoders for the two entities that are not venues, built once per `Decoder`. A venue's
/// decoders live on its registry entry instead, constructed with that venue's addresses.
pub(crate) struct EntityDecoders {
    sender: Vec<Box<dyn TradeDecoder>>,
    intent: Vec<Box<dyn TradeDecoder>>,
}

impl EntityDecoders {
    pub(crate) fn new() -> Self {
        Self { sender: vec![Box::new(SenderNetting)], intent: intents::decoders() }
    }
}

/// Decode a matched transaction: try its role's decoders in order. Returns the winning decoder
/// with the flow.
pub(crate) async fn recover<'d>(
    role: TraderRole<'d>,
    decoders: &'d EntityDecoders,
    ctx: &mut DecodeContext<'_>,
) -> Option<(&'d dyn TradeDecoder, TraderFlow)> {
    let list = match role {
        TraderRole::Sender => &decoders.sender,
        TraderRole::Intent => &decoders.intent,
        TraderRole::Venue(venue) => &venue.decoders,
    };
    try_decoders(list, ctx).await
}

/// Try each decoder in order; the first flow wins and the rest are not consulted.
async fn try_decoders<'d>(
    decoders: &'d [Box<dyn TradeDecoder>],
    ctx: &mut DecodeContext<'_>,
) -> Option<(&'d dyn TradeDecoder, TraderFlow)> {
    for decoder in decoders {
        if let Some(flow) = decoder.decode(ctx).await {
            return Some((decoder.as_ref(), flow));
        }
    }
    None
}

/// Everything a decoder may read from one matched transaction.
///
/// Every kind of evidence is gathered up front, for every matched transaction, regardless of
/// which decoder wins: the receipt and its logs, the root calldata, and the flattened transfer
/// ledger all arrive here. A decoder that starts needing another input extends this struct.
pub(crate) struct DecodeContext<'a> {
    /// Answers "does this address hold contract code?" (see [`ContractCode`]).
    pub contract_code: &'a mut dyn ContractCode,
    pub registry: &'a Registry,
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
}

/// The trader's side of a matched transaction: the swap, plus the corrections that make it
/// comparable (venue fees backed out).
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
}

impl TraderFlow {
    pub(crate) fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self { tracked, swap, venue_fee_in: None, venue_fee_out: None, solver_override: None }
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
    use crate::decoder::test_utils::{addr, swap, CtxFixture};

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
        let transfer_ledger = TransferLedger::from_transaction(&[], &[]);
        let mut fixture = CtxFixture::new(addr(1), addr(2));
        let mut ctx = fixture.ctx(&registry, &transfer_ledger, &[]);
        try_decoders(&decoders, &mut ctx)
            .await
            .map(|(decoder, flow)| (decoder.name(), flow))
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
