use std::collections::HashMap;

use alloy::{
    primitives::{Address, TxHash, U256},
    providers::{ext::DebugApi, Provider},
    rpc::types::trace::geth::{CallConfig, CallFrame, GethDebugTracingOptions, GethTrace},
};
use anyhow::Context;

/// Fetch the callTracer root frame for a transaction.
///
/// The trace is the only place native ETH transfers and the internal
/// aggregator call appear — neither emits a log.
pub(crate) async fn fetch_trace<P: Provider>(
    provider: &P,
    tx_hash: TxHash,
) -> anyhow::Result<CallFrame> {
    let options = GethDebugTracingOptions::call_tracer(CallConfig::default());
    let trace = provider
        .debug_trace_transaction(tx_hash, options)
        .await
        .with_context(|| {
            format!("failed to trace {tx_hash} (does the RPC support debug_traceTransaction?)")
        })?;

    let GethTrace::CallTracer(root) = trace else {
        anyhow::bail!("expected callTracer output for {tx_hash}");
    };
    Ok(root)
}

/// Walk the call frames, collecting native ETH value transfers.
///
/// Reverted frames (and their subtrees) move no value and are skipped.
/// `DELEGATECALL`/`STATICCALL` report an apparent value but transfer
/// nothing, so only genuine value-bearing call types are counted.
pub(crate) fn collect_native_transfers(frame: &CallFrame, out: &mut Vec<(Address, Address, U256)>) {
    if frame.error.is_some() {
        return;
    }

    if transfers_value(&frame.typ) {
        if let (Some(to), Some(value)) = (frame.to, frame.value) {
            if value > U256::ZERO {
                out.push((frame.from, to, value));
            }
        }
    }

    for child in &frame.calls {
        collect_native_transfers(child, out);
    }
}

/// Whether a call type actually moves ETH from caller to callee.
fn transfers_value(call_type: &str) -> bool {
    matches!(call_type, "CALL" | "CALLCODE" | "CREATE" | "CREATE2" | "SELFDESTRUCT")
}

/// Attribute the aggregator that settled the swap.
///
/// A direct swap (the entry point is itself an aggregator) settles there.
/// Otherwise the entry point is a client (e.g. Relay) that routes through an
/// aggregator found in the trace: a known router if recognized, else the
/// external contract the client called that moved the most value.
pub(crate) fn attribute_aggregator(
    root: &CallFrame,
    entry_point: Address,
    sender: Address,
    aggregators: &HashMap<Address, &'static str>,
) -> Option<Address> {
    if aggregators.contains_key(&entry_point) {
        return Some(entry_point);
    }
    if let Some(found) = find_known_aggregator(root, aggregators) {
        return Some(found);
    }
    largest_external_call(root, entry_point, sender)
}

/// Depth-first search for the first call into a known aggregator, skipping
/// reverted frames (and their subtrees), which settle nothing.
fn find_known_aggregator(
    frame: &CallFrame,
    aggregators: &HashMap<Address, &'static str>,
) -> Option<Address> {
    if frame.error.is_some() {
        return None;
    }
    if let Some(to) = frame.to {
        if aggregators.contains_key(&to) {
            return Some(to);
        }
    }
    for child in &frame.calls {
        if let Some(found) = find_known_aggregator(child, aggregators) {
            return Some(found);
        }
    }
    None
}

