//! Decode solver trades from on-chain data.
//!
//! Terminology — three tiers, two of which appear in every record:
//! - **venue**: the contract the user entered through (`tx.to`) — Relay, `MetaMask`. Order-flow
//!   owners; they pick a solver and may take a fee.
//! - **solver** (`solvers/`, the only tier with code): the router that computed and settled the
//!   route — `KyberSwap`, 1inch, 0x. These are Fynd's competitors. Datasets recorded before run6
//!   call this tier `aggregator` in their column names; the two words mean the same thing.
//! - **liquidity venues**: the pools and makers a route executes against (Uniswap, Curve,
//!   prop-AMMs). Not modeled here; they only appear inside traces.
//!
//! The pipeline is three steps, per block:
//!
//! 1. **Trace the whole block** — one `eth_getBlockReceipts` call and one
//!    `debug_traceBlockByNumber` call.
//! 2. **Per transaction, decode the swap from the solver's side** — the declared decode reads the
//!    settling solver frame's own calldata (`declared`), or `CoW`'s `Trade` log for batch
//!    settlements; `netting` is the fallback, and its records are marked (`decode: "netted"`). A
//!    transaction with no known solver frame, venue entry, batch settler, or solver log is skipped.
//!    `veto` rejects shapes that are not comparable trades.
//! 3. **Attribute** — `attribution` names the solver and the venue on the record; `registry` is the
//!    address book behind every lookup.

mod attribution;
mod declared;
mod netting;
mod registry;
mod sandwich;
mod solvers;
mod trace;
mod transfer_ledger;
mod veto;

#[cfg(test)]
mod test_utils;

use std::collections::HashMap;

use alloy::{
    eips::BlockId,
    network::{AnyTransactionReceipt, ReceiptResponse},
    primitives::{Address, TxHash, U256},
    providers::Provider,
    rpc::types::trace::geth::CallFrame,
};
use anyhow::Context;
use tracing::{debug, warn};

