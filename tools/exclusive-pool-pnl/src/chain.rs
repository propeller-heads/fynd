//! Reads the pool's history from an Ethereum archive node.
//!
//! Discovery starts at the extension rather than at Ekubo core: the extension indexes the pool id
//! as topic1, so one `eth_getLogs` filter returns every interaction with this pool and nothing
//! else. Ekubo core emits its swap event with `log0` (no topics at all), which cannot be filtered
//! server-side, so it is decoded from the receipts of the transactions the extension points at.

use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use alloy::{
    consensus::Transaction as _,
    eips::BlockNumberOrTag,
    network::Ethereum,
    primitives::TxHash,
    providers::{Provider, RootProvider},
    rpc::types::Filter,
};
use anyhow::{anyhow, Context, Result};
use futures::stream::{StreamExt as _, TryStreamExt as _};
use tracing::{debug, warn};

use crate::pool::{EKUBO_CORE, EXTENSION, FEES_ACCUMULATED_TOPIC, INTERACTION_TOPIC, POOL_ID};

/// Lowest plausible signed-swap deadline, used to reject false calldata matches.
const MIN_PLAUSIBLE_DEADLINE: u32 = 1_600_000_000;

/// Delay before the first retry; doubles on each further attempt.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Tuning for one scan of the pool's history.
#[derive(Debug, Clone, Copy)]
pub struct ScanConfig {
    /// First block to scan, inclusive.
    pub from_block: u64,
    /// Last block to scan, inclusive.
    pub to_block: u64,
    /// Block span of a single `eth_getLogs` call.
    pub chunk: u64,
    /// Maximum in-flight requests.
    pub concurrency: usize,
    /// Attempts per request before giving up. Managed nodes throttle under concurrency.
    pub retries: u32,
}

/// Runs `op` until it succeeds or `attempts` are exhausted, backing off between tries.
async fn with_retry<T, F, Fut>(attempts: u32, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = RETRY_BACKOFF;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                debug!("rpc attempt {} failed: {error:#}", attempt + 1);
                last = Some(error);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("retry loop ran no attempts")))
}

/// The trade the pool's curve executed, from the pool's point of view.
///
/// A negative delta means the pool paid that token out. The deltas are gross: the extension's fee
/// is carved out of the output *after* the curve runs, so it is not reflected here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveTrade {
    /// Pool-side token0 delta.
    pub delta0: i128,
    /// Pool-side token1 delta.
    pub delta1: i128,
}

/// The fee the Fynd controller signed into `user_data` for one swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedFee {
    /// Fee rate as 0.32 fixed point, charged on the swap's output token.
    pub fee_q32: u32,
    /// Unix second the signature expires at.
    pub deadline: u32,
}

/// One transaction that touched the pool: a swap, or a position update.
#[derive(Debug, Clone)]
pub struct Interaction {
    pub block: u64,
    pub timestamp: u64,
    pub tx: TxHash,
    /// `None` for position updates, which emit the same extension topic as swaps.
    pub trade: Option<CurveTrade>,
    /// Rate signed for this swap, absent when the calldata carries no recognisable signed hop.
    pub signed_fee: Option<SignedFee>,
    /// token0 fees credited to LPs in this transaction.
    pub fees_credited0: u128,
    /// token1 fees credited to LPs in this transaction.
    pub fees_credited1: u128,
}

impl Interaction {
    /// Whether this interaction actually moved the curve.
    pub fn is_swap(&self) -> bool {
        self.trade.is_some()
    }
}

/// Reads `n` big-endian bytes at `offset` as a signed 128-bit integer.
fn read_i128(data: &[u8], offset: usize) -> i128 {
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&data[offset..offset + 16]);
    i128::from_be_bytes(buf)
}

/// Reads a 32-byte big-endian word at `offset`, saturating into `u128`.
fn read_u128_word(data: &[u8], offset: usize) -> u128 {
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&data[offset + 16..offset + 32]);
    u128::from_be_bytes(buf)
}

/// Decodes Ekubo core's untopiced swap event for this pool.
///
/// Wire layout: `extension(20) | poolId(32) | delta0(16) | delta1(16) | sqrtRatioAfter(16) |
/// liquidityAfter(16)`. Returns `None` for any other pool or event.
pub fn decode_core_swap(data: &[u8]) -> Option<CurveTrade> {
    if data.len() < 116 || data[..20] != EXTENSION[..] || data[20..52] != POOL_ID[..] {
        return None;
    }
    Some(CurveTrade { delta0: read_i128(data, 52), delta1: read_i128(data, 68) })
}

/// Decodes Ekubo core's `FeesAccumulated` payload for this pool.
///
/// Wire layout: `poolId(32) | amount0(32) | amount1(32)`. Returns `None` for any other pool.
pub fn decode_fees_accumulated(data: &[u8]) -> Option<(u128, u128)> {
    if data.len() < 96 || data[..32] != POOL_ID[..] {
        return None;
    }
    Some((read_u128_word(data, 32), read_u128_word(data, 64)))
}

