use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, Log as PrimitiveLog, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use tracing::debug;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event TransferSingle(
        address indexed operator, address indexed from, address indexed to, uint256 id,
        uint256 value
    );
    event TransferBatch(
        address indexed operator, address indexed from, address indexed to, uint256[] ids,
        uint256[] values
    );
}

/// Convert an RPC log to a primitive log for event decoding.
pub(crate) fn to_primitive_log(log: &Log) -> PrimitiveLog {
    PrimitiveLog::new_unchecked(log.address(), log.topics().to_vec(), log.data().data.clone())
}

/// Whether `recipient` received an NFT (ERC-721 or ERC-1155) in the transaction.
///
/// An ERC-721 `Transfer` shares the ERC-20 event signature but indexes all three parameters
/// (four topics, empty data), so it is invisible to ERC-20 netting; ERC-1155 uses its own
/// events with the recipient as the third indexed parameter.
pub(crate) fn received_nft(logs: &[Log], recipient: Address) -> bool {
    for log in logs {
        let topics = log.topics();
        let Some(&signature) = topics.first() else {
            continue;
        };
        let to = if signature == Transfer::SIGNATURE_HASH && topics.len() == 4 {
            topics[2]
        } else if (signature == TransferSingle::SIGNATURE_HASH ||
            signature == TransferBatch::SIGNATURE_HASH) &&
            topics.len() == 4
        {
            topics[3]
        } else {
            continue;
        };
        if Address::from_word(to) == recipient {
            return true;
        }
    }
    false
}

/// A netted swap: the single token (and amount) that left an address and the
/// single token that came back. Native ETH is [`Address::ZERO`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NetSwap {
    pub token_in: Address,
    pub amount_in: U256,
    pub token_out: Address,
    pub amount_out: U256,
}

/// A wrap-pair trade (native <-> wrapped native) more than this factor off 1:1 is mis-paired:
/// wrapping is exactly 1:1 by construction, and fee skims only shave a few percent, so nothing
/// legitimate strays this far.
const WRAP_PAIR_MAX_RATIO: u64 = 2;

/// Whether a "swap" between the native token and its wrapped form has amounts a wrap or unwrap
/// cannot produce.
///
/// Seen with cross-chain deposits where the trader sends WETH and the only same-chain receipt is
/// a dust remainder refund in native ETH — netting pairs the two into a phantom trade orders of
/// magnitude off parity.
pub(crate) fn wrap_pair_mispaired(swap: &NetSwap, wrapped_native: Address) -> bool {
    let pair = [swap.token_in, swap.token_out];
    if !(pair.contains(&Address::ZERO) && pair.contains(&wrapped_native)) {
        return false;
    }
    let max = U256::from(WRAP_PAIR_MAX_RATIO);
    swap.amount_in > swap.amount_out.saturating_mul(max) ||
        swap.amount_out > swap.amount_in.saturating_mul(max)
}

/// A token's flow through the whole transaction, tracked alongside the netted
/// per-address balances so residue legs can be told apart from real ones.
#[derive(Default)]
struct TokenFlow {
    /// Total value of the token moved in the transaction, by any party.
    gross: U256,
    /// Whether the token also moved between two parties other than the
    /// tracked address — the signature of a routing intermediate.
    intermediate: bool,
}

/// Determine what the tracked address sent and received, netting
/// intermediate hops, from ERC-20 Transfer logs and native ETH transfers.
pub(crate) fn decode_trade(
    logs: &[Log],
    native_transfers: &[(Address, Address, U256)],
    tracked: Address,
) -> Option<NetSwap> {
    // (token, from, to, value) for every transfer in the transaction; native ETH is token ZERO.
    let mut transfers: Vec<(Address, Address, Address, U256)> = Vec::new();
    for &(from, to, value) in native_transfers {
        transfers.push((Address::ZERO, from, to, value));
    }
    for log in logs {
        let primitive = to_primitive_log(log);
        let Ok(transfer) = Transfer::decode_log(&primitive) else {
            continue;
        };
        transfers.push((log.address(), transfer.from, transfer.to, transfer.value));
    }

    let mut sent: HashMap<Address, U256> = HashMap::new();
    let mut received: HashMap<Address, U256> = HashMap::new();
    let mut flows: HashMap<Address, TokenFlow> = HashMap::new();
    for &(token, from, to, value) in &transfers {
        let flow = flows.entry(token).or_default();
        flow.gross = flow.gross.saturating_add(value);
        if from != tracked && to != tracked {
            flow.intermediate = true;
        }
        if from == tracked {
            *sent.entry(token).or_default() += value;
        }
        if to == tracked {
            *received.entry(token).or_default() += value;
        }
    }

    net_trade(&sent, &received, &flows)
}

