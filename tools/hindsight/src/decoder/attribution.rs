//! Attribution: which solver settled a decoded trade, and which venue owns its order flow.
//!
//! Attribution runs after decoding and only labels the record (plus the fee bookkeeping a
//! fee-wallet match implies). Nothing here affects whether a trade decodes.
//!
//! The solver label comes from the first evidence tier that answers, most- to least-trusted
//! (see `AttributionSource`). The venue label is normally the contract the trader entered
//! through (`tx.to`); some venues own the order flow without being that contract and are
//! recognized from registry-driven fingerprints (owner, `appData` tag, fee wallet, integrator
//! tag).

use std::collections::HashSet;

use alloy::{
    primitives::{Address, B256, U256},
    rpc::types::trace::geth::CallFrame,
};
use serde::Serialize;

use crate::decoder::{
    registry::Registry,
    trace,
    transfer_ledger::{SettledSwap, TransferLedger},
};

/// The evidence tier that produced a record's solver label, most- to least-trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttributionSource {
    /// The entry point (`tx.to`) is itself a known solver router: the trade settled there.
    EntryPoint,
    /// A known solver router was called inside the trace (venue-wrapped entries).
    TraceMatch,
    /// No known router anywhere: best guess is the external call that moved the most native
    /// value (an unknown router's address).
    LargestCall,
    /// Even the guess was indeterminate (e.g. a token→token trace where no call moves value).
    /// The record is labeled with its entry point — typically the venue's name — flagging it
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
/// Every tier reads the trace or the address book, ending at the entry-point label as the honest
/// "don't know". A venue's own claim about which solver it routed to is not consulted: the router
/// that settled the trade is in the trace, which is the harder fact.
pub(crate) fn solver(
    root: &CallFrame,
    entry_point: Address,
    sender: Address,
    registry: &Registry,
) -> Attribution {
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

/// The order-flow venue for a decoded flow, when a fingerprint matches — overriding the
/// entry-point label. Every fingerprint is registry-driven; nothing here knows about a specific
/// venue or provider.
///
/// Four fingerprints, tried in order: owning trader (`[venue_owners]`), `CoW` `appData` tag
/// (`[venue_appdata]`; the hash is extracted by the caller), fee wallet (`[venue_fees]`),
/// provider integrator tag (`[venue_integrators]`; extracted by the caller).
///
/// A fee-wallet match also corrects the amounts onto the swap's own basis, on whichever side the
/// wallet was paid. Fynd quotes the swap alone, so both corrections are what keep the comparison
/// like-for-like:
///
/// - Output side: the wallet is paid out of the routing path, so the trader's receipt — netted or
///   declared — is short of the gross output by exactly the fee. The fee is added back. Without
///   this the comparison hands Fynd the venue's cut as savings.
/// - Input side: the wallet is paid out of the amount the trader authorized, so the swap saw less
///   than `amount_in` states. The fee is subtracted. Without this Fynd is re-solved on more input
///   than reached the pools, which overstates its output.
pub(crate) fn venue(
    registry: &Registry,
    flow: &mut SettledSwap,
    ledger: &TransferLedger,
    integrator: Option<&str>,
    app_data: Option<B256>,
) -> Option<String> {
    if let Some(venue) = registry.venue_for_owner(flow.tracked) {
        return Some(venue.to_string());
    }
    if let Some(venue) = app_data.and_then(|hash| registry.venue_for_appdata(hash)) {
        return Some(venue.to_string());
    }
    if let Some((venue, fee)) = fee_venue(registry, ledger, flow.token_in, flow.token_out) {
        match fee {
            VenueFee::Input(amount) => flow.amount_in = flow.amount_in.saturating_sub(amount),
            VenueFee::Output(amount) => flow.amount_out = flow.amount_out.saturating_add(amount),
        }
        return Some(venue);
    }
    integrator
        .and_then(|tag| registry.venue_for_integrator(tag))
        .map(str::to_string)
}

/// Which side of the swap a venue took its fee from, with the amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VenueFee {
    /// Skimmed off the input before the swap, so the settled input is smaller than the user spent.
    Input(U256),
    /// Taken out of the output after the swap, so the settled output is larger than the user kept.
    Output(U256),
}