/// Extracts every signed-swap fee the calldata carries for this pool's extension, in wire order.
///
/// `EkuboV3SwapEncoder` lays each hop out as `extension(20) | fee(8) | pool_type_config(4) |
/// meta(32) | …`, and packs the controller's rate into `meta[4..8]`. The extension address is
/// matched as a byte pattern, so a match is confirmed by requiring the meta's leading deadline to
/// be a plausible Unix second; random calldata almost never satisfies that.
pub fn decode_signed_fees(calldata: &[u8]) -> Vec<SignedFee> {
    let mut found = Vec::new();
    let mut i = 0usize;
    while i + 64 <= calldata.len() {
        if calldata[i..i + 20] != EXTENSION[..] {
            i += 1;
            continue;
        }
        let meta = &calldata[i + 32..i + 64];
        let deadline = u32::from_be_bytes([meta[0], meta[1], meta[2], meta[3]]);
        if deadline >= MIN_PLAUSIBLE_DEADLINE {
            let fee_q32 = u32::from_be_bytes([meta[4], meta[5], meta[6], meta[7]]);
            found.push(SignedFee { fee_q32, deadline });
        }
        i += 20;
    }
    found
}

/// Fetches every interaction with the pool in `config`'s block range, in chain order.
///
/// Public and managed nodes commonly cap `eth_getLogs` at 1 000 blocks per call and throttle
/// under concurrency, so the scan is chunked and every request is retried.
pub async fn fetch_interactions(
    provider: &RootProvider<Ethereum>,
    config: ScanConfig,
) -> Result<Vec<Interaction>> {
    let ScanConfig { from_block, to_block, chunk, concurrency, retries } = config;
    let ranges: Vec<(u64, u64)> = (from_block..=to_block)
        .step_by(chunk as usize)
        .map(|start| (start, (start + chunk - 1).min(to_block)))
        .collect();
    debug!("scanning {} block ranges of up to {chunk} blocks", ranges.len());

    let logs = futures::stream::iter(
        ranges
            .into_iter()
            .map(|(start, end)| async move {
                with_retry(retries, || async move {
                    let filter = Filter::new()
                        .address(EXTENSION)
                        .event_signature(INTERACTION_TOPIC)
                        .topic1(POOL_ID)
                        .from_block(BlockNumberOrTag::Number(start))
                        .to_block(BlockNumberOrTag::Number(end));
                    provider
                        .get_logs(&filter)
                        .await
                        .map_err(anyhow::Error::from)
                })
                .await
                .with_context(|| format!("eth_getLogs failed for blocks {start}..={end}"))
            }),
    )
    .buffer_unordered(concurrency)
    .try_collect::<Vec<_>>()
    .await?;

    let mut refs: Vec<(u64, u64, TxHash, Option<u64>)> = logs
        .into_iter()
        .flatten()
        .filter_map(|log| {
            Some((log.block_number?, log.log_index?, log.transaction_hash?, log.block_timestamp))
        })
        .collect();
    refs.sort_unstable_by_key(|(block, index, _, _)| (*block, *index));

    let timestamps = resolve_timestamps(provider, &refs, config).await?;
    let hashes: Vec<(u64, TxHash)> = refs
        .into_iter()
        .map(|(block, _, tx, _)| (block, tx))
        .collect();

    futures::stream::iter(hashes.into_iter().map(|(block, tx)| {
        let timestamp = timestamps
            .get(&block)
            .copied()
            .unwrap_or_default();
        async move { load_interaction(provider, block, timestamp, tx, retries).await }
    }))
    .buffered(concurrency)
    .try_collect()
    .await
}

/// Fills in block timestamps the node omitted from its log payloads.
async fn resolve_timestamps(
    provider: &RootProvider<Ethereum>,
    refs: &[(u64, u64, TxHash, Option<u64>)],
    config: ScanConfig,
) -> Result<BTreeMap<u64, u64>> {
    let mut known = BTreeMap::new();
    let mut missing = HashSet::new();
    for (block, _, _, timestamp) in refs {
        match timestamp {
            Some(t) => {
                known.insert(*block, *t);
            }
            None => {
                missing.insert(*block);
            }
        }
    }
    if missing.is_empty() {
        return Ok(known);
    }
    let fetched = futures::stream::iter(
        missing
            .into_iter()
            .map(|block| async move {
                with_retry(config.retries, || async move {
                    let header = provider
                        .get_block_by_number(BlockNumberOrTag::Number(block))
                        .await?
                        .with_context(|| format!("block {block} not found"))?;
                    anyhow::Ok((block, header.header.timestamp))
                })
                .await
            }),
    )
    .buffered(config.concurrency)
    .try_collect::<Vec<_>>()
    .await?;
    known.extend(fetched);
    Ok(known)
}

