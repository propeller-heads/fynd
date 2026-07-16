use alloy::{
    primitives::{Address, TxHash, U256},
    providers::{ext::DebugApi, Provider},
    rpc::types::trace::geth::{CallConfig, CallFrame, GethDebugTracingOptions, GethTrace},
};
use anyhow::Context;

use crate::decoder::registry::Registry;

/// Fetch the callTracer root frame for a transaction.
///
/// The trace is the only place native ETH transfers and the internal
/// solver call appear — neither emits a log.
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

/// Gas consumed by the settled route inside a venue-wrapped transaction (Relay, `MetaMask`), in
/// gas units.
///
/// The venue's own gas — fee skim, forwarding, the base transaction cost — is charged whichever
/// router the venue picks, so like the venue fee it is excluded from the comparison. Each trace
/// frame's `gas_used` includes its whole subtree, so the call into the solver carries the full
/// routing cost. Prefers the first call into a known solver; falls back to the most
/// gas-consuming direct child, since in a wrapped transaction the routing work dwarfs the
/// bookkeeping calls. `None` when no usable frame exists — the caller then skips the gas
/// deduction rather than guess.
pub(crate) fn route_gas(root: &CallFrame, registry: &Registry) -> Option<U256> {
    if let Some(frame) = find_solver_frame(root, registry) {
        return Some(frame.gas_used);
    }
    root.calls
        .iter()
        .filter(|child| child.error.is_none())
        .map(|child| child.gas_used)
        .max()
        .filter(|gas| !gas.is_zero())
}

/// Depth-first search for the first call frame into a known solver, skipping reverted frames
/// (and their subtrees), which settle nothing.
///
/// The one walk serves both questions asked of a trace: *who* settled the swap (the frame's
/// `to`, for attribution) and *what the route cost* (the frame's `gas_used`, for gas
/// accounting) — so the gas charged is always the gas of the exact frame the solver label came
/// from.
pub(crate) fn find_solver_frame<'a>(
    frame: &'a CallFrame,
    registry: &Registry,
) -> Option<&'a CallFrame> {
    if frame.error.is_some() {
        return None;
    }
    if let Some(to) = frame.to {
        if registry.is_solver(to) {
            return Some(frame);
        }
    }
    frame
        .calls
        .iter()
        .find_map(|child| find_solver_frame(child, registry))
}

