//! Decode solver trades from on-chain data.
//!
//! Terminology — three tiers, two of which appear in every record:
//! - **venue** (`venues/`): the contract the user entered through (`tx.to`) — Relay, `MetaMask`.
//!   Order-flow owners; they pick a solver and may skim a fee.
//! - **solver** (`solvers/`): the router that computed and settled the route — `KyberSwap`, 1inch,
//!   0x. These are Fynd's competitors. Datasets recorded before run6 call this tier `aggregator` in
//!   their column names; the two words mean the same thing.
//! - **liquidity venues**: the pools and makers a route executes against (Uniswap, Curve,
//!   prop-AMMs). Not modeled here; they only appear inside traces.
//!
//! The pipeline is match → trace → decode → guard → record: `strategy` picks how a matched
//! transaction is decoded, `ledger` answers all value-flow questions, `guards` vetoes shapes that
//! are not comparable trades, and `registry` is the address book behind matching.

mod guards;
mod intent;
mod ledger;
mod registry;
mod sandwich;
mod solvers;
mod strategy;
mod trace;
pub(crate) mod venues;

#[cfg(test)]
mod test_utils;

use std::collections::HashMap;

use alloy::{
    primitives::{Address, TxHash, U256},
    providers::Provider,
    rpc::types::trace::geth::CallFrame,
};
use anyhow::Context;
use futures::stream::{StreamExt, TryStreamExt};
use tracing::{debug, warn};

use crate::decoder::{
    ledger::TransferLedger,
    strategy::{DecodeContext, Matched},
    trace::{collect_native_transfers, fetch_trace, route_gas},
};
pub(crate) use crate::decoder::{
    registry::Registry,
    sandwich::SandwichEvidence,
    solvers::{attribution::AttributionSource, SolverQuote},
};

