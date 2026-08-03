//! Decode solver trades from on-chain data.
//!
//! Terminology — three tiers, two of which appear in every record:
//! - **venue** (`venues/`): the contract the user entered through (`tx.to`) — Relay, `MetaMask`.
//!   Order-flow owners; they pick a solver and may take a fee.
//! - **solver** (`solvers/`): the router that computed and settled the route — `KyberSwap`, 1inch,
//!   0x. These are Fynd's competitors. Datasets recorded before run6 call this tier `aggregator` in
//!   their column names; the two words mean the same thing.
//! - **liquidity venues**: the pools and makers a route executes against (Uniswap, Curve,
//!   prop-AMMs). Not modeled here; they only appear inside traces.
//!
//! The pipeline is match → trace → decode → veto → record: `matching` filters a block down to
//! solver trades, `decode` recovers each trade's swap (picking the decoders for the matched
//! entity), `transfer_ledger` answers all value-flow questions, `veto` rejects shapes that are not
//! comparable trades, and `registry` is the address book behind matching.

mod decode;
mod intents;
mod matching;
mod netting_decoders;
mod registry;
mod sandwich;
mod solvers;
mod trace;
mod transfer_ledger;
mod venue_attribution;
pub(crate) mod venues;
mod veto;

#[cfg(test)]
mod test_utils;

use std::collections::HashMap;

use alloy::{
    eips::BlockId,
    network::AnyTransactionReceipt,
    primitives::{Address, TxHash, U256},
    providers::Provider,
    rpc::types::trace::geth::CallFrame,
};
use anyhow::Context;
use futures::stream::StreamExt;
use tracing::{debug, warn};

use crate::decoder::{
    decode::{recover, DecodeContext, GasScope, TraderFlow},
    matching::MatchedSolverTrade,
    trace::{collect_native_transfers, fetch_trace, route_gas},
    transfer_ledger::TransferLedger,
};
pub(crate) use crate::decoder::{
    registry::Registry,
    sandwich::SandwichEvidence,
    solvers::{attribution::AttributionSource, SwapIntent},
    trace::RevertCause,
};

/// Whether a matched transaction settled or reverted on-chain. Serialized flattened into
/// `DecodedTrade`'s own JSON object (`"status":"settled"`, or `"status":"reverted"` plus
/// `"cause"`), so a settled and a reverted trade read as the same record shape, told apart by
/// this one field rather than by which fields happen to be present.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum TradeStatus {
    Settled,
    Reverted { cause: RevertCause },
}