/// A net leg is residue when its token routed between third parties and the leg is under this
/// fraction of the token's gross transaction flow: `net * RESIDUE_GROSS_RATIO < gross` (1%).
const RESIDUE_GROSS_RATIO: u64 = 100;

/// Net the sent and received balances into a single swap.
///
/// Returns `None` unless exactly one token nets out and exactly one token nets in. A net with more
/// than one token on a side is a batch settlement (e.g. a CoW solver settling many orders, where
/// the tracked sender is the solver, not a trader), not a single comparable swap. Amounts across
/// tokens with different decimals are not comparable, so guessing a "dominant" leg would pair
/// unrelated tokens — declining keeps the re-solve comparison honest.
///
/// One exception: a genuine single swap can carry a **residue leg** — an RFQ hop consumes an exact
/// intermediate amount and the surplus lands on the trader, or rounding leaves dust of a routing
/// token. An ambiguous side first drops legs that are provably residue: the token also moved
/// between parties other than the tracked address (a routing intermediate) *and* the leg is under
/// 1% of the token's gross flow in the transaction. Both conditions are same-token comparisons, so
/// no prices are needed, and a real batch leg (all of its token's flow) is never dropped. Residue
/// on a token that never routed third-party (e.g. a rebasing input token) and fixed-token protocol
/// fees still decline, logged at debug level.
fn net_trade(
    sent: &HashMap<Address, U256>,
    received: &HashMap<Address, U256>,
    flows: &HashMap<Address, TokenFlow>,
) -> Option<NetSwap> {
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

    drop_residue_legs(&mut net_sent, flows);
    drop_residue_legs(&mut net_received, flows);

    if net_sent.len() != 1 || net_received.len() != 1 {
        // Flow on both sides but more than one significant token on one of them: a real batch
        // settlement, or a residue leg the pruning rules cannot prove (see the docstring).
        if !net_sent.is_empty() && !net_received.is_empty() {
            debug!(?net_sent, ?net_received, "declining multi-token net flow");
        }
        return None;
    }
    let (&token_in, &amount_in) = net_sent.iter().next()?;
    let (&token_out, &amount_out) = net_received.iter().next()?;
    Some(NetSwap { token_in, amount_in, token_out, amount_out })
}

