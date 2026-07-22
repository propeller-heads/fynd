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

mod clients;
mod decode;
mod intent;
mod matching;
mod netting_decoders;
mod registry;
mod sandwich;
mod solvers;
mod trace;
mod transfer_ledger;
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
use futures::stream::{StreamExt, TryStreamExt};
use tracing::{debug, warn};

use crate::decoder::{
    decode::{recover, DecodeContext, GasScope},
    matching::MatchedSolverTrade,
    trace::{collect_native_transfers, fetch_trace, route_gas},
    transfer_ledger::TransferLedger,
};
pub(crate) use crate::decoder::{
    registry::Registry,
    sandwich::SandwichEvidence,
    solvers::{attribution::AttributionSource, SolverQuote},
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
    /// Wei cost of the gas the trader paid for the settled route (`gas_used ×
    /// effective_gas_price`). For venue-wrapped entries (Relay, `MetaMask`) the venue's own
    /// overhead is excluded — it is charged whichever router the venue picks, like the venue
    /// fee. `None` when the trader did not pay the transaction's gas (intent fills, solver
    /// rebalances) or the route's gas could not be isolated from the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_gas: Option<U256>,
    /// The solver's own off-chain quote for this swap, recovered from calldata (see
    /// `solvers::embedded_quote` for the solvers that declare one). Informational — it is what
    /// the venue compared against at decision time, as opposed to `amount_out`, which is what
    /// execution delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<SolverQuote>,
    /// Evidence that a front-run and a back-run bracketed this trade (see
    /// `sandwich::detect`). `None` when no bracket pair was found.
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
        let Self { provider, registry, code_cache } = self;
        let MatchedSolverTrade { receipt, entry_point } = matched;
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

        // A client fingerprint (a kpk Safe owning the order, or a client fee wallet taking the fee
        // on a shared router) overrides the entry-point label, backing any client fee out before
        // the quote check reads the grossed output.
        let integrator = solvers::integrator(logs);
        let venue =
            clients::attribute(registry, &mut flow, &transfer_ledger, integrator.as_deref())
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

        // The solver's off-chain quote, when its calldata declares one. Dispatched on the
        // attributed solver so a lookalike blob from another router cannot masquerade as a
        // quote, and unit-checked against the settled amount (quotes are self-reported).
        let quote = solvers::embedded_quote(&attribution.solver, &root.input, flow.swap.amount_in)
            .filter(|quote| solvers::plausible_quote(quote, flow.swap.amount_out));

        Some(DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            tx_index,
            venue,
            solver: attribution.solver,
            solver_source: attribution.source,
            decoder,
            sender: flow.tracked,
            token_in: flow.swap.token_in,
            token_out: flow.swap.token_out,
            amount_in: flow.swap.amount_in,
            amount_out: flow.swap.amount_out,
            venue_fee_in: flow.venue_fee_in,
            venue_fee_out: flow.venue_fee_out,
            settled_gas,
            quote,
            // The full receipts slice isn't available here (this fn only sees the one matched
            // transaction); the caller (`decode_block`) fills this in once decoding succeeds.
            sandwich: None,
        })
    }
}