/// Builds one [`Interaction`] from its receipt and calldata.
async fn load_interaction(
    provider: &RootProvider<Ethereum>,
    block: u64,
    timestamp: u64,
    tx: TxHash,
    retries: u32,
) -> Result<Interaction> {
    let receipt = with_retry(retries, || async move {
        provider
            .get_transaction_receipt(tx)
            .await?
            .with_context(|| format!("receipt missing for {tx}"))
    })
    .await?;

    let mut trades = Vec::new();
    let mut fees_credited0 = 0u128;
    let mut fees_credited1 = 0u128;
    for log in receipt.inner.logs() {
        if log.address() != EKUBO_CORE {
            continue;
        }
        let data = log.data().data.as_ref();
        match log.topics().first() {
            None => trades.extend(decode_core_swap(data)),
            Some(&topic) if topic == FEES_ACCUMULATED_TOPIC => {
                if let Some((fee0, fee1)) = decode_fees_accumulated(data) {
                    fees_credited0 += fee0;
                    fees_credited1 += fee1;
                }
            }
            Some(_) => {}
        }
    }
    if trades.len() > 1 {
        warn!(%tx, "transaction holds {} swaps on the pool; reporting the first", trades.len());
    }

    let transaction = with_retry(retries, || async move {
        provider
            .get_transaction_by_hash(tx)
            .await?
            .with_context(|| format!("transaction {tx} not found"))
    })
    .await?;
    let signed_fee = decode_signed_fees(transaction.input())
        .into_iter()
        .next();

    Ok(Interaction {
        block,
        timestamp,
        tx,
        trade: trades.into_iter().next(),
        signed_fee,
        fees_credited0,
        fees_credited1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ekubo core swap log from tx 0x162dff8f…98a46b (1 522.185 USDC in, 0.651354 ETH out).
    const SWAP_LOG: &str = concat!(
        "55b703eed01b35641963da2fb2e14885993605a3",
        "9efd723a29a4e7f40c955f7b968144a5e2a7261a4a0f1573fbb0e2653600e4a4",
        "fffffffffffffffff6f5ecf4a82780ed",
        "0000000000000000000000005abab328",
        "4000cb44275e839ff9427e9cfed0d0f3",
        "000000000000000000167d314653bf35",
    );

    /// The signed Ekubo hop from the same transaction's calldata.
    const SIGNED_HOP: &str = concat!(
        "55b703eed01b35641963da2fb2e14885993605a3", // extension
        "0000000000000000",                         // pool config fee
        "80000400",                                 // pool_type_config
        "6a88a3010742c560000000006a88915900000000000000000000000000000000", // meta
    );

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn decodes_curve_deltas_from_a_real_swap_log() {
        let trade = decode_core_swap(&bytes(SWAP_LOG)).expect("pool swap");
        assert_eq!(trade.delta1, 1_522_185_000);
        assert_eq!(trade.delta0, -651_354_035_547_832_083);
    }

    #[test]
    fn rejects_a_swap_log_from_another_pool() {
        let mut data = bytes(SWAP_LOG);
        data[20] ^= 0xff;
        assert_eq!(decode_core_swap(&data), None);
    }

    #[test]
    fn rejects_a_truncated_swap_log() {
        let data = bytes(SWAP_LOG);
        assert_eq!(decode_core_swap(&data[..115]), None);
    }

    #[test]
    fn decodes_fees_accumulated_for_this_pool() {
        let mut data = bytes(&POOL_ID.to_string()[2..]);
        data.extend(std::iter::repeat_n(0, 16));
        data.extend(23_667_994_721_360u128.to_be_bytes());
        data.extend(std::iter::repeat_n(0, 32));
        assert_eq!(decode_fees_accumulated(&data), Some((23_667_994_721_360, 0)));
    }

    #[test]
    fn ignores_fees_accumulated_for_another_pool() {
        let data = vec![0u8; 96];
        assert_eq!(decode_fees_accumulated(&data), None);
    }

    #[test]
    fn extracts_the_signed_fee_from_a_real_hop() {
        let fees = decode_signed_fees(&bytes(SIGNED_HOP));
        assert_eq!(fees, vec![SignedFee { fee_q32: 0x0742_c560, deadline: 0x6a88_a301 }]);
    }

    #[test]
    fn extracts_one_entry_per_hop() {
        let calldata = bytes(&format!("{SIGNED_HOP}{SIGNED_HOP}"));
        assert_eq!(decode_signed_fees(&calldata).len(), 2);
    }

    #[test]
    fn skips_an_extension_match_without_a_plausible_deadline() {
        let mut data = bytes(SIGNED_HOP);
        data[32..36].copy_from_slice(&0u32.to_be_bytes());
        assert!(decode_signed_fees(&data).is_empty());
    }

    #[test]
    fn ignores_an_extension_match_too_close_to_the_end() {
        let data = bytes("55b703eed01b35641963da2fb2e14885993605a3");
        assert!(decode_signed_fees(&data).is_empty());
    }
}
