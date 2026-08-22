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
    primitives::{Address, U256},
    rpc::types::{trace::geth::CallFrame, Log},
};

use crate::decoder::{
    registry::Registry,
    solvers::{self, DeclaredSwap},
    trace,
    transfer_ledger::{SettledSwap, TransferLedger},
    veto::Veto,
};

/// Decode a transaction from the settling solver's own declaration: the decoder label recorded on
/// the trade, the settled swap, and the terms the solver declared alongside it.
///
/// Only the settling solver is asked — the outermost known solver in the trace — so a record's
/// amounts and its solver label always come from the same solver. A transaction that merely
/// touches another solver's router somewhere is not read by that solver.
///
/// `Ok(None)` means no solver was read at all — no known solver frame, or the settling solver's
/// own data carried nothing it could parse — so the caller falls back to netting.
///
/// `Err(veto)` means the transaction is dropped, and netting is not tried. Either the solver said
/// it is not a swap, or it named an output that the transfers do not show: once a solver has told
/// us the trade, netting would answer a different question, so its answer is not substituted.
pub(crate) fn declared_flow(
    root: &CallFrame,
    registry: &Registry,
    logs: &[Log],
    transfer_ledger: &TransferLedger,
    sender: Address,
) -> Result<Option<(&'static str, SettledSwap, DeclaredSwap)>, Veto> {
    let Some(solver_frame) = trace::find_solver_frame(root, registry) else { return Ok(None) };
    let Some(solver) = solver_frame
        .to
        .and_then(|address| registry.solver(address))
    else {
        return Ok(None);
    };
    let Some(mut declared) = solver
        .decoder
        .declared(&solver_frame.input, logs)?
    else {
        return Ok(None);
    };
    // An event states the output outright; calldata never does, so it is recovered from the
    // recipient's receipt. That is the only difference between the two, and the label records it.
    let (label, amount_out) = match declared.amount_out {
        Some(amount_out) => ("solver-logs", amount_out),
        None => (
            "solver-calldata",
            recover_output(&declared, transfer_ledger, sender).ok_or(Veto::OutputNotFound)?,
        ),
    };
    // A quote is self-reported decoration: integrators sometimes fill it in a different token or
    // decimal basis, which would fabricate a huge slippage. One that far from the settled amount
    // is dropped, and the trade — whose tokens and amounts are ABI-decoded facts — is kept.
    if let Some(quoted) = declared.declared_quote {
        if !solvers::plausible_quote(quoted, amount_out) {
            declared.declared_quote = None;
            declared.timestamp = None;
        }
    }
    let settled = SettledSwap {
        tracked: declared.tracked.unwrap_or(sender),
        token_in: declared.token_in,
        amount_in: declared.amount_in,
        token_out: declared.token_out,
        amount_out,
    };
    Ok(Some((label, settled, declared)))
}

