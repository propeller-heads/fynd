use alloy::{
    primitives::{Address, TxHash, U256},
    providers::{ext::DebugApi, Provider},
    rpc::types::trace::geth::{CallConfig, CallFrame, GethDebugTracingOptions, GethTrace},
};
use anyhow::Context;

use crate::decoder::{registry::Registry, solvers};

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
/// The venue's own gas — fee transfers, forwarding, the base transaction cost — is charged
/// whichever router the venue picks, so like the venue fee it is excluded from the comparison. Each
/// trace frame's `gas_used` includes its whole subtree, so the call into the solver carries the
/// full routing cost. Prefers the first call into a known solver; falls back to the most
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

/// Depth-first search for the call frame that settled or tried the swap with a known solver.
///
/// Prefers the frame that actually settled — skipping reverted frames and their subtrees, which
/// settle nothing — and, only when that search finds nothing, falls back to a tolerant search
/// that descends into reverted frames too. Plain "ignore every revert" (the simpler-looking
/// alternative) would get a settled trade wrong: a router that tried solver A (which reverted)
/// before succeeding via solver B has both frames in its trace, and a revert-blind walk can
/// attribute to whichever it reaches first in depth-first order, not whichever actually settled.
/// The two-phase preference handles both shapes with one function: a settled trade always finds
/// its real solver frame in the first (strict) pass, and a reverted trade — which has no settled
/// frame by definition — falls through to the second (tolerant) pass, since a revert emits no
/// logs and the frame that tried is the only frame there is to attribute to or recover calldata
/// from (see the trace fixtures for both shapes observed live).
///
/// The one walk serves every question asked of a trace: *who* settled or tried the swap (the
/// frame's `to`, for attribution and calldata extraction) and *what the route cost* (the frame's
/// `gas_used`, for gas accounting) — so the gas charged is always the gas of the exact frame the
/// solver label came from.
pub(crate) fn find_solver_frame<'a>(
    frame: &'a CallFrame,
    registry: &Registry,
) -> Option<&'a CallFrame> {
    find_solver_frame_impl(frame, registry, false)
        .or_else(|| find_solver_frame_impl(frame, registry, true))
}

/// Shared walk for both passes of [`find_solver_frame`]: the only difference is whether a frame's
/// own revert stops the search or not.
fn find_solver_frame_impl<'a>(
    frame: &'a CallFrame,
    registry: &Registry,
    tolerate_reverts: bool,
) -> Option<&'a CallFrame> {
    if frame.error.is_some() && !tolerate_reverts {
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
        .find_map(|child| find_solver_frame_impl(child, registry, tolerate_reverts))
}

/// The geth call tracer's exact error string for a frame that ran out of gas.
const OUT_OF_GAS: &str = "out of gas";

/// Why a reverted transaction failed, classified from its trace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub(crate) enum RevertCause {
    /// A slippage-floor revert: Fly's `InsufficientAmountOut()` or `KyberSwap`'s "Return amount is
    /// not enough" — the class of revert a fresher quote could have avoided.
    SlippageFloor,
    /// A frame ran out of gas.
    OutOfGas,
    /// Any other cause, taken from the deepest reverted frame's error or revert reason.
    Other(String),
}

/// Classify why a reverted transaction's root frame failed, from its whole reverted subtree.
///
/// Checked in order of specificity: a slippage-floor marker (Fly's custom-error selector in any
/// frame's output, or `KyberSwap`'s revert reason string) anywhere in the subtree wins even when a
/// deeper frame failed for an unrelated reason — real reverts often cascade through several
/// frames, and the slippage-floor check is the one classification callers act on (it is the
/// avoidable class). Otherwise, any frame with the exact "out of gas" error settles it. Anything
/// else falls back to the deepest reverted frame's own error or revert reason.
pub(crate) fn classify_revert_cause(root: &CallFrame) -> RevertCause {
    if has_slippage_floor_marker(root) {
        return RevertCause::SlippageFloor;
    }
    if has_out_of_gas(root) {
        return RevertCause::OutOfGas;
    }
    RevertCause::Other(deepest_revert_reason(root).unwrap_or_else(|| "unknown revert".to_string()))
}

