//! Relay-specific decoding.
//!
//! Relay differs from direct solver swaps in two ways: its router skims a
//! venue fee to a collector address on either side of the swap, and its
//! solvers submit rebalancing fills whose transaction sender has no net flow.

use std::collections::HashSet;

use alloy::primitives::{Address, U256};

use crate::decoder::{
    ledger::{NetSwap, TransferLedger},
    strategy::Flow,
    venues::{venue_fee_flow, VenueContext},
};

/// Decode a Relay-entered transaction.
///
/// The common case is a user swap: net the sender's flow, then back the
/// venue fee out of it. When the sender has no net flow the transaction is a
/// solver-initiated rebalancing fill, decoded by anchoring on the fee
/// collector instead (Relay funds the swap from it); the collector is the
/// funding source there, not a skim, so no fee is backed out.
pub(crate) fn decode(ctx: &VenueContext<'_>) -> Option<Flow> {
    let relay = ctx.registry.venue("relay")?;
    if let Some(flow) =
        venue_fee_flow(ctx.ledger, ctx.sender, ctx.entry_point, &relay.fee_collectors)
    {
        return Some(flow);
    }
    decode_rebalance(
        ctx.ledger,
        &relay.fee_collectors,
        &relay.entry_points,
        ctx.registry.wrapped_native(),
    )
    .map(|swap| Flow::without_fees(ctx.sender, swap))
}

/// Decode a Relay solver-initiated rebalancing fill, where `tx.from` is a rotating solver EOA with
/// no net flow (so sender netting finds nothing) and the swap moves Relay's own liquidity.
///
/// Anchors on the fee collector, which always funds the input: `token_in` is the single token it
/// net-sends. The output is one of two shapes — the token that comes back to the collector (an
/// internal inventory rebalance), or the asset received by the single external recipient that
/// only receives and never sends (a cross-chain order fill; see
/// [`TransferLedger::sink_receipts`]).
///
/// Declines (returns `None`) when the shape is ambiguous: not exactly one input token, a
/// same-token "swap", more than one token back to the collector, or more than one external
/// recipient or output (a batched multi-order fill, like the multi-leg netting guard).
fn decode_rebalance(
    ledger: &TransferLedger,
    fee_collectors: &HashSet<Address>,
    relay_entry_points: &HashSet<Address>,
    wrapped_native: Address,
) -> Option<NetSwap> {
    let net_in: Vec<(Address, U256)> = ledger
        .group_net_sent(fee_collectors)
        .into_iter()
        .collect();
    if net_in.len() != 1 {
        return None;
    }
    let (token_in, amount_in) = net_in[0];

    // C2 internal rebalance: the collector net-receives exactly one (different) token.
    let net_recv: Vec<(Address, U256)> = ledger
        .group_net_received(fee_collectors)
        .into_iter()
        .collect();
    if !net_recv.is_empty() {
        if net_recv.len() != 1 || net_recv[0].0 == token_in {
            return None;
        }
        let (token_out, amount_out) = net_recv[0];
        return Some(NetSwap { token_in, amount_in, token_out, amount_out });
    }

    // C1 external fill: the single pure-sink recipient, excluding infrastructure (routers,
    // collector, the wrapped-native token, the zero address) and the input token.
    let mut outputs: Vec<(Address, U256)> = Vec::new();
    for (recipient, token, amount) in ledger.sink_receipts() {
        if relay_entry_points.contains(&recipient) ||
            fee_collectors.contains(&recipient) ||
            recipient == Address::ZERO ||
            recipient == wrapped_native
        {
            continue;
        }
        // A payout, not a swap: the collector's token reached an external recipient unconverted
        // (a cross-chain order settled from same-token inventory). There is no conversion to
        // re-solve — pairing the leftover (e.g. a gas top-up) as "the output" fabricated
        // seven-figure-bps wins. A genuine fill routes token_in into a pool, which sends
        // something back and is therefore never a pure sink.
        if token == token_in {
            return None;
        }
        outputs.push((token, amount));
    }
    if outputs.len() != 1 {
        return None;
    }
    let (token_out, amount_out) = outputs[0];
    Some(NetSwap { token_in, amount_in, token_out, amount_out })
}

#[cfg(test)]
mod tests {
    use alloy::rpc::types::Log;

    use super::*;
    use crate::decoder::{
        registry::Registry,
        strategy::GasScope,
        test_utils::{addr, make_transfer_log, swap},
    };

    fn ledger(logs: &[Log], native: &[(Address, Address, U256)]) -> TransferLedger {
        TransferLedger::from_transaction(logs, native)
    }