/// A decoded solver trade: what token went in, what came out — settled or reverted, told apart by
/// `status`. Both are trades in the same sense: a trader routed through a solver; one just did
/// not fill.
///
/// Native ETH is represented as `Address::ZERO`.
///
/// # The settled/reverted invariant
///
/// A settled trade always carries `token_in`/`token_out`/`amount_in`/`amount_out`:
/// `Decoder::decode_settled` only ever returns `Some(DecodedTrade)` once a `TraderFlow` decoder
/// has recovered a real swap, and never leaves them empty. A reverted trade carries
/// `token_in`/`token_out`/`amount_in` only when its solver frame's calldata parsed into a
/// `SwapIntent` — a revert emits no logs, so calldata is the only source; when it did not parse,
/// all three are `None`. The trade is still recorded (not filtered out) so parser coverage stays
/// measurable against every reverted candidate. `amount_out` is settled-only: nothing was
/// delivered on a revert, by definition.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DecodedTrade {
    pub tx_hash: TxHash,
    pub block_number: u64,
    /// The transaction's position in its block, from the receipt (falls back to its position in
    /// the fetched receipt slice when the RPC omitted it).
    pub tx_index: u64,
    #[serde(flatten)]
    pub status: TradeStatus,
    pub venue: String,
    pub solver: String,
    /// The evidence tier the solver label came from (see `solvers::attribution`). Downstream
    /// analysis weighs low-trust tiers (`largest_call`, fallback) differently — e.g. when judging
    /// an embedded quote.
    pub solver_source: AttributionSource,
    /// Which decoder recovered this trade (see `decode`), or `"reverted"` when nothing settled to
    /// hand to a `TradeDecoder` — a reverted trade's terms, when recoverable, come straight from
    /// the solver frame's own calldata, not the netting/calldata decoder chain. Once several
    /// decoders can carry a venue's trades this measures how often each one carries a trade the
    /// others could not.
    pub decoder: &'static str,
    pub sender: Address,
    /// `None` only for a reverted trade whose solver frame's calldata did not parse — see the
    /// settled/reverted invariant above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_in: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_out: Option<Address>,
    /// Input amount that actually entered the swap — a venue fee taken from the input (see
    /// `venue_fee_in`) is already subtracted, so a re-solve compares like-for-like. For a
    /// reverted trade this is the calldata-declared amount, not a netted one (see the
    /// settled/reverted invariant above).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_in: Option<U256>,
    /// Gross swap output — a venue fee taken from the output (see `venue_fee_out`) is added
    /// back, so the settled amount is the full swap proceeds, comparable to Fynd's gross output.
    /// `None` for a reverted trade: nothing was delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_out: Option<U256>,
    /// Venue fee taken from the input token before swapping (e.g. Relay's fee), in `token_in`
    /// units. `None` when no known fee collector took a cut, or the trade reverted. Recorded for
    /// transparency; it is already excluded from `amount_in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_fee_in: Option<U256>,
    /// Venue fee taken from the output token after swapping, in `token_out` units. `None` when
    /// no known fee collector took a cut, or the trade reverted. Recorded for transparency; it is
    /// already added back into `amount_out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_fee_out: Option<U256>,
    /// Wei cost of the gas the trader paid for the settled route (`gas_used ×
    /// effective_gas_price`). For venue-wrapped entries (Relay, `MetaMask`) the venue's own
    /// overhead is excluded — it is charged whichever router the venue picks, like the venue
    /// fee. `None` when the trader did not pay the transaction's gas (intent fills, solver
    /// rebalances), the route's gas could not be isolated from the trace, or the trade reverted
    /// (nothing settled to charge gas against).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_gas: Option<U256>,
    /// The on-chain enforced floor declared in the settling solver frame's own calldata (see
    /// `solvers::swap_intent` for the solvers that declare one). A settled trade cleared this by
    /// construction; it is recorded so avoidance analysis has the same field on both settled and
    /// reverted trades. `None` when no solver frame was found or its calldata did not parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_amount_out: Option<U256>,
    /// The solver's own off-chain quote, when its calldata declares one (unit-checked against
    /// the settled amount; a quote that fails the check is dropped, keeping `min_amount_out`).
    /// This is calldata-declared and self-reported — distinct from `amount_out`, which is what
    /// the settlement ledger actually delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_quote: Option<U256>,
    /// Unix timestamp of `declared_quote`, when the calldata carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_timestamp: Option<u64>,
    /// Evidence that a front-run and a back-run bracketed this trade (see
    /// `sandwich::detect`). `None` when no bracket pair was found, or the trade reverted —
    /// nothing settled to be sandwiched around.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandwich: Option<SandwichEvidence>,
}

impl DecodedTrade {
    /// This trade's swap terms, when known — always `Some` for a settled trade (see the
    /// settled/reverted invariant above), and for a reverted trade only when its solver frame's
    /// calldata parsed. `resolve::resolve_block_range` uses this to decide whether a trade can be
    /// solved at all: a trade with unknown terms is recorded but not re-solved.
    pub(crate) fn terms(&self) -> Option<(Address, Address, U256)> {
        Some((self.token_in?, self.token_out?, self.amount_in?))
    }
}