/// The client's direct child call that moved the most value, excluding
/// self-calls and refunds to the sender. Best guess at an unknown router.
fn largest_external_call(
    root: &CallFrame,
    entry_point: Address,
    sender: Address,
) -> Option<Address> {
    let mut best: Option<(Address, U256)> = None;
    for child in &root.calls {
        if child.error.is_some() {
            continue;
        }
        let Some(to) = child.to else { continue };
        if to == entry_point || to == sender {
            continue;
        }
        let value = child.value.unwrap_or_default();
        if best.is_none_or(|(_, best_value)| value > best_value) {
            best = Some((to, value));
        }
    }
    best.map(|(to, _)| to)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::{
        registry::{known_aggregators, known_names, label},
        test_utils::{addr, frame},
    };

    #[test]
    fn native_transfers_skip_delegatecall_and_staticcall() {
        let from = addr(1);
        let to = addr(2);

        let mut root = frame("CALL", from, to, 1000);
        root.calls =
            vec![frame("DELEGATECALL", to, addr(3), 1000), frame("STATICCALL", to, addr(4), 1000)];

        let mut out = Vec::new();
        collect_native_transfers(&root, &mut out);
        assert_eq!(out, vec![(from, to, U256::from(1000))]);
    }

    #[test]
    fn reverted_frame_and_subtree_ignored() {
        let from = addr(1);
        let to = addr(2);

        let mut reverted = frame("CALL", to, addr(3), 5000);
        reverted.error = Some("execution reverted".to_string());
        reverted.calls = vec![frame("CALL", addr(3), addr(4), 5000)];

        let mut root = frame("CALL", from, to, 1000);
        root.calls = vec![reverted];

        let mut out = Vec::new();
        collect_native_transfers(&root, &mut out);
        assert_eq!(out, vec![(from, to, U256::from(1000))]);
    }

    #[test]
    fn direct_swap_attributes_entry_point() {
        let aggregators = known_aggregators();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let root = frame("CALL", addr(1), oneinch, 0);
        assert_eq!(attribute_aggregator(&root, oneinch, addr(1), &aggregators), Some(oneinch));
    }

    #[test]
    fn relay_attributes_internal_aggregator() {
        // Mirrors the real Relay tx: the client router calls 0x's AllowanceHolder.
        // root(relay) -> [ relay (self-call), 0x AllowanceHolder (the aggregator) ]
        let aggregators = known_aggregators();
        let sender = addr(1);
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut root = frame("CALL", sender, relay, 0);
        root.calls = vec![frame("CALL", relay, relay, 0), frame("CALL", relay, zerox, 1000)];

        let found = attribute_aggregator(&root, relay, sender, &aggregators).unwrap();
        assert_eq!(found, zerox);
        assert_eq!(label(found, &known_names()), "0x");
    }

    #[test]
    fn relay_attributes_tycho_router() {
        // Real tx 0x8b461c…: Relay ApprovalProxy -> Relay router -> Tycho router.
        // The settling venue is Tycho even though it sits two levels deep.
        let aggregators = known_aggregators();
        let sender = addr(1);
        let relay_proxy = address!("0xccc88a9d1b4ed6b0eaba998850414b24f1c315be");
        let relay_router = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");
        let tycho = address!("0x1f8db310f32d48b6180ff902ec60c586128cef47");

        let mut router_call = frame("CALL", relay_proxy, relay_router, 0);
        router_call.calls = vec![frame("CALL", relay_router, tycho, 0)];
        let mut root = frame("CALL", sender, relay_proxy, 0);
        root.calls = vec![router_call];

        let found = attribute_aggregator(&root, relay_proxy, sender, &aggregators).unwrap();
        assert_eq!(found, tycho);
        assert_eq!(label(found, &known_names()), "tycho");
    }

    #[test]
    fn unknown_aggregator_falls_back_to_largest_external_call() {
        // No known aggregator in the trace: pick the largest external call,
        // skipping the client self-call and the refund back to the sender.
        let aggregators = known_aggregators();
        let sender = addr(1);
        let client = addr(2);
        let unknown_router = addr(50);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![
            frame("CALL", client, client, 0),            // self-call, skipped
            frame("CALL", client, sender, 9000),         // refund to sender, skipped
            frame("CALL", client, addr(51), 10),         // small external call
            frame("CALL", client, unknown_router, 5000), // largest external call
        ];

        let found = attribute_aggregator(&root, client, sender, &aggregators).unwrap();
        assert_eq!(found, unknown_router);
        assert_eq!(label(found, &known_names()), unknown_router.to_string());
    }

    #[test]
    fn find_known_aggregator_skips_reverted() {
        let aggregators = known_aggregators();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");

        let mut reverted = frame("CALL", addr(2), oneinch, 0);
        reverted.error = Some("execution reverted".to_string());
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![reverted];

        assert_eq!(find_known_aggregator(&root, &aggregators), None);
    }
}
