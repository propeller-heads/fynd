//! Relay-specific decoding.
//!
//! Relay differs from direct aggregator swaps in two ways: its router skims a
//! client fee to a collector address on either side of the swap, and its
//! solvers submit rebalancing fills whose transaction sender has no net flow.

use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol_types::SolEvent,
};

use crate::decoder::{
    net::{to_primitive_log, NetSwap, Transfer},
    registry::Registry,
    venues::{sender_flow, Flow},
};

/// Decode a Relay-entered transaction.
///
/// The common case is a user swap: net the sender's flow, then back the
/// client fee out of it. When the sender has no net flow the transaction is a
/// solver-initiated rebalancing fill, decoded by anchoring on the fee
/// collector instead (Relay funds the swap from it); the collector is the
/// funding source there, not a skim, so no fee is backed out.
pub(crate) fn decode(
    logs: &[Log],
    native: &[(Address, Address, U256)],
    sender: Address,
    entry_point: Address,
    registry: &Registry,
) -> Option<Flow> {
    if let Some(flow) = sender_flow(logs, native, sender, entry_point) {
        return Some(back_out_client_fees(flow, logs, native, &registry.relay().fee_collectors));
    }
    decode_rebalance(
        logs,
        native,
        &registry.relay().fee_collectors,
        &registry.relay().routers,
        registry.wrapped_native(),
    )
    .map(|swap| Flow::without_fees(sender, swap))
}

/// Back a client-fee skim out of a decoded user flow.
///
/// The collector can skim on either side. An input-side skim is subtracted
/// from `amount_in` (the user's gross spend included money that never entered
/// the swap) and an output-side skim is added back into `amount_out` (the
/// swap produced more than the user kept), so both sides are the amounts
/// actually swapped — the like-for-like basis vs Fynd.
fn back_out_client_fees(
    flow: Flow,
    logs: &[Log],
    native: &[(Address, Address, U256)],
    fee_collectors: &HashSet<Address>,
) -> Flow {
    let fees = fee_to_collectors(logs, native, fee_collectors);
    let client_fee = fees
        .get(&flow.swap.token_in)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_in =
        client_fee.map_or(flow.swap.amount_in, |fee| flow.swap.amount_in.saturating_sub(fee));
    let client_fee_out = fees
        .get(&flow.swap.token_out)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_out =
        client_fee_out.map_or(flow.swap.amount_out, |fee| flow.swap.amount_out.saturating_add(fee));
    Flow {
        tracked: flow.tracked,
        swap: NetSwap { amount_in, amount_out, ..flow.swap },
        client_fee,
        client_fee_out,
    }
}

/// Total of each token transferred to a known client fee-collector within the transaction, keyed
/// by token (native ETH is [`Address::ZERO`]).
///
/// Relay skims its fee by sending part of the input token to a fee collector before swapping, so
/// the user's netted `amount_in` includes money that never entered the swap. Backing that fee out
/// lets the re-solve compare Fynd against the client on the amount actually routed, rather than
/// crediting Fynd with the client's fee. Matches by recipient regardless of sender, so it catches
/// both a direct user skim and a router skim.
fn fee_to_collectors(
    logs: &[Log],
    native_transfers: &[(Address, Address, U256)],
    fee_collectors: &HashSet<Address>,
) -> HashMap<Address, U256> {
    let mut fees: HashMap<Address, U256> = HashMap::new();
    if fee_collectors.is_empty() {
        return fees;
    }
    for &(_, to, value) in native_transfers {
        if fee_collectors.contains(&to) {
            *fees.entry(Address::ZERO).or_default() += value;
        }
    }
    for log in logs {
        let primitive = to_primitive_log(log);
        let Ok(transfer) = Transfer::decode_log(&primitive) else {
            continue;
        };
        if fee_collectors.contains(&transfer.to) {
            *fees.entry(log.address()).or_default() += transfer.value;
        }
    }
    fees
}

