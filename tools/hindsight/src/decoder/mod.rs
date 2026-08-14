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
mod netting;
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
    providers::{DynProvider, Provider},
    rpc::types::trace::geth::CallFrame,
};
use anyhow::Context;
use async_trait::async_trait;
use futures::stream::StreamExt;
use tracing::{debug, warn};

use crate::decoder::{
    decode::{recover, ContractCode, DecodeContext, EntityDecoders, TraderFlow, TraderRole},
    matching::MatchedSolverTrade,
    solvers::{DeclaredSwap, SolverKnowledge},
    trace::{collect_native_transfers, fetch_trace},
    transfer_ledger::TransferLedger,
};
pub(crate) use crate::decoder::{
    registry::Registry, sandwich::SandwichEvidence, solvers::attribution::AttributionSource,
};

/// A decoded solver trade: what token went in, what came out.
///
/// Native ETH is represented as `Address::ZERO`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DecodedTrade {
    pub tx_hash: TxHash,
    pub block_number: u64,
    /// The transaction's position in its block, from the receipt (falls back to its position in
    /// the fetched receipt slice when the RPC omitted it).
    pub tx_index: u64,
    pub venue: String,
    pub solver: String,
    /// The evidence tier the solver label came from (see `solvers::attribution`). Downstream
    /// analysis weighs low-trust tiers (`largest_call`, fallback) differently — e.g. when judging
    /// an embedded quote.
    pub solver_source: AttributionSource,
    /// Which decoder recovered this trade (see `decode`). Once several decoders can carry a
    /// venue's trades this measures how often each one carries a trade the others could not.
    pub decoder: &'static str,
    pub sender: Address,
    pub token_in: Address,
    pub token_out: Address,
    /// Input amount that actually entered the swap — a venue fee taken from the input (see
    /// `venue_fee_in`) is already subtracted, so a re-solve compares like-for-like.
    pub amount_in: U256,
    /// Gross swap output — a venue fee taken from the output (see `venue_fee_out`) is added
    /// back, so the settled amount is the full swap proceeds, comparable to Fynd's gross output.
    pub amount_out: U256,
    /// Venue fee taken from the input token before swapping (e.g. Relay's fee), in `token_in`
    /// units. `None` when no known fee collector took a cut. Recorded for transparency; it is
    /// already excluded from `amount_in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_fee_in: Option<U256>,
    /// Venue fee taken from the output token after swapping, in `token_out` units. `None` when
    /// no known fee collector took a cut. Recorded for transparency; it is already added back into
    /// `amount_out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_fee_out: Option<U256>,
    /// The on-chain enforced floor declared in the settling solver frame's own calldata (see
    /// `solvers::declared_swap` for the solvers that declare one). A settled trade cleared this by
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
    /// `sandwich::detect`). `None` when no bracket pair was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandwich: Option<SandwichEvidence>,
}

