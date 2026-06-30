use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, Log as PrimitiveLog, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};

sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}

/// Convert an RPC log to a primitive log for event decoding.
pub(crate) fn to_primitive_log(log: &Log) -> PrimitiveLog {
    PrimitiveLog::new_unchecked(log.address(), log.topics().to_vec(), log.data().data.clone())
}

/// Determine what the tracked address sent and received, netting
/// intermediate hops, from ERC-20 Transfer logs and native ETH transfers.
pub(crate) fn decode_trade(
    logs: &[Log],
    native_transfers: &[(Address, Address, U256)],
    tracked: Address,
) -> Option<(Address, U256, Address, U256)> {
    let mut sent: HashMap<Address, U256> = HashMap::new();
    let mut received: HashMap<Address, U256> = HashMap::new();

    for &(from, to, value) in native_transfers {
        if from == tracked {
            *sent.entry(Address::ZERO).or_default() += value;
        }
        if to == tracked {
            *received
                .entry(Address::ZERO)
                .or_default() += value;
        }
    }

    for log in logs {
        let primitive = to_primitive_log(log);
        let Ok(transfer) = Transfer::decode_log(&primitive) else {
            continue;
        };
        let token = log.address();
        if transfer.from == tracked {
            *sent.entry(token).or_default() += transfer.value;
        }
        if transfer.to == tracked {
            *received.entry(token).or_default() += transfer.value;
        }
    }

    net_trade(&sent, &received)
}

/// Total of each token transferred to a known client fee-collector within the transaction, keyed
/// by token (native ETH is [`Address::ZERO`]).
///
/// A client like Relay skims its fee by sending part of the input token to a fee collector before
/// swapping, so the user's netted `amount_in` includes money that never entered the swap. Backing
/// that fee out lets the re-solve compare Fynd against the client on the amount actually routed,
/// rather than crediting Fynd with the client's fee. Matches by recipient regardless of sender, so
/// it catches both a direct user skim and a router skim.
pub(crate) fn fee_to_collectors(
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

/// Net the sent and received balances into a single swap.
///
/// Returns `None` unless exactly one token nets out and exactly one token nets in. A net with more
/// than one token on a side is a batch settlement (e.g. a CoW solver settling many orders, where
/// the tracked sender is the solver, not a trader), not a single comparable swap. Amounts across
/// tokens with different decimals are not comparable, so guessing a "dominant" leg would pair
/// unrelated tokens — declining keeps the re-solve comparison honest.
fn net_trade(
    sent: &HashMap<Address, U256>,
    received: &HashMap<Address, U256>,
) -> Option<(Address, U256, Address, U256)> {
    let mut net_sent: HashMap<Address, U256> = HashMap::new();
    let mut net_received: HashMap<Address, U256> = HashMap::new();

    let all_tokens: HashSet<Address> = sent
        .keys()
        .chain(received.keys())
        .copied()
        .collect();
    for token in all_tokens {
        let s = sent
            .get(&token)
            .copied()
            .unwrap_or_default();
        let r = received
            .get(&token)
            .copied()
            .unwrap_or_default();
        if s > r {
            net_sent.insert(token, s - r);
        } else if r > s {
            net_received.insert(token, r - s);
        }
    }

    let (&token_in, &amount_in) = net_sent
        .iter()
        .next()
        .filter(|_| net_sent.len() == 1)?;
    let (&token_out, &amount_out) = net_received
        .iter()
        .next()
        .filter(|_| net_received.len() == 1)?;
    Some((token_in, amount_in, token_out, amount_out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    #[test]
    fn simple_swap() {
        let sender = addr(1);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, sender, addr(50), U256::from(1000)),
            make_transfer_log(token_b, addr(50), sender, U256::from(2000)),
        ];

        let result = decode_trade(&logs, &[], sender).unwrap();
        assert_eq!(result, (token_a, U256::from(1000), token_b, U256::from(2000)));
    }

    #[test]
    fn multi_hop_nets_correctly() {
        let sender = addr(1);
        let token_a = addr(10);
        let token_b = addr(11);
        let token_mid = addr(12);

        // sender -1000-> token_a, mid in and back out (nets to 0), 2000 token_b back.
        let logs = vec![
            make_transfer_log(token_a, sender, addr(50), U256::from(1000)),
            make_transfer_log(token_mid, addr(50), sender, U256::from(500)),
            make_transfer_log(token_mid, sender, addr(51), U256::from(500)),
            make_transfer_log(token_b, addr(51), sender, U256::from(2000)),
        ];

        let result = decode_trade(&logs, &[], sender).unwrap();
        assert_eq!(result, (token_a, U256::from(1000), token_b, U256::from(2000)));
    }

    #[test]
    fn token_in_native_eth_out() {
        // The real failure mode: user sends a token, the router unwraps WETH
        // and returns native ETH (a trace transfer, never a log).
        let user = addr(1);
        let router = addr(2);
        let token = addr(10);
        let pool = addr(50);

        let logs = vec![make_transfer_log(token, user, pool, U256::from(1000))];
        let native = vec![(router, user, U256::from(2000))];

        let result = decode_trade(&logs, &native, user).unwrap();
        assert_eq!(result, (token, U256::from(1000), Address::ZERO, U256::from(2000)));
    }

    #[test]
    fn native_eth_in_token_out() {
        // ETH -> token: native ETH in via the top-level call, token out via log.
        let user = addr(1);
        let router = addr(2);
        let token = addr(11);
        let pool = addr(50);

        let logs = vec![make_transfer_log(token, pool, user, U256::from(2000))];
        let native = vec![(user, router, U256::from(1000))];

        let result = decode_trade(&logs, &native, user).unwrap();
        assert_eq!(result, (Address::ZERO, U256::from(1000), token, U256::from(2000)));
    }

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
    fn fee_to_collectors_empty_set_is_noop() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(99), U256::from(40))];
        assert!(fee_to_collectors(&logs, &[], &HashSet::new()).is_empty());
    }

    #[test]
    fn no_sender_flow() {
        let sender = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(50), addr(51), U256::from(1000))];
        assert!(decode_trade(&logs, &[], sender).is_none());
    }

    #[test]
    fn multi_token_batch_settlement_declined() {
        // A batch settler (e.g. a CoW solver) nets two distinct tokens in and two out across
        // several orders. That is not one swap, so picking a "dominant" leg by raw amount would
        // pair unrelated tokens. Decline instead of guessing.
        let settler = addr(1);
        let token_a = addr(10);
        let token_b = addr(11);
        let token_c = addr(12);
        let token_d = addr(13);

        let logs = vec![
            make_transfer_log(token_a, settler, addr(50), U256::from(1_000)),
            make_transfer_log(token_b, settler, addr(51), U256::from(2_000)),
            make_transfer_log(token_c, addr(52), settler, U256::from(3_000)),
            make_transfer_log(token_d, addr(53), settler, U256::from(4_000)),
        ];

        assert!(decode_trade(&logs, &[], settler).is_none());
    }

    #[test]
    fn one_in_many_out_declined() {
        // One token in but two distinct tokens out (a split/batch fill) is also ambiguous.
        let settler = addr(1);
        let token_a = addr(10);
        let token_c = addr(12);
        let token_d = addr(13);

        let logs = vec![
            make_transfer_log(token_a, settler, addr(50), U256::from(1_000)),
            make_transfer_log(token_c, addr(52), settler, U256::from(3_000)),
            make_transfer_log(token_d, addr(53), settler, U256::from(4_000)),
        ];

        assert!(decode_trade(&logs, &[], settler).is_none());
    }
}
