use std::collections::{HashMap, VecDeque};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, TxHash, U256},
    providers::{ext::DebugApi, Provider},
    rpc::types::trace::{
        common::TraceResult,
        geth::{CallConfig, CallFrame, GethDebugTracingOptions, GethTrace},
    },
};
use anyhow::Context;
use tracing::warn;

use crate::decoder::registry::Registry;

/// Fetch the callTracer root frame of every transaction in a block, keyed by transaction hash —
/// one `debug_traceBlockByNumber` call instead of one `debug_traceTransaction` per transaction.
///
/// The trace is the only place native ETH transfers and the internal solver call appear — neither
/// emits a log. A transaction the tracer could not process is dropped from the map with a warning
/// (its trades are lost, not the block's); a failure of the whole call is the block's error.
pub(crate) async fn fetch_block_traces<P: Provider>(
    provider: &P,
    block_number: u64,
) -> anyhow::Result<HashMap<TxHash, CallFrame>> {
    let options = GethDebugTracingOptions::call_tracer(CallConfig::default());
    let traces = provider
        .debug_trace_block_by_number(BlockNumberOrTag::Number(block_number), options)
        .await
        .with_context(|| {
            format!(
                "failed to trace block {block_number} \
                 (does the RPC support debug_traceBlockByNumber?)"
            )
        })?;

    let mut roots = HashMap::with_capacity(traces.len());
    for trace in traces {
        match trace {
            TraceResult::Success { result: GethTrace::CallTracer(root), tx_hash: Some(hash) } => {
                roots.insert(hash, root);
            }
            TraceResult::Success { result, tx_hash } => {
                warn!(?tx_hash, ?result, "expected callTracer output in the block trace");
            }
            TraceResult::Error { error, tx_hash } => {
                warn!(?tx_hash, error, "block trace failed for one transaction");
            }
        }
    }
    Ok(roots)
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

/// The outermost call frame into a known solver, skipping reverted frames (and their subtrees),
/// which settle nothing.
///
/// Breadth-first, so a solver that is a direct child wins over one buried deeper in an earlier
/// branch. Depth is what decides which frame settled the trade: the outer one called the inner
/// one, so the outer one owns the trade and the inner one is a step in its route. A depth-first
/// walk returned whichever solver frame it reached first going down — the same frame on a single
/// chain of calls, the wrong one when two frames sit on separate branches.
///
/// Only meaningful when the transaction has one leg. A transaction that entered a solver router
/// several times independently has no single settling frame, and `declared::declared_flow` declines
/// it on [`solver_frames`] before asking this — so the walk order decides a label, never an amount.
///
/// The one walk serves both questions asked of a trace: *who* settled the swap (the frame's
/// `to`, for attribution) and *what the route cost* (the frame's `gas_used`, for gas
/// accounting) — so the gas charged is always the gas of the exact frame the solver label came
/// from.
pub(crate) fn find_solver_frame<'a>(
    frame: &'a CallFrame,
    registry: &Registry,
) -> Option<&'a CallFrame> {
    let mut frames = solver_frames(frame, registry);
    frames.truncate(1);
    frames.pop()
}

/// Every outermost call frame into a known solver: the legs of the transaction. A solver called
/// from inside another solver's frame is a step in that solver's route, not a leg of its own, so a
/// matched frame's subtree is not searched.
///
/// One leg is the normal case. Several means the transaction swapped more than once, so no single
/// leg is the trade and `declared::declared_flow` declines to read one — an arbitrage contract
/// routing several legs through Uniswap's universal router is the shape seen live. This is not a
/// `[batch_settlers]` settlement, which is many signed orders in one transaction and is declined
/// one layer up.
pub(crate) fn solver_frames<'a>(frame: &'a CallFrame, registry: &Registry) -> Vec<&'a CallFrame> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([frame]);
    while let Some(frame) = queue.pop_front() {
        if frame.error.is_some() {
            continue;
        }
        if frame
            .to
            .is_some_and(|to| registry.is_solver(to))
        {
            found.push(frame);
            continue;
        }
        queue.extend(&frame.calls);
    }
    found
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
    fn test_native_transfers_delegatecall_and_staticcall() {
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
    fn test_reverted_frame_and_subtree() {
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
    fn test_find_solver_frame_reverted_frames() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");

        let mut reverted = frame("CALL", addr(2), oneinch, 0);
        reverted.error = Some("execution reverted".to_string());
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![reverted];

        assert!(find_solver_frame(&root, &registry).is_none());
    }

    #[test]
    fn test_find_solver_frame_two_branches() {
        // Two known solvers on separate branches: 1inch is a direct child, 0x sits one level down
        // inside the branch that comes first. The direct child settled the trade — the other is a
        // step inside someone else's route — so depth decides, not walk order.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");
        let venue = addr(2);

        let mut first_branch = frame("CALL", venue, addr(3), 0);
        first_branch.calls = vec![frame("CALL", addr(3), zerox, 0)];
        let mut root = frame("CALL", addr(1), venue, 0);
        root.calls = vec![first_branch, frame("CALL", venue, oneinch, 0)];

        assert_eq!(find_solver_frame(&root, &registry).and_then(|frame| frame.to), Some(oneinch));
    }

    #[test]
    fn test_find_solver_frame_nested_solvers() {
        // Stacked rather than side by side: the outer solver called the inner one, so the outer
        // one owns the trade. This shape already resolved correctly and must keep doing so.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut outer = frame("CALL", addr(2), oneinch, 0);
        outer.calls = vec![frame("CALL", oneinch, zerox, 0)];
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![outer];

        assert_eq!(find_solver_frame(&root, &registry).and_then(|frame| frame.to), Some(oneinch));
    }

    #[test]
    fn test_find_solver_frame_router_at_the_root() {
        // A transaction sent straight to a router: the root is the solver frame, whatever the
        // route below it touches. This is the common case and the walk must not descend past it.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut root = frame("CALL", addr(1), oneinch, 0);
        root.calls = vec![frame("CALL", oneinch, zerox, 0)];

        assert_eq!(find_solver_frame(&root, &registry).and_then(|frame| frame.to), Some(oneinch));
    }

    #[test]
    fn test_find_solver_frame_reverted_branch_beside_a_live_one() {
        // A reverted frame prunes its whole subtree, so a live solver deeper in another branch is
        // still found. Breadth-first must not turn the prune into a stop.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let venue = addr(2);

        let mut reverted = frame("CALL", venue, addr(3), 0);
        reverted.error = Some("execution reverted".to_string());
        reverted.calls = vec![frame("CALL", addr(3), oneinch, 0)];
        let mut live = frame("CALL", venue, addr(4), 0);
        live.calls = vec![frame("CALL", addr(4), oneinch, 0)];
        let mut root = frame("CALL", addr(1), venue, 0);
        root.calls = vec![reverted, live];

        let found = find_solver_frame(&root, &registry).expect("the live branch still matches");
        assert_eq!(found.to, Some(oneinch));
        assert_eq!(found.from, addr(4));
    }
}
