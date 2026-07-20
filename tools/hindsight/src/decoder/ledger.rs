//! Token-flow accounting for a single transaction.
//!
//! Everything the decoder knows about value movement comes from two sources: the transaction's
//! ERC-20 `Transfer` logs and the native ETH transfers recovered from its call trace (native ETH
//! moves emit no log). A [`TransferLedger`] flattens both into one list of
//! `(token, from, to, value)` entries — native ETH is token [`Address::ZERO`] — and every flow
//! question a decode strategy asks is answered from that list, so all strategies share one model
//! of "what moved".
//!
//! # The netting model
//!
//! [`TransferLedger::net_swap`] recovers the swap one address performed: sum what the address
//! sent and received per token, subtract, and require **exactly one token net-out and one token
//! net-in**. Intermediate hops (multi-hop routes, wrap/unwrap legs) net to zero on their own, so
//! they disappear without being modeled. The strict one-in/one-out rule is what keeps the
//! re-solve comparison honest. A net with more tokens on a side is a batch settlement or a
//! shape this model doesn't cover, and guessing a "dominant" leg there would pair unrelated
//! tokens — so ambiguous nets are declined, with one provable exception (residue legs, see
//! [`TransferLedger::net_swap`]).
//!
//! # Assumptions
//!
//! - **The tracked address both pays and receives.** A swap whose output is delivered to a
//!   different receiver nets as output-less and is declined (a coverage miss, never a wrong
//!   record).
//! - **One swap per address per transaction.** Two swaps by the same address net into one combined
//!   flow; if they share sides they merge, otherwise they decline as multi-token.
//! - **Rebasing/fee-on-transfer tokens** can leave residue that fails the one-in/one-out rule; such
//!   trades decline rather than record skewed amounts.

use std::collections::{BTreeSet, HashMap, HashSet};