pub(crate) use crate::decoder::{
    attribution::AttributionSource, registry::Registry, sandwich::SandwichEvidence,
};
use crate::decoder::{
    solvers::DeclaredSwap,
    trace::{collect_native_transfers, fetch_block_traces},
    transfer_ledger::TransferLedger,
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
    /// The evidence tier the solver label came from (see `attribution`). Downstream
    /// analysis weighs low-trust tiers (`largest_call`, fallback) differently — e.g. when judging
    /// an embedded quote.
    pub solver_source: AttributionSource,
    /// Which decoder recovered this trade (see `decode`). Once several decoders can carry a
    /// venue's trades this measures how often each one carries a trade the others could not.
    pub decoder: &'static str,
    /// How this record's amounts were read: `"declared"` (the settling solver's own calldata or
    /// logs — the trusted tier) or `"netted"` (balance netting — a fallback whose amounts can be
    /// off by an unaccounted fee; the report excludes these by default).
    pub decode: &'static str,
    pub sender: Address,
    pub token_in: Address,
    pub token_out: Address,
    /// Input amount that entered the swap. A fee paid to a `[venue_fees]` wallet out of the input
    /// is subtracted (see `attribution::venue`), so a re-solve is quoted the amount that reached
    /// the pools.
    pub amount_in: U256,
    /// Gross swap output. A fee paid to a `[venue_fees]` wallet out of the output is added back
    /// (see `attribution::venue`), so the settled amount is the full swap proceeds, comparable to
    /// Fynd's gross output.
    pub amount_out: U256,
    /// The on-chain enforced floor declared in the settling solver frame's own calldata (see
    /// `SolverDecoder::declared` for the solvers that declare one). A settled trade cleared
    /// this by construction; it is recorded so avoidance analysis has the same field on both
    /// settled and reverted trades. `None` when no solver frame was found or its calldata did
    /// not parse.
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

/// The terms the solver declared alongside the trade, or all-`None` for a netted record where no
/// solver was read. Split out of `decode_transaction` purely to keep it under the line limit.
fn declared_terms(declared: Option<&DeclaredSwap>) -> (Option<U256>, Option<U256>, Option<u64>) {
    (
        declared.and_then(|declared| declared.min_amount_out),
        declared.and_then(|declared| declared.declared_quote),
        declared.and_then(|declared| declared.timestamp),
    )
}

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

    /// Decode solver trades from a block.
    ///
    /// Fetches all receipts in one `eth_getBlockReceipts` call and all traces in one
    /// `debug_traceBlockByNumber` call, then matches a transaction three ways: a known solver's
    /// frame appears in its trace, its entry point (`tx.to`) is a known venue, solver, or batch
    /// settler, or one of its logs was emitted by a known solver (filler-initiated intent fills,
    /// where `tx.to` is a rotating filler). Everything else is skipped, never decoded.
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

        // One debug_traceBlockByNumber call covers the block. A transaction the tracer could not
        // process is absent from the map and costs that trade, not the block.
        let mut roots = fetch_block_traces(&self.provider, block_number).await?;

        let mut trades = Vec::new();
        // The receipt's position in the slice — not the transaction_index field, which the RPC
        // may omit — is what "neighbor" means for the sandwich scan below: receipts are already
        // in block order.
        for (index, receipt) in receipts.iter().enumerate() {
            if !receipt.status() {
                continue;
            }
            let Some(entry_point) = receipt.to else { continue };
            let known_entry = self.registry.is_known(entry_point) ||
                self.registry
                    .is_batch_settler(entry_point);
            let solver_logged = receipt
                .logs()
                .iter()
                .any(|log| self.registry.is_solver(log.address()));

            let Some(root) = roots.remove(&receipt.transaction_hash) else {
                if known_entry || solver_logged {
                    warn!(
                        block = block_number,
                        tx = %receipt.transaction_hash,
                        "skipping transaction absent from the block trace"
                    );
                    crate::telemetry::record_untraced_transaction();
                }
                continue;
            };
            // Matching: a known solver frame in the trace, a known entry point, or a known
            // solver's log. Everything else is skipped, never decoded.
            if !known_entry &&
                !solver_logged &&
                trace::find_solver_frame(&root, &self.registry).is_none()
            {
                continue;
            }
            let tx_index = receipt
                .transaction_index
                .unwrap_or(index as u64);
            if let Some(mut trade) = self
                .decode_transaction(receipt, entry_point, &root, block_number, tx_index)
                .await
            {
                let evidence = sandwich::detect(&receipts, index, &trade, &self.registry);
                trade.sandwich = evidence;
                trades.push(trade);
            }
        }
        Ok(trades)
    }

    /// Decode one matched transaction from its trace: build the transfer ledger, decode the swap
    /// (declared first, netting fallback), veto non-trades, and attribute the solver and venue.
    async fn decode_transaction(
        &mut self,
        receipt: &AnyTransactionReceipt,
        entry_point: Address,
        root: &CallFrame,
        block_number: u64,
        tx_index: u64,
    ) -> Option<DecodedTrade> {
        let Self { provider, registry, code_cache } = self;
        let logs = receipt.logs();
        let sender = receipt.from;

        let mut native = Vec::new();
        collect_native_transfers(root, &mut native);
        let transfer_ledger = TransferLedger::from_transaction(logs, &native);

        // The declared decode runs first: the settling solver's own data is the trusted reading.
        // Netting is the fallback, and its records are marked. A solver that declares the
        // transaction is not a swap at all vetoes it here, before netting gets a chance to pair
        // its legs into a trade that never happened.
        let read = declared::declared_flow(root, registry, logs, &transfer_ledger, sender).ok()?;
        let (decoder, mut flow, declared) = if let Some((decoder, flow, declared)) = read {
            (decoder, flow, Some(declared))
        } else {
            let netted = netting::fallback_flow(
                provider,
                code_cache,
                registry,
                &transfer_ledger,
                sender,
                entry_point,
            )
            .await;
            let Some((decoder, flow)) = netted else {
                warn!(
                    tx = %receipt.transaction_hash,
                    venue = %registry.label(entry_point),
                    "no decoder recovered a trade from this transaction"
                );
                return None;
            };
            (decoder, flow, None)
        };

        if let Some(veto) = veto::check(&flow, &transfer_ledger, logs, registry) {
            debug!(
                tx = %receipt.transaction_hash,
                venue = %registry.label(entry_point),
                ?veto,
                "decoded flow is not a comparable trade; skipping"
            );
            return None;
        }

        let attribution = attribution::solver(root, entry_point, sender, registry);

        // A venue fingerprint overrides the entry-point label (see `attribution`). Two of the four
        // are read from the settling solver's own data, dispatched through its address-book entry
        // so this file names no solver; the other two are pure address-book lookups.
        let tag = attribution
            .address
            .and_then(|address| registry.solver(address))
            .and_then(|solver| {
                solver
                    .decoder
                    .venue_fingerprint(&root.input, logs)
            });
        let venue = attribution::venue(registry, &mut flow, &transfer_ledger, tag.as_ref())
            .unwrap_or_else(|| registry.label(entry_point));

        let (min_amount_out, declared_quote, quote_timestamp) = declared_terms(declared.as_ref());
        let decode = if declared.is_some() { "declared" } else { "netted" };

        Some(DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            tx_index,
            venue,
            solver: attribution.solver,
            solver_source: attribution.source,
            decoder,
            decode,
            sender: flow.tracked,
            token_in: flow.token_in,
            token_out: flow.token_out,
            amount_in: flow.amount_in,
            amount_out: flow.amount_out,
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
        use alloy::rpc::types::trace::{common::TraceResult, geth::GethTrace};

        let asserter = Asserter::new();
        asserter.push_success(&vec![
            swap_receipt(tx_hash(1), addr(1)),
            swap_receipt(tx_hash(2), addr(2)),
        ]);
        // The block trace answers in one call: the first transaction failed inside the tracer,
        // the second traced fine.
        asserter.push_success(&vec![
            TraceResult::Error { error: "tracer aborted".to_string(), tx_hash: Some(tx_hash(1)) },
            TraceResult::Success {
                result: GethTrace::CallTracer(frame("CALL", addr(2), ONEINCH, 0)),
                tx_hash: Some(tx_hash(2)),
            },
        ]);

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

    #[tokio::test]
    async fn test_venue_fingerprint_reaches_the_settling_solvers_decoder() {
        use alloy::{
            primitives::B256,
            rpc::types::{
                trace::{common::TraceResult, geth::GethTrace},
                Log,
            },
            sol_types::SolEvent,
        };

        use crate::decoder::solvers::lifi::LiFiGenericSwapCompleted;

        // An Infinex swap through the shared LiFi Diamond: the venue is named only by the
        // integrator tag in LiFi's own event. The fixture names LiFi because it needs concrete
        // bytes; the decode path under test does not — it asks whatever decoder the Diamond's
        // address-book entry carries. So this asserts the dispatch, not the parse (`lifi.rs`
        // tests that).
        let lifi = address!("0x1231deb6f5749ef6ce6943a275a1d3e7486f4eae");
        let trader = addr(1);
        let pool = addr(0x50);
        let (token_in, token_out) = (addr(0xaa), addr(0xbb));

        let swap_event = LiFiGenericSwapCompleted {
            transactionId: B256::ZERO,
            integrator: "infinex".to_string(),
            referrer: String::new(),
            receiver: trader,
            fromAssetId: token_in,
            toAssetId: token_out,
            fromAmount: U256::from(1_000),
            toAmount: U256::from(2_000),
        };
        let log_data = swap_event.encode_log_data();
        let lifi_log = Log {
            inner: alloy::primitives::Log::new_unchecked(
                lifi,
                log_data.topics().to_vec(),
                log_data.data.clone(),
            ),
            ..Default::default()
        };

        let asserter = Asserter::new();
        asserter.push_success(&vec![receipt(
            tx_hash(1),
            trader,
            Some(lifi),
            vec![
                make_transfer_log(token_in, trader, pool, U256::from(1_000)),
                make_transfer_log(token_out, pool, trader, U256::from(2_000)),
                lifi_log,
            ],
        )]);
        let traces: Vec<TraceResult<GethTrace, String>> = vec![TraceResult::Success {
            result: GethTrace::CallTracer(frame("CALL", trader, lifi, 0)),
            tx_hash: Some(tx_hash(1)),
        }];
        asserter.push_success(&traces);

        let mut decoder = Decoder::new(
            ProviderBuilder::default().connect_mocked_client(asserter),
            Registry::ethereum(),
        );
        let trades = decoder
            .decode_block(21_000_000)
            .await
            .unwrap();

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].solver, "lifi");
        // Without the fingerprint dispatch this reads "lifi" — the router, not the frontend.
        assert_eq!(trades[0].venue, "infinex");
    }
}
