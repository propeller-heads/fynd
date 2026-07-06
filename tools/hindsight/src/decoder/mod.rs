mod net;
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

pub(crate) use crate::decoder::registry::Registry;
use crate::decoder::{
    net::{received_nft, wrap_pair_mispaired},
    trace::{attribute_aggregator, collect_native_transfers, fetch_trace},
    venues::{started_bridge_order, Matched, Strategy},
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
            .filter_map(|receipt| venues::select(receipt, registry))
            .filter(|matched| {
                // A cross-chain bridge order's real output lands on the destination chain; the
                // same-chain flow (deposit in, leftover refund out) is not a swap. Filtered
                // before tracing, so bridge orders cost no trace call.
                if started_bridge_order(matched.receipt.logs()) {
                    debug!(
                        tx = %matched.receipt.transaction_hash,
                        client = %registry.label(matched.entry_point),
                        "cross-chain bridge order; skipping"
                    );
                    return false;
                }
                true
            })
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

            let flow = match strategy {
                Strategy::Sender => venues::sender_flow(logs, &native, sender, entry_point),
                Strategy::Maker => {
                    venues::intent::find_maker_trade(
                        provider,
                        logs,
                        &native,
                        &[entry_point, sender],
                        registry,
                        code_cache,
                    )
                    .await
                }
                Strategy::Relay => {
                    venues::relay::decode(logs, &native, sender, entry_point, registry)
                }
            };

            let Some(flow) = flow else {
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

            let aggregator =
                attribute_aggregator(&root, entry_point, sender, registry).unwrap_or(entry_point);

            trades.push(DecodedTrade {
                tx_hash: receipt.transaction_hash,
                block_number,
                client: registry.label(entry_point),
                aggregator: registry.label(aggregator),
                sender: flow.tracked,
                token_in: flow.swap.token_in,
                token_out: flow.swap.token_out,
                amount_in: flow.swap.amount_in,
                amount_out: flow.swap.amount_out,
                client_fee: flow.client_fee,
                client_fee_out: flow.client_fee_out,
            });
        }

        Ok(trades)
    }
}
