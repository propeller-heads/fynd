mod match_receipts;
mod net;
mod registry;
mod trace;

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
use tracing::warn;

pub(crate) use crate::decoder::registry::Registry;
use crate::decoder::{
    match_receipts::{find_maker_trade, match_receipt, Matched},
    net::{decode_relay_rebalance, decode_trade, fee_to_collectors},
    trace::{attribute_aggregator, collect_native_transfers, fetch_trace},
};

/// A decoded aggregator trade: what token went in, what came out.
///
/// Native ETH is represented as [`Address::ZERO`].
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DecodedTrade {
    pub tx_hash: TxHash,
    pub block_number: u64,
    pub client: String,
    pub aggregator: String,
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
}

/// Max concurrent trace requests per block. Bounds RPC load so a block
/// with many aggregator trades still completes within the block time
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

    /// Decode aggregator trades from a block.
    ///
    /// Fetches all receipts in one `eth_getBlockReceipts` call, then matches a
    /// transaction two ways: its entry point (`tx.to`) is a known client or
    /// aggregator, or one of its logs was emitted by a known aggregator. The
    /// second case catches filler-initiated intent fills (UniswapX, 1inch
    /// limit orders) where `tx.to` is a rotating filler. Matched transactions are
    /// traced concurrently; the trace recovers native ETH flows and attributes
    /// the settling aggregator.
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
            .filter_map(|receipt| match_receipt(receipt, registry))
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
            let Matched { receipt, entry_point, intent_fill } = matched;
            let logs = receipt.logs();
            let sender = receipt.from;

            let mut native = Vec::new();
            collect_native_transfers(&root, &mut native);

            // For an intent fill the sender is the filler, so find the order maker
            // by its net flow. Otherwise track the sender, falling back to the
            // entry point for the rare case where output is delivered there.
            let decoded = if intent_fill {
                find_maker_trade(
                    provider,
                    logs,
                    &native,
                    &[entry_point, sender],
                    registry,
                    code_cache,
                )
                .await
            } else {
                decode_trade(logs, &native, sender)
                    .map(|trade| (sender, trade))
                    .or_else(|| {
                        decode_trade(logs, &native, entry_point).map(|trade| (entry_point, trade))
                    })
            };

            let aggregator =
                attribute_aggregator(&root, entry_point, sender, registry).unwrap_or(entry_point);

            let Some((tracked, swap)) = decoded else {
                // No user net flow. A Relay solver rebalancing fill has its sender net to zero, so
                // anchor on the fee collector instead (Relay funds the swap from it).
                if registry
                    .relay()
                    .routers
                    .contains(&entry_point)
                {
                    if let Some(swap) = decode_relay_rebalance(
                        logs,
                        &native,
                        &registry.relay().fee_collectors,
                        &registry.relay().routers,
                        registry.wrapped_native(),
                    ) {
                        trades.push(DecodedTrade {
                            tx_hash: receipt.transaction_hash,
                            block_number,
                            client: registry.label(entry_point),
                            aggregator: registry.label(aggregator),
                            sender,
                            token_in: swap.token_in,
                            token_out: swap.token_out,
                            amount_in: swap.amount_in,
                            amount_out: swap.amount_out,
                            // The collector is the funding source here, not a skim — no fee
                            // back-out.
                            client_fee: None,
                            client_fee_out: None,
                        });
                        continue;
                    }
                }
                warn!(
                    tx = %receipt.transaction_hash,
                    client = %registry.label(entry_point),
                    "no token or native ETH flow found"
                );
                continue;
            };

            // A known client collector can skim a fee on either side. Back an input-side skim out
            // of `amount_in` (the user's gross spend included money that never entered the swap)
            // and add an output-side skim back into `amount_out` (the swap produced more than the
            // user kept), so both sides are the amounts actually swapped — the like-for-like basis
            // vs Fynd.
            let fees = fee_to_collectors(logs, &native, &registry.relay().fee_collectors);
            let client_fee = fees
                .get(&swap.token_in)
                .copied()
                .filter(|fee| !fee.is_zero());
            let amount_in =
                client_fee.map_or(swap.amount_in, |fee| swap.amount_in.saturating_sub(fee));
            let client_fee_out = fees
                .get(&swap.token_out)
                .copied()
                .filter(|fee| !fee.is_zero());
            let amount_out =
                client_fee_out.map_or(swap.amount_out, |fee| swap.amount_out.saturating_add(fee));

            trades.push(DecodedTrade {
                tx_hash: receipt.transaction_hash,
                block_number,
                client: registry.label(entry_point),
                aggregator: registry.label(aggregator),
                sender: tracked,
                token_in: swap.token_in,
                token_out: swap.token_out,
                amount_in,
                amount_out,
                client_fee,
                client_fee_out,
            });
        }

        Ok(trades)
    }
}
