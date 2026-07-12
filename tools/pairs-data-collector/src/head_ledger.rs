//! Independent Ethereum head subscription, gap detection, and canonicality tracking.

use std::{collections::BTreeMap, time::Duration};

use alloy::{
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::Header,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::record::{BlockStatusEvent, CanonicalStatus, SCHEMA_VERSION};

/// Full header metadata required by the collector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedHead {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: String,
    /// Parent hash.
    pub parent_hash: String,
    /// Unix block timestamp.
    pub timestamp: u64,
    /// EIP-1559 base fee.
    pub base_fee_per_gas: Option<u64>,
    /// Local wall-clock receipt time.
    pub received_at_ms: u64,
    /// Non-secret endpoint identifier.
    pub rpc_endpoint_id: String,
}

/// Effects of adding one head to the ledger.
#[derive(Debug, Default)]
pub struct LedgerUpdate {
    /// Heights absent between the previous maximum and this head.
    pub missing_numbers: Vec<u64>,
    /// Append-only observed, reorg, and confirmation transitions.
    pub status_events: Vec<BlockStatusEvent>,
    /// Whether this exact number/hash had not been seen before.
    pub is_new: bool,
}

/// In-memory canonicality state rebuilt from the durable event stream on restart.
pub struct HeadLedger {
    confirmation_depth: u64,
    highest_number: Option<u64>,
    hash_by_number: BTreeMap<u64, String>,
    pending: BTreeMap<u64, String>,
}

impl HeadLedger {
    /// Create an empty ledger.
    pub fn new(confirmation_depth: u64) -> Self {
        Self {
            confirmation_depth,
            highest_number: None,
            hash_by_number: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    /// Rebuild canonicality state from durable append-only events.
    pub fn restore(
        confirmation_depth: u64,
        events: impl IntoIterator<Item = BlockStatusEvent>,
    ) -> Self {
        let mut ledger = Self::new(confirmation_depth);
        for event in events {
            ledger.highest_number = Some(
                ledger
                    .highest_number
                    .map_or(event.block_number, |number| number.max(event.block_number)),
            );
            match event.status {
                CanonicalStatus::Observed | CanonicalStatus::PotentiallyOrphaned => {
                    ledger
                        .hash_by_number
                        .insert(event.block_number, event.block_hash.clone());
                    ledger
                        .pending
                        .insert(event.block_number, event.block_hash);
                }
                CanonicalStatus::Canonical => {
                    ledger
                        .hash_by_number
                        .insert(event.block_number, event.block_hash);
                    ledger
                        .pending
                        .remove(&event.block_number);
                }
                CanonicalStatus::Orphaned => {
                    if ledger
                        .hash_by_number
                        .get(&event.block_number) ==
                        Some(&event.block_hash)
                    {
                        ledger
                            .hash_by_number
                            .remove(&event.block_number);
                    }
                    ledger
                        .pending
                        .remove(&event.block_number);
                }
            }
        }
        ledger
    }

    /// Observe one head and derive gap and canonicality events.
    pub fn observe(&mut self, head: &ObservedHead) -> LedgerUpdate {
        let mut update = LedgerUpdate::default();
        if let Some(highest) = self.highest_number {
            if head.number > highest + 1 {
                update.missing_numbers = ((highest + 1)..head.number).collect();
            }
        }

        match self.hash_by_number.get(&head.number) {
            Some(existing) if existing == &head.hash => return update,
            Some(existing) => {
                update.status_events.push(status_event(
                    head.number,
                    existing,
                    CanonicalStatus::Orphaned,
                    head.received_at_ms,
                    Some(head.number),
                ));
                self.pending.remove(&head.number);
            }
            None => {}
        }

        self.hash_by_number
            .insert(head.number, head.hash.clone());
        self.pending
            .insert(head.number, head.hash.clone());
        self.highest_number = Some(
            self.highest_number
                .map_or(head.number, |n| n.max(head.number)),
        );
        update.is_new = true;
        update.status_events.push(status_event(
            head.number,
            &head.hash,
            CanonicalStatus::Observed,
            head.received_at_ms,
            None,
        ));
        self.confirm_pending(head, &mut update.status_events);
        update
    }

    fn confirm_pending(&mut self, head: &ObservedHead, events: &mut Vec<BlockStatusEvent>) {
        let confirmed_through = head
            .number
            .saturating_sub(self.confirmation_depth);
        let numbers: Vec<u64> = self
            .pending
            .range(..=confirmed_through)
            .map(|(number, _)| *number)
            .collect();
        for number in numbers {
            let Some(hash) = self.pending.remove(&number) else { continue };
            if self.hash_by_number.get(&number) == Some(&hash) {
                events.push(status_event(
                    number,
                    &hash,
                    CanonicalStatus::Canonical,
                    head.received_at_ms,
                    Some(head.number),
                ));
            }
        }
    }
}

fn status_event(
    block_number: u64,
    block_hash: &str,
    status: CanonicalStatus,
    status_changed_at_ms: u64,
    canonical_head: Option<u64>,
) -> BlockStatusEvent {
    BlockStatusEvent {
        schema_version: SCHEMA_VERSION,
        block_number,
        block_hash: block_hash.to_string(),
        status,
        status_changed_at_ms,
        canonical_head,
    }
}

/// Continuously reconnecting WebSocket `newHeads` source.
pub async fn run_head_subscription(
    ws_url: String,
    rpc_endpoint_id: String,
    sender: mpsc::Sender<ObservedHead>,
) -> Result<()> {
    loop {
        if let Err(error) = subscribe_once(&ws_url, &rpc_endpoint_id, &sender).await {
            if sender.is_closed() {
                return Ok(());
            }
            warn!(%error, "Ethereum head subscription disconnected; reconnecting");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn subscribe_once(
    ws_url: &str,
    rpc_endpoint_id: &str,
    sender: &mpsc::Sender<ObservedHead>,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_url))
        .await
        .with_context(|| format!("connecting Ethereum WebSocket endpoint {rpc_endpoint_id}"))?;
    let subscription = provider
        .subscribe_blocks()
        .await
        .context("subscribing to Ethereum newHeads")?;
    info!(rpc_endpoint_id, "Ethereum newHeads subscription connected");
    let mut stream = subscription.into_stream();
    while let Some(header) = stream.next().await {
        let head = observed_from_header(&header, rpc_endpoint_id);
        sender
            .send(head)
            .await
            .context("head consumer stopped")?;
    }
    anyhow::bail!("Ethereum newHeads stream ended")
}

fn observed_from_header(header: &Header, rpc_endpoint_id: &str) -> ObservedHead {
    ObservedHead {
        number: header.number,
        hash: format!("{:#x}", header.hash),
        parent_hash: format!("{:#x}", header.parent_hash),
        timestamp: header.timestamp,
        base_fee_per_gas: header.base_fee_per_gas,
        received_at_ms: now_ms(),
        rpc_endpoint_id: rpc_endpoint_id.to_string(),
    }
}

/// Minimal HTTP client used only to reconcile headers missed during WS disconnects.
pub struct RpcHeaderClient {
    client: reqwest::Client,
    url: String,
    endpoint_id: String,
}

impl RpcHeaderClient {
    /// Create a reconciliation client.
    pub fn new(url: String, endpoint_id: String) -> Self {
        Self { client: reqwest::Client::new(), url, endpoint_id }
    }

    /// Fetch a block header by number.
    pub async fn header(&self, number: u64) -> Result<ObservedHead> {
        let response: RpcResponse = self
            .client
            .post(&self.url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getBlockByNumber",
                "params": [format!("0x{number:x}"), false],
            }))
            .send()
            .await
            .with_context(|| format!("requesting block {number} from {}", self.endpoint_id))?
            .error_for_status()?
            .json()
            .await?;
        let block = response
            .result
            .with_context(|| format!("RPC returned no block for height {number}"))?;
        Ok(ObservedHead {
            number: parse_hex_u64(&block.number)?,
            hash: block.hash,
            parent_hash: block.parent_hash,
            timestamp: parse_hex_u64(&block.timestamp)?,
            base_fee_per_gas: block
                .base_fee_per_gas
                .as_deref()
                .map(parse_hex_u64)
                .transpose()?,
            received_at_ms: now_ms(),
            rpc_endpoint_id: self.endpoint_id.clone(),
        })
    }
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<RpcBlock>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcBlock {
    number: String,
    hash: String,
    parent_hash: String,
    timestamp: String,
    base_fee_per_gas: Option<String>,
}

fn parse_hex_u64(value: &str) -> Result<u64> {
    u64::from_str_radix(value.trim_start_matches("0x"), 16)
        .with_context(|| format!("invalid RPC hex quantity {value}"))
}

/// Current Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(number: u64, hash: &str) -> ObservedHead {
        ObservedHead {
            number,
            hash: hash.into(),
            parent_hash: "0xparent".into(),
            timestamp: number,
            base_fee_per_gas: Some(1),
            received_at_ms: number * 1_000,
            rpc_endpoint_id: "test".into(),
        }
    }