/// Log a disagreement between the calldata-recovered intent and the netted flow, on any of the
/// three terms they both claim. The ledger stays authoritative for what settled; two
/// independently-derived readings landing on different terms is diagnostic signal we would
/// otherwise lose, not a decode failure. Skipped when the winning decoder's flow already IS the
/// intent (`TradeDecoder::flow_is_the_declared_swap`), since there is nothing independent to
/// disagree with.
fn warn_on_declaration_disagreement(
    flow_is_the_declared_swap: bool,
    tx_hash: TxHash,
    intent: Option<&DeclaredSwap>,
    flow: &TraderFlow,
) {
    let Some(intent) = intent.filter(|_| !flow_is_the_declared_swap) else {
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

/// The trader's swap terms, when the settling solver frame's own calldata declares them.
/// Dispatched with the solver frame's input, not the root transaction's — a packed calldata
/// layout (Fly) uses offsets valid only in its own frame — and with the decoded flow's input
/// amount as a hint for scan-based extractors (`ParaSwap`). Only the netted amounts stay
/// authoritative for what actually settled; this is informational. A declared quote that fails
/// the unit-plausibility check against the settled amount is dropped (quotes are self-reported);
/// the ABI-decoded terms stay either way.
fn recover_declared_swap(
    knowledge: &dyn SolverKnowledge,
    root: &CallFrame,
    registry: &Registry,
    flow: &TraderFlow,
) -> Option<DeclaredSwap> {
    let intent = trace::find_solver_frame(root, registry)
        .and_then(|frame| knowledge.declared_swap(&frame.input, Some(flow.swap.amount_in)))?;
    let mut intent = intent;
    if let Some(quoted) = intent.declared_quote() {
        if !solvers::plausible_quote(quoted, flow.swap.amount_out) {
            intent.clear_quote();
        }
    }
    Some(intent)
}

/// Copy the calldata-declared terms off a parsed intent, or all-`None` when no intent was
/// recovered. Split out of `decode_transaction` purely to keep it under the line limit.
fn declaration_fields(intent: Option<&DeclaredSwap>) -> (Option<U256>, Option<U256>, Option<u64>) {
    let min_amount_out = intent.map(|intent| intent.min_amount_out);
    let declared_quote = intent.and_then(DeclaredSwap::declared_quote);
    let quote_timestamp = intent.and_then(|intent| intent.timestamp);
    (min_amount_out, declared_quote, quote_timestamp)
}

/// Max concurrent trace requests per block. Bounds RPC load so a block
/// with many solver trades still completes within the block time
/// without tripping provider rate limits.
const TRACE_CONCURRENCY: usize = 10;

/// The RPC-backed [`ContractCode`] adapter, with a cross-block cache.
///
/// Whether an address had contract code when first checked is kept for the life of the decoder.
/// An address gaining code mid-run (a deploy or an EIP-7702 delegation) keeps its stale answer
/// until restart — acceptable for distinguishing swapper EOAs from pools. A 7702-delegated
/// account carries code, so a 7702 swapper EOA is classified as a contract and dropped; 7702 is
/// not yet widely used, so this is accepted for now.
struct CachedContractCode {
    provider: DynProvider,
    cache: HashMap<Address, bool>,
}

#[async_trait]
impl ContractCode for CachedContractCode {
    /// On RPC failure the address is treated as a contract, per the port's contract.
    async fn is_contract(&mut self, address: Address) -> bool {
        if let Some(&is_contract) = self.cache.get(&address) {
            return is_contract;
        }
        let is_contract = match self.provider.get_code_at(address).await {
            Ok(code) => !code.is_empty(),
            Err(error) => {
                warn!(%address, %error, "failed to fetch code; treating as contract");
                true
            }
        };
        self.cache.insert(address, is_contract);
        is_contract
    }
}

/// Stateful trade decoder: owns the RPC provider, the chain's address
/// registry, the entity decoders, and the caches that are worth keeping
/// across blocks.
pub(crate) struct Decoder {
    provider: DynProvider,
    registry: Registry,
    /// The sender and intent decoder lists, built once. Venue decoders live on their registry
    /// entries instead, constructed with each venue's addresses.
    decoders: EntityDecoders,
    /// Answers decoders' "does this address hold contract code?" over RPC, caching across
    /// blocks.
    contract_code: CachedContractCode,
}

impl Decoder {
    /// The provider is type-erased once here: decoders are trait objects held in the registry
    /// and the entity lists, so they read RPC through `DynProvider` rather than a type
    /// parameter.
    pub(crate) fn new(provider: impl Provider + 'static, registry: Registry) -> Self {
        let provider = provider.erased();
        Self {
            contract_code: CachedContractCode { provider: provider.clone(), cache: HashMap::new() },
            provider,
            registry,
            decoders: EntityDecoders::new(),
        }
    }

    /// The decoder's RPC provider, for adjacent lookups (chain head, token
    /// metadata) that should share its connection.
    pub(crate) fn provider(&self) -> &DynProvider {
        &self.provider
    }

    /// The decoder's address registry, for the label names shared with telemetry.
    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Decode solver trades from a block.
    ///
    /// Fetches all receipts in one `eth_getBlockReceipts` call, then matches a
    /// transaction two ways: its entry point (`tx.to`) is a known venue or
    /// solver, or one of its logs was emitted by a known solver. The
    /// second case catches filler-initiated intent fills (`UniswapX`, 1inch
    /// limit orders) where `tx.to` is a rotating filler. Matched transactions are
    /// traced concurrently; the trace recovers native ETH flows and attributes
    /// the settling solver.
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

        let mut trades = Vec::with_capacity(matched.len());
        for ((index, matched), trace) in matched.into_iter().zip(traces) {
            let tx_index = matched
                .receipt
                .transaction_index
                .unwrap_or(index as u64);
            let root = match trace {
                Ok(root) => root,
                Err(e) => {
                    warn!(
                        block = block_number,
                        tx = %matched.receipt.transaction_hash,
                        "skipping untraceable transaction: {e}"
                    );
                    crate::telemetry::record_untraced_transaction();
                    continue;
                }
            };
            if let Some(mut trade) = self
                .decode_transaction(matched, &root, block_number, tx_index)
                .await
            {
                let evidence = sandwich::detect(&receipts, index, &trade, &self.registry);
                trade.sandwich = evidence;
                trades.push(trade);
            }
        }
        Ok(trades)
    }

    /// Decode one matched transaction from its trace: build the transfer ledger, run the decoders
    /// for its entity, veto non-trades, attribute the solver, and account gas and quote.
    async fn decode_transaction(
        &mut self,
        matched: MatchedSolverTrade<'_>,
        root: &CallFrame,
        block_number: u64,
        tx_index: u64,
    ) -> Option<DecodedTrade> {
        let Self { provider: _, registry, decoders, contract_code } = self;
        let MatchedSolverTrade { receipt, entry_point } = matched;
        let logs = receipt.logs();
        let sender = receipt.from;

        let mut native = Vec::new();
        collect_native_transfers(root, &mut native);
        let transfer_ledger = TransferLedger::from_transaction(logs, &native);

        let role = TraderRole::classify(entry_point, registry);
        let mut ctx = DecodeContext {
            contract_code,
            registry,
            receipt,
            entry_point,
            transfer_ledger: &transfer_ledger,
            input: &root.input,
            root,
        };
        let Some((decoder, mut flow)) = recover(role, decoders, &mut ctx).await else {
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

        let intent = recover_declared_swap(attribution.knowledge, root, registry, &flow);

        warn_on_declaration_disagreement(
            decoder.flow_is_the_declared_swap(),
            receipt.transaction_hash,
            intent.as_ref(),
            &flow,
        );
        let (min_amount_out, declared_quote, quote_timestamp) = declaration_fields(intent.as_ref());

        Some(DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            tx_index,
            venue,
            solver: attribution.solver,
            solver_source: attribution.source,
            decoder: decoder.name(),
            sender: flow.tracked,
            token_in: flow.swap.token_in,
            token_out: flow.swap.token_out,
            amount_in: flow.swap.amount_in,
            amount_out: flow.swap.amount_out,
            venue_fee_in: flow.venue_fee_in,
            venue_fee_out: flow.venue_fee_out,
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
}