/// Best guess at an unknown router: the entry point's direct child call that moved the most
/// native value, excluding self-calls, refunds to the sender, and the registry's infrastructure
/// addresses (Permit2, the wrapped-native contract).
///
/// The wrapped-native exclusion matters most: on an ETH-input swap through an unknown router,
/// the highest-value call is typically the `WETH.deposit()` wrapping the input — infrastructure,
/// not a solver (seen live: 83 run6 records attributed to the WETH contract).
///
/// Returns `None` when no candidate moved any value: on a token→token swap every call carries
/// zero value, so "largest" would degenerate to the first child — typically the token pull, not
/// the venue. The caller then labels the trade with its entry point instead.
pub(crate) fn largest_external_call(
    root: &CallFrame,
    entry_point: Address,
    sender: Address,
    registry: &Registry,
) -> Option<Address> {
    let mut best: Option<(Address, U256)> = None;
    for child in &root.calls {
        if child.error.is_some() {
            continue;
        }
        let Some(to) = child.to else { continue };
        if to == entry_point || to == sender || registry.is_infrastructure(to) {
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

    fn with_gas(mut call: CallFrame, gas_used: u64) -> CallFrame {
        call.gas_used = U256::from(gas_used);
        call
    }

    #[test]
    fn route_gas_reads_known_venue_frame() {
        // Mirrors the audited Relay tx 0xf25ceafd…: two small wrapper self-calls around the
        // KyberSwap router call, whose frame carries the full routing cost.
        //
        //   relay (1,271,689 total)
        //   ├── relay self-call        15,066
        //   ├── kyberswap router    1,067,571   <- the route
        //   └── relay self-call        18,802
        let registry = Registry::ethereum();
        let sender = addr(1);
        let relay = addr(2);
        let kyber = address!("0x6131b5fae19ea4f9d964eac0408e4408b66337b5");

        let mut root = with_gas(frame("CALL", sender, relay, 0), 1_271_689);
        root.calls = vec![
            with_gas(frame("CALL", relay, relay, 0), 15_066),
            with_gas(frame("CALL", relay, kyber, 0), 1_067_571),
            with_gas(frame("CALL", relay, relay, 0), 18_802),
        ];

        assert_eq!(route_gas(&root, &registry), Some(U256::from(1_067_571u64)));
    }

    #[test]
    fn route_gas_falls_back_to_largest_child() {
        // Unknown venue: no registry match, so the most gas-consuming child is the route.
        let registry = Registry::ethereum();
        let client = addr(2);

        let mut root = with_gas(frame("CALL", addr(1), client, 0), 500_000);
        root.calls = vec![
            with_gas(frame("CALL", client, addr(50), 0), 30_000),
            with_gas(frame("CALL", client, addr(51), 0), 400_000),
        ];

        assert_eq!(route_gas(&root, &registry), Some(U256::from(400_000u64)));
    }

    #[test]
    fn route_gas_skips_reverted_and_declines_empty() {
        let registry = Registry::ethereum();
        let client = addr(2);

        let mut reverted = with_gas(frame("CALL", client, addr(50), 0), 400_000);
        reverted.error = Some("execution reverted".to_string());
        let mut root = with_gas(frame("CALL", addr(1), client, 0), 500_000);
        root.calls = vec![reverted];
        assert_eq!(route_gas(&root, &registry), None);

        let leaf = frame("CALL", addr(1), client, 0);
        assert_eq!(route_gas(&leaf, &registry), None);
    }

    #[test]
    fn route_gas_real_relay_kyberswap_trace() {
        // Real callTracer output of tx 0xf25ceafd… (block 25480207, 39.67 ETH -> USDT via
        // Relay + KyberSwap), payload fields stripped. The route's gas is the KyberSwap router
        // frame; Relay's wrapper overhead (1,271,689 total) stays out.
        let root: CallFrame =
            serde_json::from_str(include_str!("fixtures/trace_relay_kyberswap_0xf25ceafd.json"))
                .unwrap();
        assert_eq!(root.gas_used, U256::from(1_271_689u64));
        assert_eq!(route_gas(&root, &Registry::ethereum()), Some(U256::from(1_067_571u64)));
    }

    #[test]
    fn route_gas_real_metamask_oneinch_trace() {
        // Real callTracer output of tx 0xe815e2b5… (block 25476433, a $3.4k MetaMask swap
        // routed via 1inch), payload fields stripped. The 1inch frame sits three levels deep:
        //
        //   metamask router        185,699
        //   └── spender            180,406   <- largest child: wrapper, NOT the route
        //       └── adapter        175,635   (delegatecall)
        //           ├── 1inch v6   115,795   <- the route
        //           └── fee wallet   6,329   (MetaMask's skim, correctly excluded)
        //
        // so the known-venue search must win over the largest-child fallback.
        let root: CallFrame =
            serde_json::from_str(include_str!("fixtures/trace_metamask_1inch_0xe815e2b5.json"))
                .unwrap();
        assert_eq!(root.gas_used, U256::from(185_699u64));
        assert_eq!(route_gas(&root, &Registry::ethereum()), Some(U256::from(115_795u64)));
    }

    #[test]
    fn find_solver_frame_skips_reverted() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");

        let mut reverted = frame("CALL", addr(2), oneinch, 0);
        reverted.error = Some("execution reverted".to_string());
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![reverted];

        assert!(find_solver_frame(&root, &registry).is_none());
    }
}
