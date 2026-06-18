mod net;
mod registry;
mod trace;

#[cfg(test)]
mod test_support;

use alloy::{
    primitives::{Address, TxHash, U256},
    providers::Provider,
    rpc::types::TransactionReceipt,
};
use anyhow::Context;
use futures::stream::{StreamExt, TryStreamExt};
use tracing::warn;

use crate::decoder::{
    net::decode_trade,
    registry::{known_aggregators, known_names, label},
    trace::{attribute_aggregator, collect_native_transfers, fetch_trace},
};

/// A decoded aggregator trade: what token went in, what came out.
///
/// Native ETH is represented as [`Address::ZERO`].
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DecodedTrade {
    pub tx_hash: String,
    pub block_number: u64,
    pub client: String,
    pub aggregator: String,
    pub sender: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
}

/// Max concurrent trace requests per block. Bounds RPC load so a block
/// with many aggregator trades still completes within the block time
/// without tripping provider rate limits.
const TRACE_CONCURRENCY: usize = 10;

/// Decode aggregator trades from a block.
///
/// Fetches all receipts in one `eth_getBlockReceipts` call, filters to
/// successful transactions that hit a known client or aggregator, then
/// traces those transactions concurrently. The trace recovers native ETH
/// flows and, for client-routed trades, attributes the settling aggregator.
pub(crate) async fn decode_block<P: Provider>(
    provider: &P,
    block_number: u64,
) -> anyhow::Result<Vec<DecodedTrade>> {
    let aggregators = known_aggregators();
    let names = known_names();

    let receipts = provider
        .get_block_receipts(block_number.into())
        .await
        .with_context(|| format!("failed to fetch receipts for block {block_number}"))?
        .ok_or_else(|| anyhow::anyhow!("block {block_number} not found"))?;

    let matched: Vec<(&TransactionReceipt, Address)> = receipts
        .iter()
        .filter_map(|receipt| {
            let entry_point = receipt.to?;
            (receipt.status() && names.contains_key(&entry_point)).then_some((receipt, entry_point))
        })
        .collect();

    // Per-block batch: trace every matched tx concurrently (bounded),
    // collected in block order for deterministic output. Wall-clock cost is
    // one receipts call plus the slowest trace wave — not the sum of every
    // request — so a block stays well inside its block time.
    let hashes: Vec<TxHash> = matched
        .iter()
        .map(|(receipt, _)| receipt.transaction_hash)
        .collect();
    let traces = futures::stream::iter(
        hashes
            .into_iter()
            .map(|hash| fetch_trace(provider, hash)),
    )
    .buffered(TRACE_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;

    let mut trades = Vec::with_capacity(matched.len());
    for ((receipt, entry_point), root) in matched.into_iter().zip(traces) {
        let logs = receipt.logs();
        let sender = receipt.from;

        let mut native = Vec::new();
        collect_native_transfers(&root, &mut native);

        // Track the sender first; fall back to the entry-point contract for
        // the rare case where the output is delivered there.
        let decoded = decode_trade(logs, &native, sender)
            .map(|trade| (sender, trade))
            .or_else(|| decode_trade(logs, &native, entry_point).map(|trade| (entry_point, trade)));

        let Some((tracked, (token_in, amount_in, token_out, amount_out))) = decoded else {
            warn!(
                tx = %receipt.transaction_hash,
                client = %label(entry_point, &names),
                "no token or native ETH flow found for sender or entry point"
            );
            continue;
        };

        let aggregator =
            attribute_aggregator(&root, entry_point, sender, &aggregators).unwrap_or(entry_point);

        trades.push(DecodedTrade {
            tx_hash: format!("{}", receipt.transaction_hash),
            block_number,
            client: label(entry_point, &names),
            aggregator: label(aggregator, &names),
            sender: tracked,
            token_in,
            token_out,
            amount_in,
            amount_out,
        });
    }

    Ok(trades)
}
