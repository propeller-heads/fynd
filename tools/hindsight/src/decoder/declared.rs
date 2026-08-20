//! The declared decode: the trade as the settling solver's own data states it.
//!
//! This is the primary decode for every matched transaction, regardless of venue. A solver either
//! states its trade in its own logs — amounts included, nothing left to recover — or carries the
//! terms in its calldata, in which case the settled `amount_out` is recovered as the gross amount
//! the declared output recipient received. That one field is the only thing calldata never carries.
//!
//! `amount_in` needs no fee adjustment either way: an input-side venue fee left before the solver
//! frame, so the frame's own figure is already the amount that entered the swap. No venue
//! knowledge is needed to decode.

use alloy::{
    primitives::Address,
    rpc::types::{trace::geth::CallFrame, Log},
};

use crate::decoder::{
    netting::TraderFlow,
    registry::Registry,
    solvers::{self, Declaration, SwapIntent},
    trace,
    transfer_ledger::{NetSwap, TransferLedger},
};

/// Decode a transaction from the settling solver's own declaration: the decoder label recorded on
/// the trade, the flow, and the parsed terms when the read was a calldata one (their columns land
/// on the record).
///
/// A log-stated trade is tried first, across every registered solver that emitted a log here: it
/// is complete, so nothing has to be recovered from the ledger. Otherwise the settling solver's
/// frame is found and its calldata read.
pub(crate) fn declared_flow(
    root: &CallFrame,
    registry: &Registry,
    logs: &[Log],
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<(&'static str, TraderFlow, Option<SwapIntent>)> {
    if let Some(flow) = settled_from_logs(logs, registry) {
        return Some(("solver-logs", flow, None));
    }
    // A batch settlement whose log read declined (a multi-order batch) must not fall through to
    // calldata: its inner router frames are order plumbing, not one trade.
    if registry.is_batch_settler(entry_point) {
        return None;
    }
    let (flow, intent) = terms_from_calldata(root, registry, logs, transfer_ledger, sender)?;
    Some(("solver-calldata", flow, Some(intent)))
}

/// The trade a solver stated in its own logs, from whichever registered solver emitted one.
fn settled_from_logs(logs: &[Log], registry: &Registry) -> Option<TraderFlow> {
    logs.iter()
        .filter_map(|log| registry.solver(log.address()))
        .find_map(|solver| match solver.decoder.declared(&[], logs, None) {
            Some(Declaration::Settled(flow)) => Some(flow),
            Some(Declaration::Terms(_)) | None => None,
        })
}

/// The settling solver frame's calldata terms, with `amount_out` recovered from the recipient the
/// same calldata declares (a solver that declares none delivers to the caller, so the transaction
/// sender is the fallback anchor).
///
/// Two guards protect against the recipient-receipt query picking up a multi-order transaction's
/// output: the recovered output must clear the intent's on-chain floor (a successful trade cleared
/// it by construction, so a violation means the wrong legs were picked up), and, when the calldata
/// also declares a quote, it must sit within `plausible_quote`'s band of the recovered output.
fn terms_from_calldata(
    root: &CallFrame,
    registry: &Registry,
    logs: &[Log],
    transfer_ledger: &TransferLedger,
    sender: Address,
) -> Option<(TraderFlow, SwapIntent)> {
    let solver_frame = trace::find_solver_frame(root, registry)?;
    let solver = registry.solver(solver_frame.to?)?;
    let intent = match solver
        .decoder
        .declared(&solver_frame.input, logs, None)?
    {
        Declaration::Terms(intent) => intent,
        // A solver that states its trade in logs already had its chance above, and its calldata
        // is not the place to read it from.
        Declaration::Settled(_) => return None,
    };
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

    let flow = TraderFlow::new(
        sender,
        NetSwap {
            token_in: intent.token_in,
            amount_in: intent.amount_in,
            token_out: intent.token_out,
            amount_out,
        },
    );
    Some((flow, intent))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, U256};

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

        let (flow, intent) = terms_from_calldata(&root, &registry, &[], &ledger, sender).unwrap();
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

        assert!(terms_from_calldata(&root, &registry, &[], &ledger, sender).is_none());
    }

    #[test]
    fn test_decode_no_recipient_receipt_declines() {
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let ledger = TransferLedger::from_transaction(&[], &[]);

        assert!(terms_from_calldata(&root, &registry, &[], &ledger, sender).is_none());
    }

    #[test]
    fn test_decode_no_solver_frame_declines() {
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = frame("CALL", sender, ROUTER, 0);
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(terms_from_calldata(&root, &registry, &[], &ledger, sender).is_none());
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

        assert!(terms_from_calldata(&root, &registry, &[], &ledger, sender).is_none());
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

        assert!(terms_from_calldata(&root, &registry, &[], &ledger, sender).is_none());
    }

    #[test]
    fn test_decode_third_party_funded_rebalance() {
        // Someone other than the sender net-sends the input token: a solver-initiated rebalance
        // still decodes from the calldata, with the intent's own amounts.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let funder = addr(99);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let logs = vec![make_transfer_log(TOKEN_IN, funder, ROUTER, U256::from(AMOUNT_IN))];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, _) = terms_from_calldata(&root, &registry, &[], &ledger, sender).unwrap();
        assert_eq!(flow.tracked, sender);
        assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
    }

    #[test]
    fn test_decode_ignores_an_input_side_fee_leg() {
        // An input-side fee leg on the way to the solver: `amount_in` stays the frame's own
        // figure, which is already what reached the solver.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let logs = vec![
            make_transfer_log(TOKEN_IN, sender, ROUTER, U256::from(AMOUNT_IN)),
            make_transfer_log(TOKEN_IN, ROUTER, addr(99), U256::from(40)),
        ];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, _) = terms_from_calldata(&root, &registry, &[], &ledger, sender).unwrap();
        assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
    }

    #[test]
    fn test_decode_needs_no_venue() {
        // A direct transaction: the root frame is itself the solver frame, and nothing about the
        // decode consults a venue.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let mut root = frame("CALL", sender, FLY, 0);
        root.input = fly_input().into();
        let logs = vec![make_transfer_log(TOKEN_IN, sender, FLY, U256::from(AMOUNT_IN))];
        // The recipient is whatever the calldata declares — here Relay's router, even though the
        // transaction never went near a venue.
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let (flow, _) = terms_from_calldata(&root, &registry, &[], &ledger, sender).unwrap();
        assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
        assert_eq!(flow.swap.amount_out, U256::from(MIN_AMOUNT_OUT + 1_000));
    }
}
