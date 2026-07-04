use alloy::{
    primitives::{address, Address, TxHash, U256},
    providers::{ext::DebugApi, Provider},
    rpc::types::trace::geth::{CallConfig, CallFrame, GethDebugTracingOptions, GethTrace},
};
use anyhow::Context;

use crate::decoder::registry::Registry;

/// The canonical Permit2 deployment (same address on every chain) — token-pull infrastructure
/// that routers call first, never the venue settling a swap.
const PERMIT2: Address = address!("0x000000000022d473030f116ddee9f6b43ac78ba3");

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
    registry: &Registry,
) -> Option<Address> {
    if registry.is_aggregator(entry_point) {
        return Some(entry_point);
    }
    if let Some(found) = find_known_aggregator(root, registry) {
        return Some(found);
    }
    largest_external_call(root, entry_point, sender)
}

/// Depth-first search for the first call into a known aggregator, skipping
/// reverted frames (and their subtrees), which settle nothing.
fn find_known_aggregator(frame: &CallFrame, registry: &Registry) -> Option<Address> {
    if frame.error.is_some() {
        return None;
    }
    if let Some(to) = frame.to {
        if registry.is_aggregator(to) {
            return Some(to);
        }
    }
    for child in &frame.calls {
        if let Some(found) = find_known_aggregator(child, registry) {
            return Some(found);
        }
    }
    None
}

/// The client's direct child call that moved the most native value, excluding
/// self-calls, refunds to the sender, and Permit2. Best guess at an unknown router.
///
/// Returns `None` when no candidate moved any value: on a token→token swap every call carries
/// zero value, so "largest" would degenerate to the first child — typically the token pull, not
/// the venue. The caller then labels the trade with its entry point instead.
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
        if to == entry_point || to == sender || to == PERMIT2 {
            continue;
        }
        let value = child.value.unwrap_or_default();
        if best.is_none_or(|(_, best_value)| value > best_value) {
            best = Some((to, value));
        }
    }
    match best {
        Some((to, value)) if value > U256::ZERO => Some(to),
        Some(_) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::test_utils::{addr, frame};

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
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let root = frame("CALL", addr(1), oneinch, 0);
        assert_eq!(attribute_aggregator(&root, oneinch, addr(1), &registry), Some(oneinch));
    }

    #[test]
    fn relay_attributes_internal_aggregator() {
        // Mirrors the real Relay tx: the client router calls 0x's AllowanceHolder.
        // root(relay) -> [ relay (self-call), 0x AllowanceHolder (the aggregator) ]
        let registry = Registry::ethereum();
        let sender = addr(1);
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut root = frame("CALL", sender, relay, 0);
        root.calls = vec![frame("CALL", relay, relay, 0), frame("CALL", relay, zerox, 1000)];

        let found = attribute_aggregator(&root, relay, sender, &registry).unwrap();
        assert_eq!(found, zerox);
        assert_eq!(registry.label(found), "0x");
    }

    #[test]
    fn relay_attributes_tycho_router() {
        // Real tx 0x8b461c…: Relay ApprovalProxy -> Relay router -> Tycho router.
        // The settling venue is Tycho even though it sits two levels deep.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let relay_proxy = address!("0xccc88a9d1b4ed6b0eaba998850414b24f1c315be");
        let relay_router = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");
        let tycho = address!("0x1f8db310f32d48b6180ff902ec60c586128cef47");

        let mut router_call = frame("CALL", relay_proxy, relay_router, 0);
        router_call.calls = vec![frame("CALL", relay_router, tycho, 0)];
        let mut root = frame("CALL", sender, relay_proxy, 0);
        root.calls = vec![router_call];

        let found = attribute_aggregator(&root, relay_proxy, sender, &registry).unwrap();
        assert_eq!(found, tycho);
        assert_eq!(registry.label(found), "tycho");
    }

    #[test]
    fn unknown_aggregator_falls_back_to_largest_external_call() {
        // No known aggregator in the trace: pick the largest external call,
        // skipping the client self-call and the refund back to the sender.
        let registry = Registry::ethereum();
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

        let found = attribute_aggregator(&root, client, sender, &registry).unwrap();
        assert_eq!(found, unknown_router);
        assert_eq!(registry.label(found), unknown_router.to_string());
    }

    #[test]
    fn attribution_declines_zero_value_fallback() {
        // Unknown venue, token->token swap: every child call moves zero value, so the fallback
        // would degenerate to the first child (the Permit2 token pull). Decline instead; the
        // caller labels the trade with its entry point.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![
            frame("CALL", client, PERMIT2, 0),  // token pull
            frame("CALL", client, addr(50), 0), // unknown venue, zero value
        ];

        assert_eq!(attribute_aggregator(&root, client, sender, &registry), None);
    }

    #[test]
    fn attribution_never_picks_permit2() {
        // Even when Permit2 is the highest-value direct call, it is infrastructure, not a venue.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);
        let venue = addr(50);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![frame("CALL", client, PERMIT2, 9000), frame("CALL", client, venue, 100)];

        assert_eq!(attribute_aggregator(&root, client, sender, &registry), Some(venue));
    }

    #[test]
    fn find_known_aggregator_skips_reverted() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");

        let mut reverted = frame("CALL", addr(2), oneinch, 0);
        reverted.error = Some("execution reverted".to_string());
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![reverted];

        assert_eq!(find_known_aggregator(&root, &registry), None);
    }
}