/// A decoded solver trade: what token went in, what came out.
///
/// Native ETH is represented as [`Address::ZERO`].
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DecodedTrade {
    pub tx_hash: TxHash,
    pub block_number: u64,
    /// The transaction's position in its block, from the receipt (falls back to its position in
    /// the fetched receipt slice when the RPC omitted it).
    pub tx_index: u64,
    pub venue: String,
    pub solver: String,
    /// The evidence tier the solver label came from (see [`solvers::attribution`]). Downstream
    /// analysis weighs low-trust tiers (`largest_call`, fallback) differently — e.g. when judging
    /// an embedded quote.
    pub solver_source: AttributionSource,
    pub sender: Address,
    pub token_in: Address,
    pub token_out: Address,
    /// Input amount that actually entered the swap — a venue fee skimmed from the input (see
    /// [`venue_fee`]) is already subtracted, so a re-solve compares like-for-like.
    pub amount_in: U256,
    /// Gross swap output — a venue fee skimmed from the output (see [`venue_fee_out`]) is added
    /// back, so the settled amount is the full swap proceeds, comparable to Fynd's gross output.
    pub amount_out: U256,
    /// Venue fee skimmed from the input token before swapping (e.g. Relay's fee), in `token_in`
    /// units. `None` when no known fee collector took a cut. Recorded for transparency; it is
    /// already excluded from `amount_in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_fee: Option<U256>,
    /// Venue fee skimmed from the output token after swapping, in `token_out` units. `None` when
    /// no known fee collector took a cut. Recorded for transparency; it is already added back into
    /// `amount_out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_fee_out: Option<U256>,
    /// Wei cost of the gas the trader paid for the settled route (`gas_used ×
    /// effective_gas_price`). For venue-wrapped entries (Relay, `MetaMask`) the venue's own
    /// overhead is excluded — it is charged whichever router the venue picks, like the venue
    /// fee. `None` when the trader did not pay the transaction's gas (maker fills, solver
    /// rebalances) or the route's gas could not be isolated from the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_gas: Option<U256>,
    /// The solver's own off-chain quote for this swap, recovered from calldata (see
    /// [`solvers::embedded_quote`] for the solvers that declare one). Informational — it is what
    /// the venue compared against at decision time, as opposed to `amount_out`, which is what
    /// execution delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<SolverQuote>,
    /// Evidence that a front-run and a back-run bracketed this trade (see
    /// [`sandwich::detect`]). `None` when no bracket pair was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandwich: Option<SandwichEvidence>,
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
    /// for distinguishing maker EOAs from pools.
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

    /// The decoder's address registry, for label vocabulary shared with telemetry.
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
        let receipts = self
            .provider
            .get_block_receipts(block_number.into())
            .await
            .with_context(|| format!("failed to fetch receipts for block {block_number}"))?
            .ok_or_else(|| anyhow::anyhow!("block {block_number} not found"))?;

        // Paired with each receipt's position in the slice, since that position — not the
        // transaction_index field, which the RPC may omit — is what "neighbor" means for the
        // sandwich scan below: receipts are already in block order.
        let matched: Vec<(usize, Matched)> = receipts
            .iter()
            .enumerate()
            .filter_map(|(index, receipt)| {
                strategy::select(receipt, &self.registry).map(|matched| (index, matched))
            })
            .collect();

        // Per-block batch: trace every matched tx concurrently (bounded),
        // collected in block order for deterministic output. Wall-clock cost is
        // one receipts call plus the slowest trace wave — not the sum of every
        // request — so a block stays well inside its block time.
        let traces = futures::stream::iter(
            matched
                .iter()
                .map(|(_, m)| fetch_trace(&self.provider, m.receipt.transaction_hash)),
        )
        .buffered(TRACE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

        let mut trades = Vec::with_capacity(matched.len());
        for ((index, matched), root) in matched.into_iter().zip(traces) {
            let tx_index = matched
                .receipt
                .transaction_index
                .unwrap_or(index as u64);
            if let Some(mut trade) = self
                .decode_transaction(matched, &root, block_number, tx_index)
                .await
            {
                trade.sandwich = sandwich::detect(&receipts, index, trade.sender, &self.registry);
                trades.push(trade);
            }
        }
        Ok(trades)
    }

    /// Decode one matched transaction from its trace: build the ledger, run the trader
    /// strategy, guard the result, attribute the solver, and account gas and quote.
    async fn decode_transaction(
        &mut self,
        matched: Matched<'_>,
        root: &CallFrame,
        block_number: u64,
        tx_index: u64,
    ) -> Option<DecodedTrade> {
        let Self { provider, registry, code_cache } = self;
        let Matched { receipt, entry_point, strategy } = matched;
        let logs = receipt.logs();
        let sender = receipt.from;

        let mut native = Vec::new();
        collect_native_transfers(root, &mut native);
        let ledger = TransferLedger::from_transaction(logs, &native);

        let flow = strategy
            .decode(DecodeContext {
                provider,
                registry,
                code_cache,
                ledger: &ledger,
                input: &root.input,
                sender,
                entry_point,
            })
            .await;

        let Some(mut flow) = flow else {
            warn!(
                tx = %receipt.transaction_hash,
                venue = %registry.label(entry_point),
                "no token or native ETH flow found"
            );
            return None;
        };

        if let Some(veto) = guards::veto(&flow, logs, registry) {
            debug!(
                tx = %receipt.transaction_hash,
                venue = %registry.label(entry_point),
                ?veto,
                "decoded flow is not a comparable trade; skipping"
            );
            return None;
        }

        let attribution = solvers::attribution::attribute(
            flow.solver_override.take(),
            root,
            entry_point,
            sender,
            registry,
        );

        // Gas the trader paid for the settled route, as a wei cost. Only charged when the
        // tracked trader sent the transaction; for venue-wrapped entries the route's gas is
        // read from the solver call's trace frame so the wrapper's overhead stays out of the
        // comparison on both sides.
        let settled_gas = flow
            .trader_paid_gas
            .then(|| {
                if strategy.routes_via_wrapper() {
                    route_gas(root, registry)
                } else {
                    Some(U256::from(receipt.gas_used))
                }
            })
            .flatten()
            .map(|units| units * U256::from(receipt.effective_gas_price));

        // The solver's off-chain quote, when its calldata declares one. Dispatched on the
        // attributed solver so a lookalike blob from another router cannot masquerade as a
        // quote, and unit-checked against the settled amount (quotes are self-reported).
        let quote = solvers::embedded_quote(&attribution.solver, &root.input, flow.swap.amount_in)
            .filter(|quote| solvers::plausible_quote(quote, flow.swap.amount_out));

        Some(DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            tx_index,
            venue: registry.label(entry_point),
            solver: attribution.solver,
            solver_source: attribution.source,
            sender: flow.tracked,
            token_in: flow.swap.token_in,
            token_out: flow.swap.token_out,
            amount_in: flow.swap.amount_in,
            amount_out: flow.swap.amount_out,
            venue_fee: flow.venue_fee,
            venue_fee_out: flow.venue_fee_out,
            settled_gas,
            quote,
            // The full receipts slice isn't available here (this fn only sees the one matched
            // transaction); the caller (`decode_block`) fills this in once decoding succeeds.
            sandwich: None,
        })
    }
}
