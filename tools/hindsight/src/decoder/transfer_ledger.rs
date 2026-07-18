//! Token-flow accounting for a single transaction.
//!
//! Everything the decoder knows about value movement comes from two sources: the transaction's
//! ERC-20 `Transfer` logs and the native ETH transfers recovered from its call trace (native ETH
//! moves emit no log). A [`TransferLedger`] flattens both into one list of
//! `(token, from, to, value)` entries — native ETH is token [`Address::ZERO`] — and every flow
//! question a decoder asks is answered from that list, so all decoders share one model
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

/// One token's totals from a single pass over a transaction's transfers, seen from one
/// tracked address. `gross` and `routed_third_party` describe the whole transaction's flow of
/// the token — facts the tracked address's own totals cannot tell, which the residue rules in
/// [`TransferLedger::net_swap`] need.
#[derive(Default)]
struct TokenAmounts {
    /// Gross amount the tracked address sent; nets against `received`.
    sent: U256,
    /// Gross amount the tracked address received; nets against `sent`.
    received: U256,
    /// Total amount of the token moved in the transaction, by any party.
    gross: U256,
    /// Whether the token also moved between two parties that are both not the tracked address.
    routed_third_party: bool,
}

/// Every value movement in one transaction, flattened for flow queries.
///
/// Built once per transaction and shared by all decoders (see the module docs for the
/// model and its assumptions).
pub(crate) struct TransferLedger {
    /// `(token, from, to, value)` for every transfer; native ETH is token [`Address::ZERO`].
    transfers: Vec<(Address, Address, Address, U256)>,
}

impl TransferLedger {
    /// Flatten a transaction's ERC-20 `Transfer` logs and trace-recovered native ETH transfers
    /// into one transfer ledger.
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