/// Log a disagreement between the calldata-recovered intent and the netted flow, on any of the
/// three terms they both claim. The ledger stays authoritative for what settled; two
/// independently-derived readings landing on different terms is diagnostic signal we would
/// otherwise lose, not a decode failure. Skipped for `relay-calldata`, whose flow already IS the
/// intent, so there is nothing independent to disagree with.
fn warn_on_intent_disagreement(
    decoder: &str,
    tx_hash: TxHash,
    intent: Option<&SwapIntent>,
    flow: &TraderFlow,
) {
    let Some(intent) = intent.filter(|_| decoder != "relay-calldata") else {
        return;
    };
    if intent.token_in == flow.swap.token_in &&
        intent.token_out == flow.swap.token_out &&
        intent.amount_in == flow.swap.amount_in
    {
        return;
    }
    warn!(
        tx = %tx_hash,
        intent_token_in = %intent.token_in,
        intent_token_out = %intent.token_out,
        intent_amount_in = %intent.amount_in,
        flow_token_in = %flow.swap.token_in,
        flow_token_out = %flow.swap.token_out,
        flow_amount_in = %flow.swap.amount_in,
        "calldata-recovered intent disagrees with the netted flow"
    );
}

/// Copy the calldata-declared terms off a parsed intent, or all-`None` when no intent was
/// recovered.
fn intent_fields(intent: Option<&SwapIntent>) -> (Option<U256>, Option<U256>, Option<u64>) {
    let min_amount_out = intent.map(|intent| intent.min_amount_out);
    let declared_quote = intent.and_then(SwapIntent::declared_quote);
    let quote_timestamp = intent.and_then(|intent| intent.timestamp);
    (min_amount_out, declared_quote, quote_timestamp)
}

/// Max concurrent trace requests per block. Bounds RPC load so a block
/// with many solver trades still completes within the block time
/// without tripping provider rate limits.
const TRACE_CONCURRENCY: usize = 10;

/// Stateful trade decoder: owns the RPC provider, the chain's address
/// registry, and the caches that are worth keeping across blocks.
pub(crate) struct Decoder<P> {
    provider: P,
    registry: Registry,
    /// Whether an address had contract code when first checked, kept for the
    /// life of the decoder. An address gaining code mid-run (a deploy or an
    /// EIP-7702 delegation) keeps its stale answer until restart — acceptable
    /// for distinguishing swapper EOAs from pools.
    code_cache: HashMap<Address, bool>,
}

impl<P: Provider> Decoder<P> {
    pub(crate) fn new(provider: P, registry: Registry) -> Self {
        Self { provider, registry, code_cache: HashMap::new() }
    }

    /// The decoder's RPC provider, for adjacent lookups (chain head, token
    /// metadata) that should share its connection.
    pub(crate) fn provider(&self) -> &P {
        &self.provider
    }