fn has_slippage_floor_marker(frame: &CallFrame) -> bool {
    let output = frame.output.as_ref().map(AsRef::as_ref);
    solvers::is_slippage_floor(output, frame.revert_reason.as_deref()) ||
        frame
            .calls
            .iter()
            .any(has_slippage_floor_marker)
}

fn has_out_of_gas(frame: &CallFrame) -> bool {
    frame.error.as_deref() == Some(OUT_OF_GAS) || frame.calls.iter().any(has_out_of_gas)
}

/// The error or revert reason of the deepest reverted frame, by depth-first search — the last
/// (deepest) hit wins, since a subtree's own failure is usually more specific than an ancestor's
/// bubbled-up "execution reverted".
fn deepest_revert_reason(frame: &CallFrame) -> Option<String> {
    let mut deepest: Option<(usize, String)> = None;
    collect_deepest_revert_reason(frame, 0, &mut deepest);
    deepest.map(|(_, reason)| reason)
}

fn collect_deepest_revert_reason(
    frame: &CallFrame,
    depth: usize,
    deepest: &mut Option<(usize, String)>,
) {
    if let Some(reason) = frame_revert_reason(frame) {
        if deepest
            .as_ref()
            .is_none_or(|(best_depth, _)| depth >= *best_depth)
        {
            *deepest = Some((depth, reason));
        }
    }
    for child in &frame.calls {
        collect_deepest_revert_reason(child, depth + 1, deepest);
    }
}