    /// Net what `tracked` sent and received per token, cancelling intermediate hops.
    ///
    /// `Some` only when exactly one token nets out and exactly one nets in; ambiguous nets
    /// decline (see the module docs).
    ///
    /// # The residue-leg exception
    ///
    /// A leg is one token's net amount on one side of the trade. Routing can leave the trader a
    /// small extra leg — an RFQ hop's surplus, rounding dust of an intermediate token — and
    /// declining every such trade would throw away real ones. An ambiguous side therefore first
    /// drops legs that are *provably* residue, meaning all three of:
    ///
    /// - the token also moved between third parties (a routing token, not one only paid to or by
    ///   the trader),
    /// - the leg is under 1% of that token's total flow in the transaction,
    /// - the trader's own flow in the token is one-directional.
    ///
    /// All three compare same-token amounts, so the proof needs no prices. Each condition
    /// closed a real mis-decode (the last: a wash-loop MEV bundle, tx `0x280b9939…`, whose $10k
    /// loops let a $51 position change pass the 1% test).
    pub(crate) fn net_swap(&self, tracked: Address) -> Option<NetSwap> {
        let mut amounts_by_token: HashMap<Address, TokenAmounts> = HashMap::new();
        for &(token, from, to, value) in &self.transfers {
            let amounts = amounts_by_token
                .entry(token)
                .or_default();
            amounts.gross = amounts.gross.saturating_add(value);
            if from != tracked && to != tracked {
                amounts.routed_third_party = true;
            }
            if from == tracked {
                amounts.sent += value;
            }
            if to == tracked {
                amounts.received += value;
            }
        }

        net_trade(&amounts_by_token)
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

    /// Gross total received per token by any of `recipients`, regardless of sender (native ETH
    /// keyed by [`Address::ZERO`]).
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
    /// where positive.
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

/// Net the per-token amounts into a single swap (see [`TransferLedger::net_swap`]).
fn net_trade(amounts_by_token: &HashMap<Address, TokenAmounts>) -> Option<NetSwap> {
    let mut net_sent: HashMap<Address, U256> = HashMap::new();
    let mut net_received: HashMap<Address, U256> = HashMap::new();

    for (&token, amounts) in amounts_by_token {
        if amounts.sent > amounts.received {
            net_sent.insert(token, amounts.sent - amounts.received);
        } else if amounts.received > amounts.sent {
            net_received.insert(token, amounts.received - amounts.sent);
        }
    }

    // The one-directional residue condition looks at the tracked address's gross flow on the
    // side opposite the leg: for a net-sent leg that is what it received, and vice versa.
    drop_residue_legs(&mut net_sent, amounts_by_token, |amounts| amounts.received);
    drop_residue_legs(&mut net_received, amounts_by_token, |amounts| amounts.sent);

    if net_sent.len() != 1 || net_received.len() != 1 {
        // TraderFlow on both sides but more than one significant token on one of them: a real batch
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

/// Drop residue legs from one side of an ambiguous net, per the three-condition proof in
/// [`TransferLedger::net_swap`]. Only runs when the side has more than one leg — a lone leg is
/// the swap itself, however small.
///
/// `opposite_flow` reads the tracked address's gross flow on the other side of the trade from a
/// token's amounts; any flow there makes the token bidirectional and therefore never residue.
fn drop_residue_legs(
    net: &mut HashMap<Address, U256>,
    amounts_by_token: &HashMap<Address, TokenAmounts>,
    opposite_flow: fn(&TokenAmounts) -> U256,
) {
    if net.len() <= 1 {
        return;
    }
    net.retain(|token, amount| {
        let Some(amounts) = amounts_by_token.get(token) else {
            return true;
        };
        let bidirectional = !opposite_flow(amounts).is_zero();
        let residue = !bidirectional &&
            amounts.routed_third_party &&
            amount.saturating_mul(U256::from(RESIDUE_GROSS_RATIO)) < amounts.gross;
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
    fn multi_hop_swap() {
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
    fn rfq_surplus_residue() {
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
    fn residue_two_directional_flow() {
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
    fn residue_without_third_party_flow() {
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
    fn residue_large_share_of_gross() {
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
    fn multi_token_batch_settlement() {
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
    fn one_in_many_out() {
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
    fn participants_both_sides_and_native() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1))];
        let native = vec![(addr(3), addr(4), U256::from(1))];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &native);
        let participants = transfer_ledger.participants();
        assert_eq!(participants, [addr(1), addr(2), addr(3), addr(4)].into());
    }

    #[test]
    fn received_by_multiple_recipients() {
        let collector = addr(99);
        let collectors = HashSet::from([collector]);
        let logs = vec![
            make_transfer_log(addr(10), addr(1), collector, U256::from(40)),
            make_transfer_log(addr(10), addr(2), collector, U256::from(2)),
        ];
        let native = vec![(addr(1), collector, U256::from(7))];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &native);
        let totals = transfer_ledger.received_by(&collectors);
        assert_eq!(totals.get(&addr(10)).copied(), Some(U256::from(42)));
        assert_eq!(totals.get(&Address::ZERO).copied(), Some(U256::from(7)));
        assert!(transfer_ledger
            .received_by(&HashSet::new())
            .is_empty());
    }

    #[test]
    fn group_round_trips() {
        // The group sends 1000 of token A and gets 300 back: net sent 700, no net receipt.
        let group = HashSet::from([addr(99)]);
        let logs = vec![
            make_transfer_log(addr(10), addr(99), addr(50), U256::from(1000)),
            make_transfer_log(addr(10), addr(50), addr(99), U256::from(300)),
            make_transfer_log(addr(11), addr(50), addr(99), U256::from(200)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        assert_eq!(
            transfer_ledger.group_net_sent(&group),
            HashMap::from([(addr(10), U256::from(700))])
        );
        assert_eq!(
            transfer_ledger.group_net_received(&group),
            HashMap::from([(addr(11), U256::from(200))])
        );
    }

    #[test]
    fn sink_receipts_when_recipient_also_sent() {
        // The pool receives and sends (a conversion), the recipient only receives (a sink).
        let logs = vec![
            make_transfer_log(addr(10), addr(1), addr(50), U256::from(1000)),
            make_transfer_log(addr(11), addr(50), addr(7), U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        assert_eq!(transfer_ledger.sink_receipts(), vec![(addr(7), addr(11), U256::from(2000))]);
    }
}