    /// The decoder's address registry, for the label names shared with telemetry.
    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Decode solver trades from a block — settled and reverted alike, as one list told apart by
    /// `DecodedTrade::status`.
    ///
    /// Fetches all receipts in one `eth_getBlockReceipts` call, then matches each transaction one
    /// of three ways: a settled trade, matched by entry point or by a known solver's log (the log
    /// path catches filler-initiated intent fills, `UniswapX`, 1inch limit orders, where `tx.to`
    /// is a rotating filler); a reverted candidate, matched by entry point alone (a revert emits
    /// no logs — see `matching::select`); or neither, and the transaction is dropped before it
    /// costs a trace. Both matched shapes join one bounded trace wave; the trace recovers native
    /// ETH flows and attributes the settling (or attempted) solver either way.
    pub(crate) async fn decode_block(
        &mut self,
        block_number: u64,
    ) -> anyhow::Result<Vec<DecodedTrade>> {
        // Fetch receipts as `AnyTransactionReceipt` rather than the Ethereum-typed default:
        // OP-stack chains (Base) put a system deposit transaction (type `0x7e`) first in
        // every block, which the Ethereum receipt enum rejects — failing the whole
        // `eth_getBlockReceipts` batch. The `Any` receipt tolerates unknown transaction
        // types.
        let receipts: Vec<AnyTransactionReceipt> = self
            .provider
            .raw_request::<_, Option<Vec<AnyTransactionReceipt>>>(
                "eth_getBlockReceipts".into(),
                (BlockId::from(block_number),),
            )
            .await
            .with_context(|| format!("failed to fetch receipts for block {block_number}"))?
            .ok_or_else(|| anyhow::anyhow!("block {block_number} not found"))?;

        // Paired with each receipt's position in the slice, since that position — not the
        // transaction_index field, which the RPC may omit — is what "neighbor" means for the
        // sandwich scan below: receipts are already in block order.
        let matched: Vec<(usize, MatchedSolverTrade)> = receipts
            .iter()
            .enumerate()
            .filter_map(|(index, receipt)| {
                matching::select(receipt, &self.registry).map(|matched| (index, matched))
            })
            .collect();

        // Per-block batch: trace every matched tx concurrently (bounded),
        // collected in block order for deterministic output. Wall-clock cost is
        // one receipts call plus the slowest trace wave — not the sum of every
        // request — so a block stays well inside its block time.
        //
        // Failures are collected per transaction rather than aborting the wave: one transaction the
        // RPC cannot trace costs that trade, not the whole block. Failing the block instead drops
        // its every trade from the aggregates, and the surviving sample is selected by which
        // transactions the RPC happened to serve.
        let traces = futures::stream::iter(
            matched
                .iter()
                .map(|(_, m)| fetch_trace(&self.provider, m.receipt.transaction_hash)),
        )
        .buffered(TRACE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

        let mut trades = Vec::new();
        for ((index, matched), trace) in matched.into_iter().zip(traces) {
            let tx_index = matched
                .receipt
                .transaction_index
                .unwrap_or(index as u64);
            let tx_hash = matched.receipt.transaction_hash;
            let root = match trace {
                Ok(root) => root,
                Err(e) => {
                    warn!(block = block_number, tx = %tx_hash, "skipping untraceable transaction: {e}");
                    crate::telemetry::record_untraced_transaction();
                    continue;
                }
            };
            if let Some(mut trade) = self
                .decode_transaction(matched, &root, block_number, tx_index)
                .await
            {
                // Sandwich detection only makes sense around a trade that actually moved the
                // pools; a reverted trade has nothing to be sandwiched around.
                if trade.status == TradeStatus::Settled {
                    trade.sandwich = sandwich::detect(&receipts, index, &trade, &self.registry);
                }
                trades.push(trade);
            }
        }
        Ok(trades)
    }

    /// Decode one matched transaction: settled trades run the full `TraderFlow` decoder chain
    /// (`decode_settled`); reverted candidates skip it — there is no netted flow to decode, only
    /// the settling solver frame's own calldata to read (`decode_reverted`).
    async fn decode_transaction(
        &mut self,
        matched: MatchedSolverTrade<'_>,
        root: &CallFrame,
        block_number: u64,
        tx_index: u64,
    ) -> Option<DecodedTrade> {
        if matched.reverted {
            return Some(self.decode_reverted(matched, root, block_number, tx_index));
        }
        self.decode_settled(matched, root, block_number, tx_index)
            .await
    }

    /// Decode a reverted candidate from its trace: attribute the solver the same way a settled
    /// trade would be (the strict-then-tolerant `find_solver_frame` walk falls back to the frame
    /// that tried, since nothing settled), recover its swap terms when that solver's calldata
    /// supports it, and classify why the transaction failed. The terms are `None` — not an
    /// error — when no solver frame was found, its solver has no `swap_intent` support, or the
    /// calldata did not parse; the trade is still recorded so parser coverage is measurable
    /// against every reverted candidate. Always produces a trade: unlike a settled decode, there
    /// is no veto or missing-flow case to decline on.
    fn decode_reverted(
        &self,
        matched: MatchedSolverTrade<'_>,
        root: &CallFrame,
        block_number: u64,
        tx_index: u64,
    ) -> DecodedTrade {
        let registry = &self.registry;
        let MatchedSolverTrade { receipt, entry_point, .. } = matched;
        let sender = receipt.from;
        let venue = registry.label(entry_point);
        let attribution =
            solvers::attribution::attribute(None, root, entry_point, sender, registry);
        // A reverted trade has no netted flow to draw an input-amount hint from — only the
        // ABI/offset-based extractors (Fly, KyberSwap) can recover an intent here.
        let intent = trace::find_solver_frame(root, registry)
            .and_then(|frame| solvers::swap_intent(&attribution.solver, &frame.input, None));
        let (min_amount_out, declared_quote, quote_timestamp) = intent_fields(intent.as_ref());
        let (token_in, token_out, amount_in) = match &intent {
            Some(intent) => (Some(intent.token_in), Some(intent.token_out), Some(intent.amount_in)),
            None => (None, None, None),
        };
        DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            tx_index,
            status: TradeStatus::Reverted { cause: trace::classify_revert_cause(root) },
            venue,
            solver: attribution.solver,
            solver_source: attribution.source,
            decoder: "reverted",
            sender,
            token_in,
            token_out,
            amount_in,
            amount_out: None,
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out,
            declared_quote,
            quote_timestamp,
            sandwich: None,
        }
    }