use alloy::{
    primitives::{Address, Log as PrimitiveLog, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use tracing::debug;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}

/// Convert an RPC log to a primitive log for event decoding.
pub(crate) fn to_primitive_log(log: &Log) -> PrimitiveLog {
    PrimitiveLog::new_unchecked(log.address(), log.topics().to_vec(), log.data().data.clone())
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

/// Every value movement in one transaction, flattened for flow queries.
///
/// Built once per transaction and shared by all decode strategies (see the module docs for the
/// model and its assumptions).
pub(crate) struct TransferLedger {
    /// `(token, from, to, value)` for every transfer; native ETH is token [`Address::ZERO`].
    transfers: Vec<(Address, Address, Address, U256)>,
}

impl TransferLedger {
    /// Flatten a transaction's ERC-20 `Transfer` logs and trace-recovered native ETH transfers
    /// into one ledger.
    pub(crate) fn from_transaction(
        logs: &[Log],
        native_transfers: &[(Address, Address, U256)],
    ) -> Self {
        let mut transfers: Vec<(Address, Address, Address, U256)> = Vec::new();
        for &(from, to, value) in native_transfers {
            transfers.push((Address::ZERO, from, to, value));
        }
        for log in logs {
            let Ok(transfer) = Transfer::decode_log(&to_primitive_log(log)) else {
                continue;
            };
            transfers.push((log.address(), transfer.from, transfer.to, transfer.value));
        }
        Self { transfers }
    }

    /// Determine what `tracked` sent and received, netting intermediate hops.
    ///
    /// Returns `None` unless exactly one token nets out and exactly one token nets in (see the
    /// module docs for why ambiguous nets decline).
    ///
    /// One exception: a genuine single swap can carry a **residue leg** — an RFQ hop consumes an
    /// exact intermediate amount and the surplus lands on the trader, or rounding leaves dust of
    /// a routing token. An ambiguous side first drops legs that are provably residue: the token
    /// also moved between parties other than the tracked address (a routing intermediate), the
    /// leg is under 1% of the token's gross flow in the transaction, *and* the tracked address's
    /// own flow in the token is one-directional. All three are same-token comparisons, so no
    /// prices are needed, and a real batch leg (all of its token's flow) is never dropped. The
    /// one-directional condition is what keeps the gross-flow test unspoofable: surplus and dust
    /// only ever land on the trader, while a token the trader both sent and received is a real
    /// position change however small its net — and gross flow can be inflated arbitrarily by
    /// unrelated loops of the same token elsewhere in the transaction (seen live: an MEV bundle
    /// whose $10k wash loops made a $51 net leg pass the 1% test, tx `0x280b9939…`).
    /// Residue on a token that never routed third-party (e.g. a rebasing input token) and
    /// fixed-token protocol fees still decline, logged at debug level.
    pub(crate) fn net_swap(&self, tracked: Address) -> Option<NetSwap> {
        let mut sent: HashMap<Address, U256> = HashMap::new();
        let mut received: HashMap<Address, U256> = HashMap::new();
        let mut flows: HashMap<Address, TokenFlow> = HashMap::new();
        for &(token, from, to, value) in &self.transfers {
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

    /// Every address that sent or received value, ordered for deterministic iteration.
    pub(crate) fn participants(&self) -> BTreeSet<Address> {
        let mut participants = BTreeSet::new();
        for &(_, from, to, _) in &self.transfers {
            participants.insert(from);
            participants.insert(to);
        }
        participants
    }

    /// Gross total received per token by any of `recipients` (native ETH keyed by
    /// [`Address::ZERO`]). Used for venue-fee accounting: what reached the fee collectors,
    /// regardless of sender.
    pub(crate) fn received_by(&self, recipients: &HashSet<Address>) -> HashMap<Address, U256> {
        let mut totals: HashMap<Address, U256> = HashMap::new();
        if recipients.is_empty() {
            return totals;
        }
        for &(token, _, to, value) in &self.transfers {
            if recipients.contains(&to) {
                *totals.entry(token).or_default() += value;
            }
        }
        totals
    }

    /// Per-token net outflow of the address group: what the group sent minus what it got back,
    /// where positive. Used to anchor on Relay's fee collectors as a funding source.
    pub(crate) fn group_net_sent(&self, group: &HashSet<Address>) -> HashMap<Address, U256> {
        let (sent, received) = self.group_totals(group);
        net_positive(&sent, &received)
    }

    /// Per-token net inflow of the address group: what the group received minus what it sent,
    /// where positive.
    pub(crate) fn group_net_received(&self, group: &HashSet<Address>) -> HashMap<Address, U256> {
        let (sent, received) = self.group_totals(group);
        net_positive(&received, &sent)
    }

    /// Aggregated receipts of **pure sinks** — addresses that received value but never sent any
    /// in the transaction — as `(recipient, token, total)`. A pool or router always sends
    /// something back, so a pure sink is a delivery endpoint (an order's recipient, a payout).
    pub(crate) fn sink_receipts(&self) -> Vec<(Address, Address, U256)> {
        let mut senders: HashSet<Address> = HashSet::new();
        for &(_, from, _, _) in &self.transfers {
            senders.insert(from);
        }
        let mut received: HashMap<(Address, Address), U256> = HashMap::new();
        for &(token, _, to, value) in &self.transfers {
            if !senders.contains(&to) {
                *received.entry((to, token)).or_default() += value;
            }
        }
        received
            .into_iter()
            .filter(|(_, total)| !total.is_zero())
            .map(|((recipient, token), total)| (recipient, token, total))
            .collect()
    }

    /// Gross sent and received per token, summed across the group.
    fn group_totals(
        &self,
        group: &HashSet<Address>,
    ) -> (HashMap<Address, U256>, HashMap<Address, U256>) {
        let mut sent: HashMap<Address, U256> = HashMap::new();
        let mut received: HashMap<Address, U256> = HashMap::new();
        for &(token, from, to, value) in &self.transfers {
            if group.contains(&from) {
                *sent.entry(token).or_default() += value;
            }
            if group.contains(&to) {
                *received.entry(token).or_default() += value;
            }
        }
        (sent, received)
    }
}

/// Per-token `positive - negative` where positive, over the union of tokens.
fn net_positive(
    positive: &HashMap<Address, U256>,
    negative: &HashMap<Address, U256>,
) -> HashMap<Address, U256> {
    let mut net: HashMap<Address, U256> = HashMap::new();
    for (&token, &amount) in positive {
        let offset = negative
            .get(&token)
            .copied()
            .unwrap_or_default();
        if amount > offset {
            net.insert(token, amount - offset);
        }
    }
    net
}

/// A net leg is residue when its token routed between third parties and the leg is under this
/// fraction of the token's gross transaction flow: `net * RESIDUE_GROSS_RATIO < gross` (1%).
const RESIDUE_GROSS_RATIO: u64 = 100;

/// Net the sent and received balances into a single swap (see [`TransferLedger::net_swap`]).
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

    drop_residue_legs(&mut net_sent, flows, received);
    drop_residue_legs(&mut net_received, flows, sent);

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

/// Drop residue legs from one side of an ambiguous net (see [`TransferLedger::net_swap`]). Only
/// runs when the side has more than one leg — a lone leg is the swap itself, however small.
///
/// `opposite` is the tracked address's gross flow on the other side: a leg only qualifies as
/// residue when the tracked address's flow in that token is one-directional, since surplus and
/// dust land on the trader but never also leave it.
fn drop_residue_legs(
    net: &mut HashMap<Address, U256>,
    flows: &HashMap<Address, TokenFlow>,
    opposite: &HashMap<Address, U256>,
) {
    if net.len() <= 1 {
        return;
    }
    net.retain(|token, amount| {
        let bidirectional = opposite
            .get(token)
            .is_some_and(|flow| !flow.is_zero());
        let Some(flow) = flows.get(token) else {
            return true;
        };
        let residue = !bidirectional &&
            flow.intermediate &&
            amount.saturating_mul(U256::from(RESIDUE_GROSS_RATIO)) < flow.gross;
        !residue
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    fn net_swap(
        logs: &[Log],
        native: &[(Address, Address, U256)],
        tracked: Address,
    ) -> Option<NetSwap> {
        TransferLedger::from_transaction(logs, native).net_swap(tracked)
    }

    #[test]
    fn simple_swap() {
        let sender = addr(1);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, sender, addr(50), U256::from(1000)),
            make_transfer_log(token_b, addr(50), sender, U256::from(2000)),
        ];

        let result = net_swap(&logs, &[], sender).unwrap();
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

        let result = net_swap(&logs, &[], sender).unwrap();
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

        let result = net_swap(&logs, &native, user).unwrap();
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

        let result = net_swap(&logs, &native, user).unwrap();
        assert_eq!(result, swap(Address::ZERO, 1000, token, 2000));
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

        let result = net_swap(&logs, &[], user).unwrap();
        assert_eq!(result, swap(token_a, 1000, token_b, 2000));
    }

    #[test]
    fn residue_needs_one_directional_flow() {
        // Regression: MEV bundle 0x280b9939… (block 25487629). The tracked wallet nets three
        // tokens: WETH out, native ETH in, and a +51 USDC position from sending 510 and
        // receiving 561 — while the bundle's own $10k USDC wash loops inflate the token's gross
        // flow until the net leg passes the 1% test. The USDC flow is bidirectional, so it is a
        // real position change, not residue: the trade must decline as multi-token.
        let wallet = addr(1);
        let bot = addr(2);
        let pool = addr(50);
        let weth = addr(10);
        let usdc = addr(11);

        let logs = vec![
            make_transfer_log(weth, wallet, bot, U256::from(26_000)),
            make_transfer_log(usdc, wallet, bot, U256::from(510)),
            make_transfer_log(usdc, bot, wallet, U256::from(561)),
            // The bot's wash loops: huge same-token third-party flow in the same transaction.
            make_transfer_log(usdc, bot, pool, U256::from(10_000)),
            make_transfer_log(usdc, pool, bot, U256::from(10_000)),
        ];
        let native = [(bot, wallet, U256::from(16_000u64))];

        assert!(net_swap(&logs, &native, wallet).is_none());
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

        assert!(net_swap(&logs, &[], user).is_none());
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

        assert!(net_swap(&logs, &[], user).is_none());
    }

    #[test]
    fn no_sender_flow() {
        let sender = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(50), addr(51), U256::from(1000))];
        assert!(net_swap(&logs, &[], sender).is_none());
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

        assert!(net_swap(&logs, &[], settler).is_none());
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

        assert!(net_swap(&logs, &[], settler).is_none());
    }

    #[test]
    fn participants_cover_both_sides_and_native() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1))];
        let native = vec![(addr(3), addr(4), U256::from(1))];
        let ledger = TransferLedger::from_transaction(&logs, &native);
        let participants = ledger.participants();
        assert_eq!(participants, [addr(1), addr(2), addr(3), addr(4)].into());
    }

    #[test]
    fn received_by_totals_per_token() {
        let collector = addr(99);
        let collectors = HashSet::from([collector]);
        let logs = vec![
            make_transfer_log(addr(10), addr(1), collector, U256::from(40)),
            make_transfer_log(addr(10), addr(2), collector, U256::from(2)),
        ];
        let native = vec![(addr(1), collector, U256::from(7))];
        let ledger = TransferLedger::from_transaction(&logs, &native);
        let totals = ledger.received_by(&collectors);
        assert_eq!(totals.get(&addr(10)).copied(), Some(U256::from(42)));
        assert_eq!(totals.get(&Address::ZERO).copied(), Some(U256::from(7)));
        assert!(ledger
            .received_by(&HashSet::new())
            .is_empty());
    }

    #[test]
    fn group_nets_offset_round_trips() {
        // The group sends 1000 of token A and gets 300 back: net sent 700, no net receipt.
        let group = HashSet::from([addr(99)]);
        let logs = vec![
            make_transfer_log(addr(10), addr(99), addr(50), U256::from(1000)),
            make_transfer_log(addr(10), addr(50), addr(99), U256::from(300)),
            make_transfer_log(addr(11), addr(50), addr(99), U256::from(200)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        assert_eq!(ledger.group_net_sent(&group), HashMap::from([(addr(10), U256::from(700))]));
        assert_eq!(ledger.group_net_received(&group), HashMap::from([(addr(11), U256::from(200))]));
    }

    #[test]
    fn sink_receipts_exclude_anyone_who_sent() {
        // The pool receives and sends (a conversion), the recipient only receives (a sink).
        let logs = vec![
            make_transfer_log(addr(10), addr(1), addr(50), U256::from(1000)),
            make_transfer_log(addr(11), addr(50), addr(7), U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        assert_eq!(ledger.sink_receipts(), vec![(addr(7), addr(11), U256::from(2000))]);
    }
}
