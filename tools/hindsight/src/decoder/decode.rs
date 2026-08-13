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

use std::collections::HashMap;

use alloy::{
    network::AnyTransactionReceipt,
    primitives::{Address, U256},
    providers::DynProvider,
    rpc::types::trace::geth::CallFrame,
};
use async_trait::async_trait;

use crate::decoder::{
    intents,
    netting::SenderNetting,
    registry::{Registry, Venue},
    transfer_ledger::{NetSwap, TransferLedger},
};

/// Decode one matched, traced transaction into the trader's flow, or `None` when this decoder
/// cannot. Async because a decoder may need RPC lookups beyond the transaction (e.g. checking an
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
    /// RPC access, for decoders that must look beyond the transaction.
    pub provider: &'a DynProvider,
    pub registry: &'a Registry,
    /// Cross-block contract-code cache, owned by the decoder.
    pub code_cache: &'a mut HashMap<Address, bool>,
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

/// Which part of the transaction's gas counts as the settled route's cost.
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

/// Derive how the settled route's gas is charged, from facts the role and the decoded flow
/// already establish — decoders do not declare it.
///
/// Gas is charged only when the trader paid it *for this route*: the flow tracks the sender, and
/// the sender actually funded the swap (net-sent the input token — a venue transaction whose
/// sender has no outflow is a solver-initiated rebalance). A direct solver entry then charges the
/// whole transaction; a venue entry charges only the solver call's trace frame, since the venue's
/// own overhead is charged whichever router it picks. Intent fills never charge: the solver sent
/// the transaction and recoups its gas in the order price.
pub(crate) fn gas_scope(
    role: TraderRole<'_>,
    flow: &TraderFlow,
    transfer_ledger: &TransferLedger,
    sender: Address,
) -> GasScope {
    if flow.tracked != sender {
        return GasScope::NotCharged;
    }
    let sender_funded = transfer_ledger
        .group_net_sent(&std::collections::HashSet::from([sender]))
        .contains_key(&flow.swap.token_in);
    if !sender_funded {
        return GasScope::NotCharged;
    }
    match role {
        TraderRole::Sender => GasScope::WholeTransaction,
        TraderRole::Venue(_) => GasScope::SolverFrame,
        TraderRole::Intent => GasScope::NotCharged,
    }
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

    mod gas_scope_rules {
        use alloy::primitives::{address, U256};

        use super::*;
        use crate::decoder::test_utils::make_transfer_log;

        /// 1inch v6 — a `[solvers]` entry, so classify resolves the Sender role.
        const ONEINCH: Address = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        /// Relay's router — a venue entry point, so classify resolves the Venue role.
        const RELAY: Address = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        /// The `CoW` settlement contract — a batch settler, so classify resolves Intent.
        const COW: Address = address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41");

        /// A ledger where `sender` pays `token_in` into a pool.
        fn funded_ledger(sender: Address, token_in: Address) -> TransferLedger {
            let logs = vec![make_transfer_log(token_in, sender, addr(50), U256::from(1000))];
            TransferLedger::from_transaction(&logs, &[])
        }

        fn flow(tracked: Address, token_in: Address) -> TraderFlow {
            TraderFlow::without_fees(
                tracked,
                NetSwap {
                    token_in,
                    amount_in: U256::from(1000),
                    token_out: addr(11),
                    amount_out: U256::from(2000),
                },
            )
        }

        #[test]
        fn test_direct_sender_charges_the_whole_transaction() {
            let registry = Registry::ethereum();
            let sender = addr(1);
            let role = TraderRole::classify(ONEINCH, &registry);
            let ledger = funded_ledger(sender, addr(10));
            assert_eq!(
                gas_scope(role, &flow(sender, addr(10)), &ledger, sender),
                GasScope::WholeTransaction
            );
        }

        #[test]
        fn test_venue_entry_charges_the_solver_frame() {
            let registry = Registry::ethereum();
            let sender = addr(1);
            let role = TraderRole::classify(RELAY, &registry);
            let ledger = funded_ledger(sender, addr(10));
            assert_eq!(
                gas_scope(role, &flow(sender, addr(10)), &ledger, sender),
                GasScope::SolverFrame
            );
        }

        #[test]
        fn test_unfunded_sender_charges_nothing() {
            // The sender never net-sent the input token: a solver-initiated rebalance whose
            // flow still tracks the sender. Charging it would bill the route to a bystander.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let role = TraderRole::classify(RELAY, &registry);
            let ledger = TransferLedger::from_transaction(&[], &[]);
            assert_eq!(
                gas_scope(role, &flow(sender, addr(10)), &ledger, sender),
                GasScope::NotCharged
            );
        }

        #[test]
        fn test_tracked_is_not_the_sender_charges_nothing() {
            let registry = Registry::ethereum();
            let sender = addr(1);
            let role = TraderRole::classify(ONEINCH, &registry);
            let ledger = funded_ledger(sender, addr(10));
            assert_eq!(
                gas_scope(role, &flow(addr(2), addr(10)), &ledger, sender),
                GasScope::NotCharged
            );
        }

        #[test]
        fn test_intent_fill_charges_nothing() {
            // Even a self-settled order charges nothing: the solver recoups settlement gas in
            // the order price, so charging it against the output would double-count.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let role = TraderRole::classify(COW, &registry);
            let ledger = funded_ledger(sender, addr(10));
            assert_eq!(
                gas_scope(role, &flow(sender, addr(10)), &ledger, sender),
                GasScope::NotCharged
            );
        }
    }
}