/// The venue whose fee wallet took a cut of this trade, and which side it came from. `None` when no
/// venue fee wallet received a non-zero amount of either swap token.
///
/// Both sides are checked because venues split on this: Phantom and Robinhood take the buy token,
/// while Coinbase's Base App skims the sell token before routing. The output side is tried first —
/// a wallet that received both tokens is being paid its cut in the token the user bought. The
/// wallets are checked in address order, so two venues' wallets both taking a cut of one trade
/// resolve to the same venue on every run.
fn fee_venue(
    registry: &Registry,
    ledger: &TransferLedger,
    token_in: Address,
    token_out: Address,
) -> Option<(String, VenueFee)> {
    for (wallet, venue) in registry.venue_fees() {
        let received = ledger.received_by(&HashSet::from([*wallet]));
        let non_zero = |token: &Address| {
            received
                .get(token)
                .copied()
                .filter(|amount| !amount.is_zero())
        };
        if let Some(fee) = non_zero(&token_out) {
            return Some((venue.clone(), VenueFee::Output(fee)));
        }
        if let Some(fee) = non_zero(&token_in) {
            return Some((venue.clone(), VenueFee::Input(fee)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256};
    use tycho_simulation::tycho_common::models::Chain;

    use super::*;
    use crate::decoder::test_utils::{addr, frame, make_transfer_log, swap, PERMIT2};

    #[test]
    fn test_venue_wrapped_entry_reads_the_trace() {
        // A venue-wrapped entry: the router that settled the trade is inside the trace, and that
        // is what the record is labelled with — no venue's own claim is consulted.
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let mut root = frame("CALL", addr(1), addr(2), 0);
        root.calls = vec![frame("CALL", addr(2), oneinch, 1000)];

        let attribution = solver(&root, addr(2), addr(1), &registry);
        assert_eq!(attribution.solver, "1inch");
        assert_eq!(attribution.source, AttributionSource::TraceMatch);
    }

    #[test]
    fn test_direct_swap_entry_point() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let root = frame("CALL", addr(1), oneinch, 0);

        let attribution = solver(&root, oneinch, addr(1), &registry);
        assert_eq!(attribution.solver, "1inch");
        assert_eq!(attribution.source, AttributionSource::EntryPoint);
    }

    #[test]
    fn test_relay_internal_solver() {
        // Mirrors the real Relay tx: the client router calls 0x's AllowanceHolder.
        // root(relay) -> [ relay (self-call), 0x AllowanceHolder (the solver) ]
        let registry = Registry::ethereum();
        let sender = addr(1);
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let zerox = address!("0x0000000000001ff3684f28c67538d4d072c22734");

        let mut root = frame("CALL", sender, relay, 0);
        root.calls = vec![frame("CALL", relay, relay, 0), frame("CALL", relay, zerox, 1000)];

        let attribution = solver(&root, relay, sender, &registry);
        assert_eq!(attribution.solver, "0x");
        assert_eq!(attribution.source, AttributionSource::TraceMatch);
    }

    #[test]
    fn test_relay_tycho_router() {
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

        let attribution = solver(&root, relay_proxy, sender, &registry);
        assert_eq!(attribution.solver, "tycho");
        assert_eq!(attribution.source, AttributionSource::TraceMatch);
    }

    #[test]
    fn test_unknown_solver_largest_external_call() {
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

        let attribution = solver(&root, client, sender, &registry);
        assert_eq!(attribution.solver, unknown_router.to_string());
        assert_eq!(attribution.source, AttributionSource::LargestCall);
    }

    #[test]
    fn test_attribution_zero_value_fallback() {
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

        let attribution = solver(&root, client, sender, &registry);
        assert_eq!(attribution.solver, client.to_string());
        assert_eq!(attribution.source, AttributionSource::Fallback);
    }

    #[test]
    fn test_attribution_wrapped_native_frames() {
        // ETH-input swap through an unknown router: the highest-value direct call is the
        // WETH.deposit() wrapping the input. Infrastructure, not a solver — the guess must
        // fall through to the real router call.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);
        let unknown = addr(50);

        let mut root = frame("CALL", sender, client, 0);
        root.calls = vec![
            frame("CALL", client, registry.wrapped_native(), 9000), // wrap, skipped
            frame("CALL", client, unknown, 100),
        ];

        let attribution = solver(&root, client, sender, &registry);
        assert_eq!(attribution.solver, unknown.to_string());
        assert_eq!(attribution.source, AttributionSource::LargestCall);
    }

    #[test]
    fn test_attribution_permit2_frames() {
        // Even when Permit2 is the highest-value direct call, it is infrastructure, not a solver.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let client = addr(2);
        let unknown = addr(50);

        let mut root = frame("CALL", sender, client, 0);
        root.calls =
            vec![frame("CALL", client, PERMIT2, 9000), frame("CALL", client, unknown, 100)];

        let attribution = solver(&root, client, sender, &registry);
        assert_eq!(attribution.solver, unknown.to_string());
        assert_eq!(attribution.source, AttributionSource::LargestCall);
    }

    #[test]
    fn test_attributes_owner_to_venue() {
        // A CoW-settled kpk trade nets to the Safe that owns the order; the venue is that Safe.
        let registry = Registry::ethereum();
        let kpk_safe = address!("0x4f2083f5fbede34c2714affb3105539775f7fe64");
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = SettledSwap { tracked: kpk_safe, ..swap(addr(10), 1, addr(11), 2) };
        assert_eq!(venue(&registry, &mut flow, &ledger, None, None).as_deref(), Some("kpk"));
    }

    #[test]
    fn test_unknown_owner_is_not_a_venue() {
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = SettledSwap { tracked: addr(9), ..swap(addr(10), 1, addr(11), 2) };
        assert_eq!(venue(&registry, &mut flow, &ledger, None, None), None);
    }

    #[test]
    fn test_appdata_tag_attributes_venue() {
        // A CoW order carrying DefiLlama's appData hash is attributed to LlamaSwap; an unregistered
        // hash is not.
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let defillama = b256!("0xf249b3db926aa5b5a1b18f3fec86b9cc99b9a8a99ad7e8034242d2838ae97422");
        let mut flow = SettledSwap { tracked: addr(1), ..swap(addr(10), 1, addr(11), 2) };
        assert_eq!(
            venue(&registry, &mut flow, &ledger, None, Some(defillama)).as_deref(),
            Some("llamaswap")
        );
        assert_eq!(venue(&registry, &mut flow, &ledger, None, Some(B256::ZERO)), None);
    }

    #[test]
    fn test_fee_wallet_attributes_and_grosses_fee_back() {
        // A 0x-routed Phantom swap: the buy-token fee reaches Phantom's wallet. It must be added
        // back so the settled output is gross (else every Phantom swap under-reports by 85 bps).
        let registry = Registry::ethereum();
        let phantom = address!("0x2cffed5d56eb6a17662756ca0fdf350e732c9818");
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(9915)),
            make_transfer_log(token_out, pool, phantom, U256::from(85)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = SettledSwap { tracked: user, ..swap(token_in, 1000, token_out, 9915) };

        assert_eq!(venue(&registry, &mut flow, &ledger, None, None).as_deref(), Some("phantom"));
        assert_eq!(flow.amount_out, U256::from(10000));
    }

    #[test]
    fn test_fee_wallet_on_declared_amounts() {
        // The same Phantom fee leg on a declared decode: the wallet is paid from the routing
        // path directly, so the declared recipient's receipt (9915) is short of the gross output
        // by the fee — grossed back, exactly like a netted flow.
        let registry = Registry::ethereum();
        let phantom = address!("0x2cffed5d56eb6a17662756ca0fdf350e732c9818");
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(9915)),
            make_transfer_log(token_out, pool, phantom, U256::from(85)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = SettledSwap { tracked: user, ..swap(token_in, 1000, token_out, 9915) };

        assert_eq!(venue(&registry, &mut flow, &ledger, None, None).as_deref(), Some("phantom"));
        assert_eq!(flow.amount_out, U256::from(10000));
    }

    #[test]
    fn test_integrator_tag_attributes_venue() {
        // A provider integrator tag maps to its venue, case-insensitively; an unknown tag does
        // not.
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = SettledSwap { tracked: addr(1), ..swap(addr(10), 1, addr(11), 2) };
        assert_eq!(
            venue(&registry, &mut flow, &ledger, Some("Infinex"), None).as_deref(),
            Some("infinex")
        );
        assert_eq!(venue(&registry, &mut flow, &ledger, Some("somedapp"), None), None);
    }

    #[test]
    fn test_fee_wallet_input_side_fee_nets_the_input_down() {
        // A LiFi-routed Coinbase Base App swap: the 0.95% cut is skimmed off the sell token, so
        // the pools saw 9905 of the 10000 the trader authorized. The fee is subtracted, else Fynd
        // is re-solved on 10000 and its larger output reads as savings.
        let registry = Registry::builtin(Chain::Bsc).unwrap();
        let coinbase = address!("0x5aafc1f252d544f744d17a4e734afd6efc47ede4");
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, coinbase, U256::from(95)),
            make_transfer_log(token_in, user, pool, U256::from(9905)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = SettledSwap { tracked: user, ..swap(token_in, 10000, token_out, 2000) };

        assert_eq!(
            venue(&registry, &mut flow, &ledger, Some("base-app"), None).as_deref(),
            Some("coinbase")
        );
        assert_eq!(flow.amount_in, U256::from(9905));
        // The output side is untouched: this venue took nothing out of the buy token.
        assert_eq!(flow.amount_out, U256::from(2000));
    }

    #[test]
    fn test_fee_wallet_taking_both_tokens_is_read_as_an_output_fee() {
        // A wallet that received both swap tokens is being paid its cut in the token the user
        // bought; the sell-token leg is the swap's own routing, not a second fee.
        let registry = Registry::ethereum();
        let phantom = address!("0x2cffed5d56eb6a17662756ca0fdf350e732c9818");
        let user = addr(1);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, phantom, U256::from(7)),
            make_transfer_log(token_out, addr(50), phantom, U256::from(85)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = SettledSwap { tracked: user, ..swap(token_in, 1000, token_out, 9915) };

        assert_eq!(venue(&registry, &mut flow, &ledger, None, None).as_deref(), Some("phantom"));
        assert_eq!(flow.amount_out, U256::from(10000));
        assert_eq!(flow.amount_in, U256::from(1000));
    }

    #[test]
    fn test_no_fee_transfer_is_not_a_venue() {
        // Dust to the fee wallet in a token other than the output is not this trade's fee.
        let registry = Registry::ethereum();
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = SettledSwap { tracked: user, ..swap(token_in, 1000, token_out, 2000) };
        assert_eq!(venue(&registry, &mut flow, &ledger, None, None), None);
    }
}
