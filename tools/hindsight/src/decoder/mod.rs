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

use crate::decoder::{
    match_receipts::{find_maker_trade, match_receipt, Matched},
    net::{decode_trade, fee_to_collectors},
    registry::{known_aggregators, known_fee_collectors, known_names, label},
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
    pub amount_out: U256,
    /// Client fee skimmed from the input token before swapping (e.g. Relay's fee), in `token_in`
    /// units. `None` when no known fee collector took a cut. Recorded for transparency; it is
    /// already excluded from `amount_in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_fee: Option<U256>,
}

/// Max concurrent trace requests per block. Bounds RPC load so a block
/// with many aggregator trades still completes within the block time
/// without tripping provider rate limits.
const TRACE_CONCURRENCY: usize = 10;

/// Decode aggregator trades from a block.
///
/// Fetches all receipts in one `eth_getBlockReceipts` call, then matches a
/// transaction two ways: its entry point (`tx.to`) is a known client or
/// aggregator, or one of its logs was emitted by a known aggregator. The
/// second case catches filler-initiated intent fills (UniswapX, 1inch
/// limit orders) where `tx.to` is a rotating filler. Matched transactions are
/// traced concurrently; the trace recovers native ETH flows and attributes
/// the settling aggregator.
pub(crate) async fn decode_block<P: Provider>(
    provider: &P,
    block_number: u64,
) -> anyhow::Result<Vec<DecodedTrade>> {
    let aggregators = known_aggregators();
    let names = known_names();
    let fee_collectors = known_fee_collectors();

    let receipts = provider
        .get_block_receipts(block_number.into())
        .await
        .with_context(|| format!("failed to fetch receipts for block {block_number}"))?
        .ok_or_else(|| anyhow::anyhow!("block {block_number} not found"))?;

    let matched: Vec<Matched> = receipts
        .iter()
        .filter_map(|receipt| match_receipt(receipt, names, aggregators))
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

    let mut code_cache = HashMap::new();
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
                names,
                &mut code_cache,
            )
            .await
        } else {
            decode_trade(logs, &native, sender)
                .map(|trade| (sender, trade))
                .or_else(|| {
                    decode_trade(logs, &native, entry_point).map(|trade| (entry_point, trade))
                })
        };

        let Some((tracked, (token_in, amount_in, token_out, amount_out))) = decoded else {
            warn!(
                tx = %receipt.transaction_hash,
                client = %label(entry_point, names),
                "no token or native ETH flow found"
            );
            continue;
        };

        let aggregator =
            attribute_aggregator(&root, entry_point, sender, aggregators).unwrap_or(entry_point);

        // Back out any input-side fee a known client collector skimmed before the swap, so the
        // re-solve compares Fynd against the amount actually routed (not the user's gross spend).
        let client_fee = fee_to_collectors(logs, &native, &fee_collectors)
            .get(&token_in)
            .copied()
            .filter(|fee| !fee.is_zero());
        let amount_in = client_fee.map_or(amount_in, |fee| amount_in.saturating_sub(fee));

        trades.push(DecodedTrade {
            tx_hash: receipt.transaction_hash,
            block_number,
            client: label(entry_point, names),
            aggregator: label(aggregator, names),
            sender: tracked,
            token_in,
            token_out,
            amount_in,
            amount_out,
            client_fee,
        });
    }

    Ok(trades)
}