    /// A [`VenueContext`] over the given transaction pieces, for concise decode calls.
    fn ctx<'a>(
        ledger: &'a TransferLedger,
        sender: Address,
        entry_point: Address,
        registry: &'a Registry,
    ) -> VenueContext<'a> {
        VenueContext { ledger, sender, entry_point, input: &[], registry }
    }

    #[test]
    fn rebalance_external_token_fill() {
        let fee = addr(99);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let token_out = addr(11);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![
            make_transfer_log(token_in, fee, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, recipient, U256::from(2000)),
        ];
        let got = decode_rebalance(&ledger(&logs, &[]), &collectors, &routers, addr(200)).unwrap();
        assert_eq!(got, swap(token_in, 1000, token_out, 2000));
    }

    #[test]
    fn rebalance_external_native_eth_out() {
        let fee = addr(99);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![make_transfer_log(token_in, fee, pool, U256::from(1000))];
        let native = vec![(pool, recipient, U256::from(2000))];
        let got =
            decode_rebalance(&ledger(&logs, &native), &collectors, &routers, addr(200)).unwrap();
        assert_eq!(got, swap(token_in, 1000, Address::ZERO, 2000));
    }

    #[test]
    fn rebalance_internal_back_to_collector() {
        let fee = addr(99);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![
            make_transfer_log(token_in, fee, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, fee, U256::from(1001)),
        ];
        let got = decode_rebalance(&ledger(&logs, &[]), &collectors, &routers, addr(200)).unwrap();
        assert_eq!(got, swap(token_in, 1000, token_out, 1001));
    }

    #[test]
    fn rebalance_declines_multi_recipient() {
        let fee = addr(99);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![
            make_transfer_log(token_in, fee, pool, U256::from(2000)),
            make_transfer_log(token_out, pool, addr(7), U256::from(1000)),
            make_transfer_log(token_out, pool, addr(8), U256::from(1000)),
        ];
        assert!(decode_rebalance(&ledger(&logs, &[]), &collectors, &routers, addr(200)).is_none());
    }

    #[test]
    fn rebalance_declines_unconverted_payout() {
        // Live tx 0x455f5202…: the collector pays out its token unconverted to an external
        // recipient (cross-chain order settled from same-token inventory) plus a tiny native gas
        // top-up. Pairing the top-up as "the output" fabricated a 10-million-bps win — a payout
        // has no conversion to re-solve and must decline.
        let fee = addr(99);
        let router = addr(2);
        let recipient = addr(7);
        let gas_recipient = addr(8);
        let token_in = addr(10);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([router]);
        let logs = vec![
            make_transfer_log(token_in, fee, router, U256::from(2_002_781_016u64)),
            make_transfer_log(token_in, router, recipient, U256::from(2_002_781_016u64)),
        ];
        let native = vec![(router, gas_recipient, U256::from(1_139_527_584_556_489u64))];
        assert!(
            decode_rebalance(&ledger(&logs, &native), &collectors, &routers, addr(200)).is_none()
        );
    }

    #[test]
    fn rebalance_declines_without_collector_outflow() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(50), U256::from(1000))];
        let collectors = HashSet::from([addr(99)]);
        let routers = HashSet::from([addr(2)]);
        assert!(decode_rebalance(&ledger(&logs, &[]), &collectors, &routers, addr(200)).is_none());
    }

    #[test]
    fn decode_backs_fee_out_of_user_flow() {
        // User swap through Relay: sender nets token_in -> token_out, with an input-side skim to
        // the real Relay collector. The fee is backed out of amount_in.
        let registry = Registry::ethereum();
        let collector = *registry
            .venue("relay")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap();
        let user = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];

        let flow = decode(&ctx(&ledger(&logs, &[]), user, router, &registry)).unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 960, token_out, 2000));
        assert_eq!(flow.venue_fee, Some(U256::from(40)));
        assert_eq!(flow.venue_fee_out, None);
        assert_eq!(flow.gas_scope, GasScope::SolverFrame);
    }

    #[test]
    fn decode_skips_fee_back_out_for_collector_trader() {
        // Treasury op (live tx 0x80a4c0…): the fee collector itself unwraps WETH via the router.
        // Its 1:1 native receipt must not be treated as a skim and added back — that doubled the
        // output.
        let registry = Registry::ethereum();
        let collector = *registry
            .venue("relay")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap();
        let router = addr(2);
        let weth = addr(10);

        let logs = vec![make_transfer_log(weth, collector, router, U256::from(1000))];
        let native = vec![(router, collector, U256::from(1000))];

        let flow = decode(&ctx(&ledger(&logs, &native), collector, router, &registry)).unwrap();
        assert_eq!(flow.tracked, collector);
        assert_eq!(flow.swap, swap(weth, 1000, Address::ZERO, 1000));
        assert_eq!(flow.venue_fee, None);
        assert_eq!(flow.venue_fee_out, None);
    }

    #[test]
    fn decode_falls_back_to_rebalance() {
        // Solver fill: the sender has no net flow; the collector funds the swap. No fee back-out.
        let registry = Registry::ethereum();
        let collector = *registry
            .venue("relay")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap();
        let solver = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, collector, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, recipient, U256::from(2000)),
        ];

        let flow = decode(&ctx(&ledger(&logs, &[]), solver, router, &registry)).unwrap();
        assert_eq!(flow.tracked, solver);
        assert_eq!(flow.swap, swap(token_in, 1000, token_out, 2000));
        assert_eq!(flow.venue_fee, None);
        assert_eq!(flow.venue_fee_out, None);
        assert_eq!(flow.gas_scope, GasScope::NotCharged);
    }
}