/// One frame's revert reason: the tracer's own ABI-decoded string when it provides one, otherwise
/// the generic error message with the frame's raw output selector appended when there is one to
/// show (`"execution reverted (0x12345678)"`). Several RPC providers never populate
/// `revert_reason` at all, even for a frame whose output does encode a selector (a custom error,
/// or an `Error(string)` the tracer did not bother decoding) — the selector is what is left to
/// classify offline, so it is surfaced rather than dropped.
fn frame_revert_reason(frame: &CallFrame) -> Option<String> {
    if let Some(reason) = frame.revert_reason.clone() {
        return Some(reason);
    }
    let error = frame.error.clone()?;
    let selector = frame
        .output
        .as_ref()
        .filter(|output| output.len() >= 4)
        .map(|output| format!("0x{}", alloy::hex::encode(&output[..4])));
    Some(match selector {
        Some(selector) => format!("{error} ({selector})"),
        None => error,
    })
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
    use tycho_simulation::tycho_common::models::Chain;

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

    fn with_gas(mut call: CallFrame, gas_used: u64) -> CallFrame {
        call.gas_used = U256::from(gas_used);
        call
    }

    #[test]
    fn test_route_gas_known_venue() {
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
    fn test_route_gas_unknown_venue() {
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
    fn test_route_gas_reverted_and_empty() {
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
    fn test_route_gas_real_relay_kyberswap_trace() {
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
    fn test_route_gas_real_metamask_oneinch_trace() {
        // Real callTracer output of tx 0xe815e2b5… (block 25476433, a $3.4k MetaMask swap
        // routed via 1inch), payload fields stripped. The 1inch frame sits three levels deep:
        //
        //   metamask router        185,699
        //   └── spender            180,406   <- largest child: wrapper, NOT the route
        //       └── adapter        175,635   (delegatecall)
        //           ├── 1inch v6   115,795   <- the route
        //           └── fee wallet   6,329   (MetaMask's fee, correctly excluded)
        //
        // so the known-venue search must win over the largest-child fallback.
        let root: CallFrame =
            serde_json::from_str(include_str!("fixtures/trace_metamask_1inch_0xe815e2b5.json"))
                .unwrap();
        assert_eq!(root.gas_used, U256::from(185_699u64));
        assert_eq!(route_gas(&root, &Registry::ethereum()), Some(U256::from(115_795u64)));
    }

    #[test]
    fn test_find_solver_frame_prefers_a_settled_frame_over_a_reverted_attempt() {
        // The exact shape the doc comment calls out: a router tried solver A (which reverted)
        // before succeeding via solver B. The strict pass must attribute to B, not whichever it
        // reaches first in depth-first order.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut attempt_a = frame("CALL", addr(2), oneinch, 0);
        attempt_a.error = Some("execution reverted".to_string());
        let attempt_b = frame("CALL", addr(2), zerox, 0);
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![attempt_a, attempt_b];

        let found = find_solver_frame(&root, &registry).unwrap();
        assert_eq!(found.to, Some(zerox));
    }

    #[test]
    fn test_find_solver_frame_falls_back_when_nothing_settled() {
        // A reverted trade has no settled frame by definition: the strict pass finds nothing, so
        // the tolerant fallback is the only way to recover which router the trader was routed
        // through — the frame that tried is the only frame there is to find.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");

        let mut solver_call = frame("CALL", addr(2), oneinch, 0);
        solver_call.error = Some("execution reverted".to_string());
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.error = Some("execution reverted".to_string());
        root.calls = vec![solver_call];

        let found = find_solver_frame(&root, &registry).unwrap();
        assert_eq!(found.to, Some(oneinch));
    }

    #[test]
    fn test_find_solver_frame_real_fly_slippage_trace() {
        // Real reverted trace (Base, Relay -> Fly): the fly frame itself reverted
        // (InsufficientAmountOut), nested inside the reverted router and approval-proxy frames.
        // Nothing settled, so the strict pass finds nothing and the fallback recovers it.
        let root: CallFrame = serde_json::from_str(include_str!(
            "fixtures/trace_revert_fly_slippage_0xcba01d0c.json"
        ))
        .unwrap();
        let fly = address!("0x20f6ee51340adeed01a59b0e65cb3703f3dc860c");
        let found = find_solver_frame(&root, &Registry::builtin(Chain::Base).unwrap()).unwrap();
        assert_eq!(found.to, Some(fly));
    }

    #[test]
    fn test_find_solver_frame_real_out_of_gas_trace() {
        // Real reverted trace (Base): the fly frame itself succeeded (err = None) — the router
        // ran out of gas on a later, unrelated call — so the strict pass already finds it, even
        // though it sits under two reverted ancestors.
        let root: CallFrame =
            serde_json::from_str(include_str!("fixtures/trace_revert_out_of_gas_0x08fee57c.json"))
                .unwrap();
        let fly = address!("0x20f6ee51340adeed01a59b0e65cb3703f3dc860c");
        let found = find_solver_frame(&root, &Registry::builtin(Chain::Base).unwrap()).unwrap();
        assert_eq!(found.to, Some(fly));
        assert!(found.error.is_none());
    }

    #[test]
    fn test_classify_revert_cause_real_fly_slippage() {
        let root: CallFrame = serde_json::from_str(include_str!(
            "fixtures/trace_revert_fly_slippage_0xcba01d0c.json"
        ))
        .unwrap();
        assert_eq!(classify_revert_cause(&root), RevertCause::SlippageFloor);
    }

    #[test]
    fn test_classify_revert_cause_real_kyber_slippage() {
        let root: CallFrame = serde_json::from_str(include_str!(
            "fixtures/trace_revert_kyber_slippage_0xd3b7ffae.json"
        ))
        .unwrap();
        assert_eq!(classify_revert_cause(&root), RevertCause::SlippageFloor);
    }

    #[test]
    fn test_classify_revert_cause_real_out_of_gas() {
        let root: CallFrame =
            serde_json::from_str(include_str!("fixtures/trace_revert_out_of_gas_0x08fee57c.json"))
                .unwrap();
        assert_eq!(classify_revert_cause(&root), RevertCause::OutOfGas);
    }

    #[test]
    fn test_classify_revert_cause_real_other() {
        // A `transferFrom` failure deep inside Fly's frame (a custom ERC-20 error, not one of the
        // known slippage markers): neither Fly's nor KyberSwap's marker matches, so it falls back
        // to the deepest reverted frame's own error — this RPC never populates `revertReason`, so
        // the frame's raw output selector (0xe450d38c, `ERC20InsufficientBalance(address,uint256,
        // uint256)`) is appended to the generic message rather than left unclassifiable.
        let root: CallFrame = serde_json::from_str(include_str!(
            "fixtures/trace_revert_transfer_failure_0x12d802d5.json"
        ))
        .unwrap();
        assert_eq!(
            classify_revert_cause(&root),
            RevertCause::Other("execution reverted (0xe450d38c)".to_string())
        );
    }

    #[test]
    fn test_classify_revert_cause_synthetic_slippage_and_gas() {
        let mut fly_floor = frame("CALL", addr(1), addr(2), 0);
        fly_floor.error = Some("execution reverted".to_string());
        fly_floor.output = Some(alloy::primitives::Bytes::from_static(&[0xe5, 0x29, 0x70, 0xaa]));
        assert_eq!(classify_revert_cause(&fly_floor), RevertCause::SlippageFloor);

        let mut kyber_floor = frame("CALL", addr(1), addr(2), 0);
        kyber_floor.error = Some("execution reverted".to_string());
        kyber_floor.revert_reason = Some("Return amount is not enough".to_string());
        assert_eq!(classify_revert_cause(&kyber_floor), RevertCause::SlippageFloor);

        let mut out_of_gas = frame("CALL", addr(1), addr(2), 0);
        out_of_gas.error = Some("out of gas".to_string());
        assert_eq!(classify_revert_cause(&out_of_gas), RevertCause::OutOfGas);
    }

    #[test]
    fn test_classify_revert_cause_prefers_deepest_reason() {
        let mut inner = frame("CALL", addr(2), addr(3), 0);
        inner.error = Some("transferFrom failed".to_string());
        let mut outer = frame("CALL", addr(1), addr(2), 0);
        outer.error = Some("execution reverted".to_string());
        outer.calls = vec![inner];

        assert_eq!(
            classify_revert_cause(&outer),
            RevertCause::Other("transferFrom failed".to_string())
        );
    }

    #[test]
    fn test_classify_revert_cause_real_zeroex_slippage() {
        // Real reverted trace (Base): 0x's own TooMuchSlippage(address,uint256,uint256) bubbles
        // through the Settler and AllowanceHolder frames, both registered as solver "0x".
        let root: CallFrame = serde_json::from_str(include_str!(
            "fixtures/trace_revert_zeroex_slippage_0x157e025b.json"
        ))
        .unwrap();
        assert_eq!(classify_revert_cause(&root), RevertCause::SlippageFloor);
    }

    #[test]
    fn test_frame_revert_reason_appends_the_selector_when_undecoded() {
        // No `revert_reason` (this RPC never populates it) but the frame's raw output carries a
        // selector: the generic message gets it appended, so an unrecognized custom error stays
        // classifiable offline instead of collapsing into a bare "execution reverted".
        let mut custom_error = frame("CALL", addr(1), addr(2), 0);
        custom_error.error = Some("execution reverted".to_string());
        custom_error.output =
            Some(alloy::primitives::Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]));
        assert_eq!(
            frame_revert_reason(&custom_error),
            Some("execution reverted (0x12345678)".to_string())
        );
    }

    #[test]
    fn test_frame_revert_reason_prefers_the_decoded_reason() {
        let mut decoded = frame("CALL", addr(1), addr(2), 0);
        decoded.error = Some("execution reverted".to_string());
        decoded.revert_reason = Some("Return amount is not enough".to_string());
        decoded.output = Some(alloy::primitives::Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]));
        assert_eq!(frame_revert_reason(&decoded), Some("Return amount is not enough".to_string()));
    }

    #[test]
    fn test_frame_revert_reason_no_output_stays_bare() {
        let mut bare = frame("CALL", addr(1), addr(2), 0);
        bare.error = Some("execution reverted".to_string());
        assert_eq!(frame_revert_reason(&bare), Some("execution reverted".to_string()));
    }
}