/// Decode a Relay solver-initiated rebalancing fill, where `tx.from` is a rotating solver EOA with
/// no net flow (so sender netting finds nothing) and the swap moves Relay's own liquidity.
///
/// Anchors on the fee collector, which always funds the input: `token_in` is the single token it
/// net-sends. The output is either the token that returns to the collector (an **internal**
/// inventory rebalance) or the asset received by the single external **pure-sink** recipient — an
/// address that receives but never sends — for a cross-chain order fill.
///
/// Returns `None` (declines) when the shape is ambiguous: not exactly one input token, a same-token
/// "swap", more than one token back to the collector, or more than one external recipient/output
/// (a batched multi-order fill, like the multi-leg netting guard).
fn decode_rebalance(
    logs: &[Log],
    native_transfers: &[(Address, Address, U256)],
    fee_collectors: &HashSet<Address>,
    relay_routers: &HashSet<Address>,
    wrapped_native: Address,
) -> Option<NetSwap> {
    let mut sent: HashMap<(Address, Address), U256> = HashMap::new();
    let mut received: HashMap<(Address, Address), U256> = HashMap::new();
    let mut senders: HashSet<Address> = HashSet::new();

    for &(from, to, value) in native_transfers {
        senders.insert(from);
        *sent
            .entry((from, Address::ZERO))
            .or_default() += value;
        *received
            .entry((to, Address::ZERO))
            .or_default() += value;
    }
    for log in logs {
        let Ok(transfer) = Transfer::decode_log(&to_primitive_log(log)) else {
            continue;
        };
        let token = log.address();
        senders.insert(transfer.from);
        *sent
            .entry((transfer.from, token))
            .or_default() += transfer.value;
        *received
            .entry((transfer.to, token))
            .or_default() += transfer.value;
    }

    // Aggregate the fee collector(s)' flow per token.
    let mut fc_sent: HashMap<Address, U256> = HashMap::new();
    let mut fc_recv: HashMap<Address, U256> = HashMap::new();
    for (&(addr, token), &v) in &sent {
        if fee_collectors.contains(&addr) {
            *fc_sent.entry(token).or_default() += v;
        }
    }
    for (&(addr, token), &v) in &received {
        if fee_collectors.contains(&addr) {
            *fc_recv.entry(token).or_default() += v;
        }
    }

    // token_in: the single token the collector net-sends.
    let net_in: Vec<(Address, U256)> = fc_sent
        .iter()
        .filter_map(|(&token, &s)| {
            let r = fc_recv
                .get(&token)
                .copied()
                .unwrap_or_default();
            (s > r).then_some((token, s - r))
        })
        .collect();
    if net_in.len() != 1 {
        return None;
    }
    let (token_in, amount_in) = net_in[0];

    // C2 internal rebalance: the collector net-receives exactly one (different) token.
    let net_recv: Vec<(Address, U256)> = fc_recv
        .iter()
        .filter_map(|(&token, &r)| {
            let s = fc_sent
                .get(&token)
                .copied()
                .unwrap_or_default();
            (r > s).then_some((token, r - s))
        })
        .collect();
    if !net_recv.is_empty() {
        if net_recv.len() != 1 || net_recv[0].0 == token_in {
            return None;
        }
        let (token_out, amount_out) = net_recv[0];
        return Some(NetSwap { token_in, amount_in, token_out, amount_out });
    }

    // C1 external fill: the single pure-sink recipient (receives but never sends), excluding
    // infrastructure (routers, collector, the wrapped-native token, the zero address) and the
    // input token.
    let mut outputs: Vec<(Address, Address, U256)> = Vec::new(); // (recipient, token_out, amount)
    for (&(addr, token), &v) in &received {
        if token == token_in || v.is_zero() || senders.contains(&addr) {
            continue;
        }
        if relay_routers.contains(&addr) ||
            fee_collectors.contains(&addr) ||
            addr == Address::ZERO ||
            addr == wrapped_native
        {
            continue;
        }
        outputs.push((addr, token, v));
    }
    if outputs.len() != 1 {
        return None;
    }
    let (_, token_out, amount_out) = outputs[0];
    Some(NetSwap { token_in, amount_in, token_out, amount_out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    #[test]
    fn fee_to_collectors_totals_input_skim() {
        let user = addr(1);
        let router = addr(2);
        let collector = addr(99);
        let token_in = addr(10);
        let pool = addr(50);
        let collectors = HashSet::from([collector]);

        // Router skims part of the input token to the collector; the rest goes to the pool.
        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
        ];
        let fees = fee_to_collectors(&logs, &[], &collectors);
        assert_eq!(fees.get(&token_in).copied(), Some(U256::from(40)));
    }

    #[test]
    fn fee_to_collectors_totals_output_skim() {
        let user = addr(1);
        let pool = addr(50);
        let collector = addr(99);
        let token_out = addr(11);
        let collectors = HashSet::from([collector]);

        // Pool sends the output; part is skimmed to the collector, the rest to the user. The fee
        // map keys this by token_out so the decoder can add it back to the gross swap output.
        let logs = vec![
            make_transfer_log(token_out, pool, collector, U256::from(30)),
            make_transfer_log(token_out, pool, user, U256::from(970)),
        ];
        let fees = fee_to_collectors(&logs, &[], &collectors);
        assert_eq!(fees.get(&token_out).copied(), Some(U256::from(30)));
    }

    #[test]
    fn fee_to_collectors_empty_set_is_noop() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(99), U256::from(40))];
        assert!(fee_to_collectors(&logs, &[], &HashSet::new()).is_empty());
    }

    #[test]
    fn rebalance_external_token_fill() {
        // Collector funds token_in -> pool -> token_out delivered to an external recipient.
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
        let got = decode_rebalance(&logs, &[], &collectors, &routers, addr(200)).unwrap();
        assert_eq!(got, swap(token_in, 1000, token_out, 2000));
    }

    #[test]
    fn rebalance_external_native_eth_out() {
        // C1 with native-ETH output: collector sends token_in, router delivers ETH to recipient.
        let fee = addr(99);
        let router = addr(2);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([router]);
        let logs = vec![make_transfer_log(token_in, fee, pool, U256::from(1000))];
        let native = vec![(router, recipient, U256::from(2000))];
        let got = decode_rebalance(&logs, &native, &collectors, &routers, addr(200)).unwrap();
        assert_eq!(got, swap(token_in, 1000, Address::ZERO, 2000));
    }

    #[test]
    fn rebalance_internal_back_to_collector() {
        // C2: collector sends token_in, receives token_out back (inventory rebalance).
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
        let got = decode_rebalance(&logs, &[], &collectors, &routers, addr(200)).unwrap();
        assert_eq!(got, swap(token_in, 1000, token_out, 1001));
    }

    #[test]
    fn rebalance_declines_multi_recipient() {
        // One input token but two external recipients = batched multi-order fill -> decline.
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
        assert!(decode_rebalance(&logs, &[], &collectors, &routers, addr(200)).is_none());
    }

    #[test]
    fn rebalance_declines_without_collector_outflow() {
        // No transfer from a fee collector -> nothing to anchor on.
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(50), U256::from(1000))];
        let collectors = HashSet::from([addr(99)]);
        let routers = HashSet::from([addr(2)]);
        assert!(decode_rebalance(&logs, &[], &collectors, &routers, addr(200)).is_none());
    }

    #[test]
    fn decode_backs_fee_out_of_user_flow() {
        // User swap through Relay: sender nets token_in -> token_out, with an input-side skim to
        // the real Relay collector. The fee is backed out of amount_in.
        let registry = Registry::ethereum();
        let collector = *registry
            .relay()
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

        let flow = decode(&logs, &[], user, router, &registry).unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 960, token_out, 2000));
        assert_eq!(flow.client_fee, Some(U256::from(40)));
        assert_eq!(flow.client_fee_out, None);
    }

    #[test]
    fn decode_falls_back_to_rebalance() {
        // Solver fill: the sender has no net flow; the collector funds the swap. No fee back-out.
        let registry = Registry::ethereum();
        let collector = *registry
            .relay()
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

        let flow = decode(&logs, &[], solver, router, &registry).unwrap();
        assert_eq!(flow.tracked, solver);
        assert_eq!(flow.swap, swap(token_in, 1000, token_out, 2000));
        assert_eq!(flow.client_fee, None);
        assert_eq!(flow.client_fee_out, None);
    }
}
