//! The declared decode: the trade as the settling solver's own calldata states it.
//!
//! This is the primary decode for every matched transaction, regardless of venue. It reads
//! `token_in`/`token_out`/`amount_in` from the settling solver frame's `SwapIntent` and recovers
//! the settled `amount_out` as the gross amount of `token_out` received by the output recipient —
//! the one field calldata can never carry. The declared amounts are already on the solver-task
//! basis: any venue fee left the input before the solver frame, and the recipient's receipt is
//! the gross output before any output-side fee, so no venue knowledge is needed to decode. Venue
//! fees are still recorded for transparency when the entry point belongs to a known venue.
//!
//! Declines (falling through to the netting decoders) when no solver frame or intent is found,
//! the recipient never received the token, or either guard below fails.

use alloy::{
    primitives::{Address, U256},
    rpc::types::trace::geth::CallFrame,
};

use crate::decoder::{
    netting::TraderFlow,
    registry::Registry,
    solvers::{self, SwapIntent},
    trace,
    transfer_ledger::{NetSwap, TransferLedger},
};

/// Decode a transaction from the settling solver frame's own declaration, returning the flow and
/// the parsed intent (whose declared terms land on the record).
///
/// The output recipient is the one the solver's calldata declares; a solver whose calldata
/// carries none delivers to the caller, so the transaction sender is the fallback anchor.
///
/// Two guards protect against the recipient-receipt query mis-attributing a multi-order
/// transaction's output: the recovered output must clear the intent's on-chain floor (a
/// successful trade cleared it by construction, so a violation means the wrong legs were picked
/// up), and, when the calldata also declares a quote, it must sit within `plausible_quote`'s
/// band of the recovered output.
pub(crate) fn declared_flow(
    root: &CallFrame,
    registry: &Registry,
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<(TraderFlow, SwapIntent)> {
    let solver_frame = trace::find_solver_frame(root, registry)?;
    let solver = registry.solver(solver_frame.to?)?;
    let intent = solver
        .decoder
        .declared_swap(&solver_frame.input, None)?;
    let recipient = intent
        .output_recipient
        .unwrap_or(sender);

    let amount_out = transfer_ledger.received_by_address(recipient, intent.token_out);
    if amount_out.is_zero() || amount_out < intent.min_amount_out {
        return None;
    }
    if let Some(quoted) = intent.declared_quote() {
        if !solvers::plausible_quote(quoted, amount_out) {
            return None;
        }
    }

    let (venue_fee_in, venue_fee_out) = venue_fees(registry, entry_point, transfer_ledger, &intent);
    let flow = TraderFlow {
        tracked: sender,
        swap: NetSwap {
            token_in: intent.token_in,
            amount_in: intent.amount_in,
            token_out: intent.token_out,
            amount_out,
        },
        venue_fee_in,
        venue_fee_out,
    };
    Some((flow, intent))
}

/// The venue fees this transaction paid, when the entry point belongs to a known venue. Recorded
/// for transparency only — the declared amounts are already on the solver-task basis, so neither
/// is adjusted (unlike netting's fee back-out).
fn venue_fees(
    registry: &Registry,
    entry_point: Address,
    transfer_ledger: &TransferLedger,
    intent: &SwapIntent,
) -> (Option<U256>, Option<U256>) {
    let Some(venue) = registry
        .venue_name(entry_point)
        .and_then(|name| registry.venue(name))
    else {
        return (None, None);
    };
    let fees = transfer_ledger.received_by(&venue.fee_collectors);
    let non_zero = |token: &Address| {
        fees.get(token)
            .copied()
            .filter(|fee| !fee.is_zero())
    };
    (non_zero(&intent.token_in), non_zero(&intent.token_out))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::test_utils::{addr, frame, make_transfer_log};

    /// Fly's own router — same address on every chain (`docs.fly.trade`).
    const FLY: Address = address!("0x20f6ee51340adeed01a59b0e65cb3703f3dc860c");
    /// 0x's v4 exchange proxy — a registered solver with no `declared_swap` support.
    const ZEROX: Address = address!("0xdef1c0ded9bec7f1a1670819833240f027b25eff");
    /// Relay's own router — in the live fixture this is both the entry point and the
    /// declared output recipient Fly's calldata carries (Relay receives and forwards).
    const ROUTER: Address = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");

    /// The real Fly calldata used by `solvers::fly`'s fixture tests: USDT in, native out,
    /// `amount_in` 19,694,643, `min_amount_out` 10,217,898,321,149,381, declared quote
    /// 10,321,109,415,302,405.
    fn fly_input() -> Vec<u8> {
        let text = include_str!("solvers/fixtures/fly_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    const TOKEN_IN: Address = address!("0xfde4c96c8593536e31f229ea8f37b2ada2699bb2");
    const AMOUNT_IN: u64 = 19_694_643;
    const MIN_AMOUNT_OUT: u128 = 10_217_898_321_149_381;
    const QUOTED_AMOUNT_OUT: u128 = 10_321_109_415_302_405;

    /// A root frame: `sender -> router -> solver`, the solver frame carrying `input`.
    fn root_with_solver_frame(sender: Address, router: Address, solver: Address) -> CallFrame {
        let mut solver_call = frame("CALL", router, solver, 0);
        solver_call.input = fly_input().into();
        let mut root = frame("CALL", sender, router, 0);
        root.calls = vec![solver_call];
        root
    }

    fn relay_collector(registry: &Registry) -> Address {
        *registry
            .venue("relay")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    #[test]
    fn test_decode_recovers_output_from_recipient_receipt() {
        // The router — the declared recipient — receives native ETH above the floor; the
        // sender pays the input token directly (sender-funded).
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let logs = vec![make_transfer_log(TOKEN_IN, sender, ROUTER, U256::from(AMOUNT_IN))];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, intent) = declared_flow(&root, &registry, &ledger, sender, ROUTER).unwrap();
        assert_eq!(flow.tracked, sender);
        assert_eq!(flow.swap.token_in, TOKEN_IN);
        assert_eq!(flow.swap.token_out, Address::ZERO);
        assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
        assert_eq!(flow.swap.amount_out, U256::from(MIN_AMOUNT_OUT + 1_000));
        assert_eq!(intent.min_amount_out, U256::from(MIN_AMOUNT_OUT));
    }

    #[test]
    fn test_decode_below_floor_declines() {
        // The recipient's receipt sits under the intent's on-chain floor: a successful trade
        // clears its floor by construction, so this means the query mis-attributed.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT - 1))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(declared_flow(&root, &registry, &ledger, sender, ROUTER).is_none());
    }

    #[test]
    fn test_decode_no_recipient_receipt_declines() {
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let ledger = TransferLedger::from_transaction(&[], &[]);

        assert!(declared_flow(&root, &registry, &ledger, sender, ROUTER).is_none());
    }

    #[test]
    fn test_decode_no_solver_frame_declines() {
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = frame("CALL", sender, ROUTER, 0);
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(declared_flow(&root, &registry, &ledger, sender, ROUTER).is_none());
    }

    #[test]
    fn test_decode_solver_without_declared_swap_declines() {
        // 0x's v4 proxy is a registered solver (matches `find_solver_frame`) but has no
        // `declared_swap` implementation: the calldata path has nothing to recover, so it falls
        // through to netting.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, ZEROX);
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(declared_flow(&root, &registry, &ledger, sender, ROUTER).is_none());
    }

    #[test]
    fn test_decode_implausible_quote_declines() {
        // A recovered output more than 2x the declared quote: `plausible_quote`'s band would
        // reject it as a unit mismatch or a mis-attributed receipt, even though it clears the
        // floor comfortably.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let implausible = U256::from(QUOTED_AMOUNT_OUT) * U256::from(3u64);
        let native = vec![(addr(50), ROUTER, implausible)];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(declared_flow(&root, &registry, &ledger, sender, ROUTER).is_none());
    }

    #[test]
    fn test_decode_collector_funded_rebalance() {
        // The fee collector, not the sender, net-sends the input token: a solver-initiated
        // rebalance still decodes from the calldata, with the intent's own amounts.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let collector = relay_collector(&registry);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let logs = vec![make_transfer_log(TOKEN_IN, collector, ROUTER, U256::from(AMOUNT_IN))];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, _) = declared_flow(&root, &registry, &ledger, sender, ROUTER).unwrap();
        assert_eq!(flow.tracked, sender);
        assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
    }

    #[test]
    fn test_decode_records_venue_fee_without_adjusting_amounts() {
        // An input-side fee leg to the real Relay collector: recorded for transparency, but
        // `amount_in` stays the intent's raw figure — it is already post-fee, unlike netting's
        // fee back-out. The collectors come from the entry point's venue section in the address
        // book; no venue code is involved.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let collector = relay_collector(&registry);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let logs = vec![
            make_transfer_log(TOKEN_IN, sender, ROUTER, U256::from(AMOUNT_IN)),
            make_transfer_log(TOKEN_IN, ROUTER, collector, U256::from(40)),
        ];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, _) = declared_flow(&root, &registry, &ledger, sender, ROUTER).unwrap();
        assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
        assert_eq!(flow.venue_fee_in, Some(U256::from(40)));
    }

    #[test]
    fn test_decode_outside_a_venue_records_no_fee() {
        // A direct transaction (the entry point is the solver, not a venue): the root frame is
        // the solver frame, and there is no venue section to read collectors from, so no fee is
        // recorded.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let mut root = frame("CALL", sender, FLY, 0);
        root.input = fly_input().into();
        let logs = vec![make_transfer_log(TOKEN_IN, sender, FLY, U256::from(AMOUNT_IN))];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, _) = declared_flow(&root, &registry, &ledger, sender, FLY).unwrap();
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
    }
}