    /// Decode one settled transaction from its trace: build the transfer ledger, run the
    /// decoders for its entity, veto non-trades, attribute the solver, and account gas and quote.
    async fn decode_settled(
        &mut self,
        matched: MatchedSolverTrade<'_>,
        root: &CallFrame,
        block_number: u64,
        tx_index: u64,
    ) -> Option<DecodedTrade> {
        let Self { provider, registry, code_cache } = self;
        let MatchedSolverTrade { receipt, entry_point, .. } = matched;
        let logs = receipt.logs();
        let sender = receipt.from;

        let mut native = Vec::new();
        collect_native_transfers(root, &mut native);
        let transfer_ledger = TransferLedger::from_transaction(logs, &native);

        let mut ctx = DecodeContext {
            provider,
            registry,
            code_cache,
            receipt,
            entry_point,
            transfer_ledger: &transfer_ledger,
            input: &root.input,
            root,
            venue: None,
        };
        let Some((decoder, mut flow)) = recover(&mut ctx).await else {
            warn!(
                tx = %receipt.transaction_hash,
                venue = %registry.label(entry_point),
                "no decoder recovered a trade from this transaction"
            );
            return None;
        };

        if let Some(veto) = veto::check(&flow, logs, registry) {
            debug!(
                tx = %receipt.transaction_hash,
                venue = %registry.label(entry_point),
                ?veto,
                "decoded flow is not a comparable trade; skipping"
            );
            return None;
        }

        // A venue fingerprint (owning trader, CoW appData tag, fee wallet, or integrator tag — see
        // `venue_attribution`) overrides the entry-point label, backing any venue fee out before
        // the quote check reads the grossed output. The appData tag is read from a batch settler's
        // calldata; other transactions carry none.
        let integrator = solvers::integrator(logs);
        let app_data = intents::venue_tag(registry, entry_point, &root.input);
        let venue = venue_attribution::attribute(
            registry,
            &mut flow,
            &transfer_ledger,
            integrator.as_deref(),
            app_data,
        )
        .unwrap_or_else(|| registry.label(entry_point));

        let attribution = solvers::attribution::attribute(
            flow.solver_override.take(),
            root,
            entry_point,
            sender,
            registry,
        );

        // Gas the trader paid for the settled route, as a wei cost. The flow's gas scope says
        // which gas that is — see `GasScope`.
        let settled_gas = match flow.gas_scope {
            GasScope::WholeTransaction => Some(U256::from(receipt.gas_used)),
            GasScope::SolverFrame => route_gas(root, registry),
            GasScope::NotCharged => None,
        }
        .map(|units| units * U256::from(receipt.effective_gas_price));

        // The trader's swap terms, when the settling solver frame's own calldata declares them.
        // Dispatched with the solver frame's input, not the root transaction's — a packed
        // calldata layout (Fly) uses offsets valid only in its own frame — and with the decoded
        // flow's input amount as a hint for scan-based extractors (ParaSwap). Only the netting
        // amounts above stay authoritative for what actually settled; this is informational. A
        // declared quote that fails the unit-plausibility check against the settled amount is
        // dropped (quotes are self-reported); the ABI-decoded terms stay either way.
        let intent = trace::find_solver_frame(root, registry)
            .and_then(|frame| {
                solvers::swap_intent(&attribution.solver, &frame.input, Some(flow.swap.amount_in))
            })
            .map(|mut intent| {
                if let Some(quoted) = intent.declared_quote() {
                    if !solvers::plausible_quote(quoted, flow.swap.amount_out) {
                        intent.clear_quote();
                    }
                }
                intent
            });

        warn_on_intent_disagreement(decoder, receipt.transaction_hash, intent.as_ref(), &flow);
        let (min_amount_out, declared_quote, quote_timestamp) = intent_fields(intent.as_ref());

        Some(DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            tx_index,
            status: TradeStatus::Settled,
            venue,
            solver: attribution.solver,
            solver_source: attribution.source,
            decoder,
            sender: flow.tracked,
            token_in: Some(flow.swap.token_in),
            token_out: Some(flow.swap.token_out),
            amount_in: Some(flow.swap.amount_in),
            amount_out: Some(flow.swap.amount_out),
            venue_fee_in: flow.venue_fee_in,
            venue_fee_out: flow.venue_fee_out,
            settled_gas,
            min_amount_out,
            declared_quote,
            quote_timestamp,
            // The full receipts slice isn't available here (this fn only sees the one matched
            // transaction); the caller (`decode_block`) fills this in once decoding succeeds.
            sandwich: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{address, U256},
        providers::{mock::Asserter, ProviderBuilder},
    };

    use super::*;
    use crate::decoder::test_utils::{addr, frame, make_transfer_log, receipt, tx_hash};

    /// 1inch v6 — a `[solvers]` entry in the ethereum address book, so a transaction into it
    /// matches on its entry point alone.
    const ONEINCH: Address = address!("0x111111125421ca6dc452d289314280a0f8842a65");

    /// A sender-netting swap through `ONEINCH`: `sender` pays one token and is paid another.
    fn swap_receipt(hash: TxHash, sender: Address) -> AnyTransactionReceipt {
        let pool = addr(0x50);
        receipt(
            hash,
            sender,
            Some(ONEINCH),
            vec![
                make_transfer_log(addr(0xaa), sender, pool, U256::from(1_000)),
                make_transfer_log(addr(0xbb), pool, sender, U256::from(2_000)),
            ],
        )
    }

    #[tokio::test]
    async fn test_untraceable_transaction_does_not_drop_the_block() {
        let asserter = Asserter::new();
        asserter.push_success(&vec![
            swap_receipt(tx_hash(1), addr(1)),
            swap_receipt(tx_hash(2), addr(2)),
        ]);
        asserter.push_failure_msg("debug_traceTransaction unavailable");
        asserter.push_success(&frame("CALL", addr(2), ONEINCH, 0));

        let mut decoder = Decoder::new(
            ProviderBuilder::default().connect_mocked_client(asserter),
            Registry::ethereum(),
        );
        let trades = decoder
            .decode_block(21_000_000)
            .await
            .expect("an untraceable transaction must not fail the block");

        // Only the untraceable transaction is lost; before, it took the whole block with it.
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].tx_hash, tx_hash(2));
    }

    /// A Relay transaction that reverted before settling, decoded end to end through
    /// `decode_block`: matched by entry point alone, attributed via the trace's tolerant
    /// fallback, and recorded with no swap terms (an unregistered "solver" frame, so its calldata
    /// carries no `swap_intent`).
    #[tokio::test]
    async fn test_reverted_relay_transaction_decodes_with_a_reverted_status() {
        let registry = Registry::ethereum();
        let relay = *registry
            .venue("relay")
            .unwrap()
            .entry_points
            .iter()
            .next()
            .unwrap();
        let sender = addr(1);

        let asserter = Asserter::new();
        asserter.push_success(&vec![crate::decoder::test_utils::reverted_receipt(
            tx_hash(1),
            sender,
            Some(relay),
        )]);
        let mut root = frame("CALL", sender, relay, 0);
        root.error = Some("execution reverted".to_string());

        asserter.push_success(&root);

        let mut decoder =
            Decoder::new(ProviderBuilder::default().connect_mocked_client(asserter), registry);
        let trades = decoder
            .decode_block(21_000_000)
            .await
            .expect("decode_block should succeed");

        assert_eq!(trades.len(), 1);
        let trade = &trades[0];
        assert!(matches!(trade.status, TradeStatus::Reverted { .. }));
        assert_eq!(trade.venue, "relay");
        assert!(trade.token_in.is_none());
        assert!(trade.amount_out.is_none());
        assert!(trade.sandwich.is_none());
    }
}
