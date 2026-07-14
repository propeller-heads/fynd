//! Which solver settled a matched transaction.
//!
//! One decision, taken here in full: the solver label on a record comes from the first evidence
//! tier that answers, most- to least-trusted (see [`AttributionSource`]). The tier is recorded
//! alongside the label so downstream analysis can weigh it — an embedded quote attached to a
//! `declared` attribution is solid; one attached to a `largest_call` guess is not.

use alloy::{primitives::Address, rpc::types::trace::geth::CallFrame};
use serde::Serialize;

use crate::decoder::{registry::Registry, trace};

/// The evidence tier that produced a record's solver label, most- to least-trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttributionSource {
    /// The decode strategy read the solver from calldata (`MetaMask`'s `aggregatorId`).
    Declared,
    /// The entry point (`tx.to`) is itself a known solver router: the trade settled there.
    EntryPoint,
    /// A known solver router was called inside the trace (client-wrapped entries).
    TraceMatch,
    /// No known router anywhere: best guess is the external call that moved the most native
    /// value (an unknown router's address).
    LargestCall,
    /// Even the guess was indeterminate (e.g. a token→token trace where no call moves value).
    /// The record is labeled with its entry point — typically the client's name — flagging it
    /// for registry expansion.
    Fallback,
}

/// A solver label and the evidence tier it came from.
pub(crate) struct Attribution {
    pub solver: String,
    pub source: AttributionSource,
}

/// Attribute the solver that settled a matched transaction.
///
/// `declared` is the strategy's own claim (from calldata), which outranks everything; the
/// remaining tiers read the trace, ending at the entry-point label as the honest "don't know".
pub(crate) fn attribute(
    declared: Option<String>,
    root: &CallFrame,
    entry_point: Address,
    sender: Address,
    registry: &Registry,
) -> Attribution {
    if let Some(solver) = declared {
        return Attribution { solver, source: AttributionSource::Declared };
    }
    if registry.is_solver(entry_point) {
        return Attribution {
            solver: registry.label(entry_point),
            source: AttributionSource::EntryPoint,
        };
    }
    if let Some(found) = trace::find_solver_frame(root, registry).and_then(|frame| frame.to) {
        return Attribution { solver: registry.label(found), source: AttributionSource::TraceMatch };
    }
    if let Some(guess) = trace::largest_external_call(root, entry_point, sender, registry) {
        return Attribution {
            solver: registry.label(guess),
            source: AttributionSource::LargestCall,
        };
    }
    Attribution { solver: registry.label(entry_point), source: AttributionSource::Fallback }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::{
        test_utils::{addr, frame},
        trace::PERMIT2,
    };

    #[test]
    fn declared_solver_outranks_the_trace() {
        // MetaMask declares its solver in calldata; even a known router in the trace must not
        // override it.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![frame("CALL", addr(2), oneinch, 1000)];

        let attribution =
            attribute(Some("uniswap".to_string()), &root, addr(2), addr(1), &registry);
        assert_eq!(attribution.solver, "uniswap");
        assert_eq!(attribution.source, AttributionSource::Declared);
    }

    #[test]
    fn direct_swap_attributes_entry_point() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let root = frame("CALL", addr(1), oneinch, 0);

        let attribution = attribute(None, &root, oneinch, addr(1), &registry);
        assert_eq!(attribution.solver, "1inch");
        assert_eq!(attribution.source, AttributionSource::EntryPoint);
    }

    #[test]
    fn relay_attributes_internal_solver() {
        // Mirrors the real Relay tx: the client router calls 0x's AllowanceHolder.
        // root(relay) -> [ relay (self-call), 0x AllowanceHolder (the solver) ]
        let registry = Registry::ethereum();
        let sender = addr(1);
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut root = frame("CALL", sender, relay, 0);
        root.calls = vec![frame("CALL", relay, relay, 0), frame("CALL", relay, zerox, 1000)];

        let attribution = attribute(None, &root, relay, sender, &registry);
        assert_eq!(attribution.solver, "0x");
        assert_eq!(attribution.source, AttributionSource::TraceMatch);
    }

    #[test]
    fn relay_attributes_tycho_router() {
        // Real tx 0x8b461c…: Relay ApprovalProxy -> Relay router -> Tycho router.
        // The settling solver is Tycho even though it sits two levels deep.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let relay_proxy = address!("0xccc88a9d1b4ed6b0eaba998850414b24f1c315be");
        let relay_router = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");
        let tycho = address!("0x1f8db310f32d48b6180ff902ec60c586128cef47");

        let mut router_call = frame("CALL", relay_proxy, relay_router, 0);
        router_call.calls = vec![frame("CALL", relay_router, tycho, 0)];
        let mut root = frame("CALL", sender, relay_proxy, 0);
        root.calls = vec![router_call];

        let attribution = attribute(None, &root, relay_proxy, sender, &registry);
        assert_eq!(attribution.solver, "tycho");
        assert_eq!(attribution.source, AttributionSource::TraceMatch);
    }

    #[test]
    fn unknown_solver_falls_back_to_largest_external_call() {
        // No known solver in the trace: pick the largest external call,
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

        let attribution = attribute(None, &root, client, sender, &registry);
        assert_eq!(attribution.solver, unknown_router.to_string());
        assert_eq!(attribution.source, AttributionSource::LargestCall);
    }

    #[test]
    fn attribution_declines_zero_value_fallback() {
        // Unknown solver, token->token swap: every child call moves zero value, so the guess
        // would degenerate to the first child (the Permit2 token pull). The record is labeled
        // with its entry point instead, marked as a fallback.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![
            frame("CALL", client, PERMIT2, 0),  // token pull
            frame("CALL", client, addr(50), 0), // unknown solver, zero value
        ];

        let attribution = attribute(None, &root, client, sender, &registry);
        assert_eq!(attribution.solver, client.to_string());
        assert_eq!(attribution.source, AttributionSource::Fallback);
    }

    #[test]
    fn attribution_never_picks_wrapped_native() {
        // ETH-input swap through an unknown router: the highest-value direct call is the
        // WETH.deposit() wrapping the input. Infrastructure, not a solver — the guess must
        // fall through to the real router call.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);
        let solver = addr(50);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![
            frame("CALL", client, registry.wrapped_native(), 9000), // wrap, skipped
            frame("CALL", client, solver, 100),
        ];

        let attribution = attribute(None, &root, client, sender, &registry);
        assert_eq!(attribution.solver, solver.to_string());
        assert_eq!(attribution.source, AttributionSource::LargestCall);
    }

    #[test]
    fn attribution_never_picks_permit2() {
        // Even when Permit2 is the highest-value direct call, it is infrastructure, not a solver.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);
        let solver = addr(50);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![frame("CALL", client, PERMIT2, 9000), frame("CALL", client, solver, 100)];

        let attribution = attribute(None, &root, client, sender, &registry);
        assert_eq!(attribution.solver, solver.to_string());
        assert_eq!(attribution.source, AttributionSource::LargestCall);
    }
}
