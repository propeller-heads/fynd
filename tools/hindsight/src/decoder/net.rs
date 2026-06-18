use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, Log as PrimitiveLog, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use tracing::warn;

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

/// Net the sent and received balances and pick the dominant token on
/// each side. Returns `None` if either side is empty after netting.
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

    if net_sent.is_empty() || net_received.is_empty() {
        return None;
    }

    if net_sent.len() > 1 || net_received.len() > 1 {
        warn!(
            tokens_in = net_sent.len(),
            tokens_out = net_received.len(),
            "multi-token trade detected, using largest amounts"
        );
    }

    let (&token_in, &amount_in) = net_sent
        .iter()
        .max_by_key(|(_, v)| *v)?;
    let (&token_out, &amount_out) = net_received
        .iter()
        .max_by_key(|(_, v)| *v)?;
    Some((token_in, amount_in, token_out, amount_out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_support::{addr, make_transfer_log};

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
    fn no_sender_flow() {
        let sender = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(50), addr(51), U256::from(1000))];
        assert!(decode_trade(&logs, &[], sender).is_none());
    }
}