    #[test]
    fn ledger_reports_missing_heights_and_confirms_old_heads() {
        let mut ledger = HeadLedger::new(2);
        ledger.observe(&head(10, "0xa"));

        let update = ledger.observe(&head(13, "0xd"));

        assert_eq!(update.missing_numbers, vec![11, 12]);
        assert!(update
            .status_events
            .iter()
            .any(|event| {
                event.block_number == 10 && event.status == CanonicalStatus::Canonical
            }));
    }

    #[test]
    fn ledger_marks_replaced_same_height_hash_orphaned() {
        let mut ledger = HeadLedger::new(12);
        ledger.observe(&head(10, "0xold"));

        let update = ledger.observe(&head(10, "0xnew"));

        assert!(update
            .status_events
            .iter()
            .any(|event| {
                event.block_hash == "0xold" && event.status == CanonicalStatus::Orphaned
            }));
        assert!(update
            .status_events
            .iter()
            .any(|event| {
                event.block_hash == "0xnew" && event.status == CanonicalStatus::Observed
            }));
    }

    #[test]
    fn parses_rpc_hex_quantity() {
        assert_eq!(parse_hex_u64("0x2a").unwrap(), 42);
    }

    #[test]
    fn restored_ledger_detects_gap_after_restart() {
        let mut original = HeadLedger::new(2);
        let events = original
            .observe(&head(10, "0xa"))
            .status_events;
        let mut restored = HeadLedger::restore(2, events);

        let update = restored.observe(&head(12, "0xc"));

        assert_eq!(update.missing_numbers, vec![11]);
    }
}