/// Drop residue legs from one side of an ambiguous net (see [`net_trade`]). Only runs when the
/// side has more than one leg — a lone leg is the swap itself, however small.
fn drop_residue_legs(net: &mut HashMap<Address, U256>, flows: &HashMap<Address, TokenFlow>) {
    if net.len() <= 1 {
        return;
    }
    net.retain(|token, amount| {
        let Some(flow) = flows.get(token) else {
            return true;
        };
        let residue = flow.intermediate &&
            amount.saturating_mul(U256::from(RESIDUE_GROSS_RATIO)) < flow.gross;
        !residue
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::{addr, make_nft_transfer_log, make_transfer_log, swap};

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
        assert_eq!(result, swap(token_a, 1000, token_b, 2000));
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
        assert_eq!(result, swap(token_a, 1000, token_b, 2000));
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
        assert_eq!(result, swap(token, 1000, Address::ZERO, 2000));
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
        assert_eq!(result, swap(Address::ZERO, 1000, token, 2000));
    }

    #[test]
    fn received_nft_detects_erc721() {
        // The NFT purchase shape: buyer pays a token and receives an ERC-721, not a token amount.
        let buyer = addr(1);
        let seller = addr(2);
        let collection = addr(60);

        let logs = vec![make_nft_transfer_log(collection, seller, buyer, 4002)];
        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn received_nft_detects_erc1155_single() {
        let buyer = addr(1);
        let operator = addr(3);
        let seller = addr(2);
        let collection = addr(60);

        let event = TransferSingle {
            operator,
            from: seller,
            to: buyer,
            id: U256::from(7),
            value: U256::from(1),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(collection, data.topics().to_vec(), data.data.clone());
        let logs = vec![Log { inner: primitive, ..Default::default() }];

        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn received_nft_ignores_erc20_transfers() {
        // A plain ERC-20 Transfer (three topics, amount in data) must not read as an NFT even
        // though it shares the event signature.
        let user = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(2), user, U256::from(1000))];
        assert!(!received_nft(&logs, user));
    }

    #[test]
    fn rfq_surplus_residue_dropped() {
        // USDC -> WETH -> DAI where the second hop is an RFQ consuming an exact WETH amount: the
        // surplus WETH lands on the user as a second net-in token. WETH routed third-party
        // (pool -> router) and the surplus is <1% of its gross flow, so it is provably residue.
        let user = addr(1);
        let pool = addr(50);
        let router = addr(2);
        let token_a = addr(10);
        let token_mid = addr(12);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, user, pool, U256::from(1000)),
            make_transfer_log(token_mid, pool, router, U256::from(10_000)),
            make_transfer_log(token_mid, router, user, U256::from(50)),
            make_transfer_log(token_b, router, user, U256::from(2000)),
        ];

        let result = decode_trade(&logs, &[], user).unwrap();
        assert_eq!(result, swap(token_a, 1000, token_b, 2000));
    }

    #[test]
    fn residue_needs_third_party_flow() {
        // A small extra token received straight from the pool never routed third-party, so it
        // cannot be proven residue — the trade stays declined.
        let user = addr(1);
        let pool = addr(50);
        let token_a = addr(10);
        let token_b = addr(11);
        let token_c = addr(12);

        let logs = vec![
            make_transfer_log(token_a, user, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, user, U256::from(2000)),
            make_transfer_log(token_c, pool, user, U256::from(5)),
        ];

        assert!(decode_trade(&logs, &[], user).is_none());
    }

    #[test]
    fn residue_needs_small_share_of_gross() {
        // An extra leg that routed third-party but is a large share of its token's gross flow is
        // a real leg, not residue — the trade stays declined.
        let user = addr(1);
        let pool = addr(50);
        let router = addr(2);
        let token_a = addr(10);
        let token_mid = addr(12);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, user, pool, U256::from(1000)),
            make_transfer_log(token_mid, pool, router, U256::from(1000)),
            make_transfer_log(token_mid, router, user, U256::from(600)),
            make_transfer_log(token_b, router, user, U256::from(2000)),
        ];

        assert!(decode_trade(&logs, &[], user).is_none());
    }

    #[test]
    fn wrap_pair_mispaired_flags_dust_refund() {
        // Relay cross-chain deposit shape (tx 0xc9de04eb…): 0.02 WETH in, a billionth of it
        // refunded back as native ETH — not an unwrap.
        let weth = addr(20);
        let deposit = swap(weth, 20_129_551_554_664_188, Address::ZERO, 1_554_664_188);
        assert!(wrap_pair_mispaired(&deposit, weth));

        let reversed = swap(Address::ZERO, 1_000_000, weth, 100);
        assert!(wrap_pair_mispaired(&reversed, weth));
    }

    #[test]
    fn wrap_pair_near_parity_kept() {
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 1000), weth));
        // A fee-skimmed unwrap stays within the 2x band.
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 900), weth));
    }

    #[test]
    fn non_wrap_pair_never_flagged() {
        // Ordinary token pairs legitimately trade at any rate (decimals differ), and a
        // token <-> wrapped-native trade without the native side is a real swap too.
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, addr(11), 5), weth));
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, weth, 5), weth));
        assert!(!wrap_pair_mispaired(&swap(Address::ZERO, 1_000_000_000, addr(11), 5), weth));
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