/// Recover a settled `amount_out` the solver's data did not state: the gross amount the declared
/// recipient received (a solver that declares none delivers to the caller, so the transaction
/// sender is the fallback anchor).
///
/// `None` when the recipient received none of the token, or less than the floor the same calldata
/// enforces — a settled trade cleared its floor by construction, so a smaller receipt means the
/// query read the wrong legs of a multi-order transaction. The caller drops the transaction on
/// either, rather than letting netting answer instead.
fn recover_output(
    declared: &DeclaredSwap,
    transfer_ledger: &TransferLedger,
    sender: Address,
) -> Option<U256> {
    let recipient = declared
        .output_recipient
        .unwrap_or(sender);
    let amount_out = transfer_ledger.received_by_address(recipient, declared.token_out);
    if amount_out.is_zero() ||
        amount_out <
            declared
                .min_amount_out
                .unwrap_or(U256::ZERO)
    {
        return None;
    }
    Some(amount_out)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, frame, make_transfer_log};

    /// Fly's own router — same address on every chain (`docs.fly.trade`).
    const FLY: Address = address!("0x20f6ee51340adeed01a59b0e65cb3703f3dc860c");
    /// 0x's v4 exchange proxy — a registered solver with no declared read.
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

    /// `LiFi`'s Diamond, and a bridge-shaped log emitted by it.
    const LIFI: Address = address!("0x1231deb6f5749ef6ce6943a275a1d3e7486f4eae");

    fn bridge_log(emitter: Address) -> Log {
        use alloy::sol_types::SolEvent;

        let primitive = alloy::primitives::Log::new_unchecked(
            emitter,
            vec![solvers::lifi::LiFiTransferStarted::SIGNATURE_HASH],
            alloy::primitives::Bytes::default(),
        );
        Log { inner: primitive, ..Default::default() }
    }

    #[test]
    fn test_bridge_order_vetoes_the_whole_transaction() {
        // LiFi is the settling solver and its log says the order bridged out: the veto reaches
        // the caller, which drops the transaction instead of letting netting pair the input with
        // the dust refund.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, LIFI);
        let ledger = TransferLedger::from_transaction(&[], &[]);

        let logs = vec![bridge_log(LIFI)];
        assert_eq!(
            declared_flow(&root, &registry, &logs, &ledger, sender).err(),
            Some(Veto::BridgeOrder)
        );
    }

    #[test]
    fn test_bridge_log_from_another_solvers_transaction_does_not_veto() {
        // The same bridge-shaped log, but Fly settled this transaction. Only the settling solver
        // is asked, so LiFi's veto cannot reach a trade that is not LiFi's.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let logs = vec![bridge_log(LIFI)];
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        let (_, flow, _) = declared_flow(&root, &registry, &logs, &ledger, sender)
            .unwrap()
            .unwrap();
        assert_eq!(flow.amount_in, U256::from(AMOUNT_IN));
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

        let (_, flow, declared) = declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .unwrap();
        assert_eq!(flow.tracked, sender);
        assert_eq!(flow.token_in, TOKEN_IN);
        assert_eq!(flow.token_out, Address::ZERO);
        assert_eq!(flow.amount_in, U256::from(AMOUNT_IN));
        assert_eq!(flow.amount_out, U256::from(MIN_AMOUNT_OUT + 1_000));
        assert_eq!(declared.min_amount_out, Some(U256::from(MIN_AMOUNT_OUT)));
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

        assert_eq!(
            declared_flow(&root, &registry, &[], &ledger, sender).err(),
            Some(Veto::OutputNotFound)
        );
    }

    #[test]
    fn test_decode_no_recipient_receipt_drops_the_transaction() {
        // Fly's calldata names the token and the address it is paid to, and that address received
        // none of it. Netting is not asked instead — the solver already told us the trade.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let ledger = TransferLedger::from_transaction(&[], &[]);

        assert_eq!(
            declared_flow(&root, &registry, &[], &ledger, sender).err(),
            Some(Veto::OutputNotFound)
        );
    }

    #[test]
    fn test_decode_no_solver_frame_declines() {
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = frame("CALL", sender, ROUTER, 0);
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_decode_solver_without_a_declared_read_declines() {
        // 0x's v4 proxy is a registered solver (matches `find_solver_frame`) but has no
        // `declared` implementation: the calldata path has nothing to recover, so it falls
        // through to netting.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, ZEROX);
        let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
        let ledger = TransferLedger::from_transaction(&[], &native);

        assert!(declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_decode_implausible_quote_drops_the_quote_not_the_trade() {
        // A recovered output more than 2x the declared quote reads as a unit mismatch, so the
        // quote goes. The tokens and amounts are ABI-decoded facts, so the trade stays.
        let registry = Registry::ethereum();
        let sender = addr(1);
        let root = root_with_solver_frame(sender, ROUTER, FLY);
        let implausible = U256::from(QUOTED_AMOUNT_OUT) * U256::from(3u64);
        let native = vec![(addr(50), ROUTER, implausible)];
        let ledger = TransferLedger::from_transaction(&[], &native);

        let (_, flow, declared) = declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .unwrap();
        assert_eq!(flow.amount_out, implausible);
        assert_eq!(declared.declared_quote, None);
        assert_eq!(declared.min_amount_out, Some(U256::from(MIN_AMOUNT_OUT)));
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

        let (_, flow, _) = declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .unwrap();
        assert_eq!(flow.tracked, sender);
        assert_eq!(flow.amount_in, U256::from(AMOUNT_IN));
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

        let (_, flow, _) = declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .unwrap();
        assert_eq!(flow.amount_in, U256::from(AMOUNT_IN));
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

        let (_, flow, _) = declared_flow(&root, &registry, &[], &ledger, sender)
            .unwrap()
            .unwrap();
        assert_eq!(flow.amount_in, U256::from(AMOUNT_IN));
        assert_eq!(flow.amount_out, U256::from(MIN_AMOUNT_OUT + 1_000));
    }
}
