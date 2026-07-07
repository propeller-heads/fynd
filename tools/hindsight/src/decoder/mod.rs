mod guards;
mod ledger;
mod registry;
mod trace;
mod venues;

pub(crate) mod allium;
pub(crate) mod verify_decoder;

#[cfg(test)]
mod test_utils;

use std::collections::HashMap;

use alloy::{
    primitives::{Address, TxHash, U256},
    providers::Provider,
};
use anyhow::Context;
use futures::stream::{StreamExt, TryStreamExt};
use tracing::{debug, warn};

use crate::decoder::{
    guards::{received_nft, wrap_pair_mispaired},
    ledger::TransferLedger,
    trace::{attribute_solver, collect_native_transfers, fetch_trace, route_gas},
    venues::{DecodeContext, Matched},
};
pub(crate) use crate::decoder::{registry::Registry, venues::SolverQuote};

/// A decoded solver trade: what token went in, what came out.
///
/// Native ETH is represented as [`Address::ZERO`].
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DecodedTrade {
    pub tx_hash: TxHash,
    pub block_number: u64,
    pub client: String,
    pub solver: String,
    pub sender: Address,
    pub token_in: Address,
    pub token_out: Address,
    /// Input amount that actually entered the swap — a client fee skimmed from the input (see
    /// [`client_fee`]) is already subtracted, so a re-solve compares like-for-like.
    pub amount_in: U256,
    /// Gross swap output — a client fee skimmed from the output (see [`client_fee_out`]) is added
    /// back, so the settled amount is the full swap proceeds, comparable to Fynd's gross output.
    pub amount_out: U256,
    /// Client fee skimmed from the input token before swapping (e.g. Relay's fee), in `token_in`
    /// units. `None` when no known fee collector took a cut. Recorded for transparency; it is
    /// already excluded from `amount_in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_fee: Option<U256>,
    /// Client fee skimmed from the output token after swapping, in `token_out` units. `None` when
    /// no known fee collector took a cut. Recorded for transparency; it is already added back into
    /// `amount_out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_fee_out: Option<U256>,
    /// Wei cost of the gas the trader paid for the settled route (`gas_used ×
    /// effective_gas_price`). For client-wrapped entries (Relay, MetaMask) the client's own
    /// overhead is excluded — it is charged whichever router the client picks, like the client
    /// fee. `None` when the trader did not pay the transaction's gas (maker fills, solver
    /// rebalances) or the route's gas could not be isolated from the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_gas: Option<U256>,
    /// The solver's own off-chain quote for this swap, recovered from calldata (see
    /// [`venues::embedded_quote`] for the solvers that declare one). Informational — it is what
    /// the client compared against at decision time, as opposed to `amount_out`, which is what
    /// execution delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<SolverQuote>,
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

    /// Decode solver trades from a block.
    ///
    /// Fetches all receipts in one `eth_getBlockReceipts` call, then matches a
    /// transaction two ways: its entry point (`tx.to`) is a known client or
    /// solver, or one of its logs was emitted by a known solver. The
    /// second case catches filler-initiated intent fills (UniswapX, 1inch
    /// limit orders) where `tx.to` is a rotating filler. Matched transactions are
    /// traced concurrently; the trace recovers native ETH flows and attributes
    /// the settling solver.
    pub(crate) async fn decode_block(
        &mut self,
        block_number: u64,
    ) -> anyhow::Result<Vec<DecodedTrade>> {
        let Self { provider, registry, code_cache } = self;

        let receipts = provider
            .get_block_receipts(block_number.into())
            .await
            .with_context(|| format!("failed to fetch receipts for block {block_number}"))?
            .ok_or_else(|| anyhow::anyhow!("block {block_number} not found"))?;

        let matched: Vec<Matched> = receipts
            .iter()
            .filter_map(|receipt| venues::select(receipt, registry))
            .collect();

        // Per-block batch: trace every matched tx concurrently (bounded),
        // collected in block order for deterministic output. Wall-clock cost is
        // one receipts call plus the slowest trace wave — not the sum of every
        // request — so a block stays well inside its block time.
        let traces = futures::stream::iter(
            matched
                .iter()
                .map(|m| fetch_trace(provider, m.receipt.transaction_hash)),
        )
        .buffered(TRACE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

        let mut trades = Vec::with_capacity(matched.len());
        for (matched, root) in matched.into_iter().zip(traces) {
            let Matched { receipt, entry_point, strategy } = matched;
            let logs = receipt.logs();
            let sender = receipt.from;

            let mut native = Vec::new();
            collect_native_transfers(&root, &mut native);
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
                    client = %registry.label(entry_point),
                    "no token or native ETH flow found"
                );
                continue;
            };

            // A trader who received an NFT in the same transaction was buying, not swapping: the
            // netted token flow is the payment side of a purchase (e.g. an NFT sweep through
            // Relay + Seaport), and the real consideration is invisible to ERC-20 netting.
            // Recording it would pair the payment with the change as a phantom swap.
            if received_nft(logs, flow.tracked) {
                debug!(
                    tx = %receipt.transaction_hash,
                    client = %registry.label(entry_point),
                    "tracked address received an NFT; skipping purchase"
                );
                continue;
            }

            // A native <-> wrapped-native "swap" is a wrap or unwrap, which is 1:1 by
            // construction. Far off parity it is a mis-paired record: a cross-chain deposit
            // whose only same-chain receipt is a dust remainder refund (seen via Relay: WETH
            // in, a billionth of it back as ETH).
            if wrap_pair_mispaired(&flow.swap, registry.wrapped_native()) {
                debug!(
                    tx = %receipt.transaction_hash,
                    client = %registry.label(entry_point),
                    "wrap pair far off 1:1; skipping mis-paired trade"
                );
                continue;
            }

            // A strategy that knows its venue asserts it on the flow (e.g. MetaMask declares it
            // in calldata); otherwise attribute from the trace.
            let solver = flow
                .solver_override
                .take()
                .unwrap_or_else(|| {
                    registry.label(
                        attribute_solver(&root, entry_point, sender, registry)
                            .unwrap_or(entry_point),
                    )
                });

            // Gas the trader paid for the settled route, as a wei cost. Only charged when the
            // tracked trader sent the transaction; for client-wrapped entries the route's gas is
            // read from the venue call's trace frame so the wrapper's overhead stays out of the
            // comparison on both sides.
            let settled_gas = flow
                .trader_paid_gas
                .then(|| {
                    if strategy.routes_via_wrapper() {
                        route_gas(&root, registry)
                    } else {
                        Some(U256::from(receipt.gas_used))
                    }
                })
                .flatten()
                .map(|units| units * U256::from(receipt.effective_gas_price));

            // The solver's off-chain quote, when its calldata declares one. Dispatched on the
            // attributed solver so a lookalike blob from another router cannot masquerade as a
            // quote, and unit-checked against the settled amount (quotes are self-reported).
            let quote = venues::embedded_quote(&solver, &root.input, flow.swap.amount_in)
                .filter(|quote| venues::plausible_quote(quote, flow.swap.amount_out));

            trades.push(DecodedTrade {
                tx_hash: receipt.transaction_hash,
                block_number,
                client: registry.label(entry_point),
                solver,
                sender: flow.tracked,
                token_in: flow.swap.token_in,
                token_out: flow.swap.token_out,
                amount_in: flow.swap.amount_in,
                amount_out: flow.swap.amount_out,
                client_fee: flow.client_fee,
                client_fee_out: flow.client_fee_out,
                settled_gas,
                quote,
            });
        }

        Ok(trades)
    }
}
