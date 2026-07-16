//! Venue-specific knowledge: the platforms users enter through (Relay, `MetaMask`).
//!
//! A venue owns the order flow — it picks a solver and may take a fee. A module here is one
//! venue, and it is the only place for everything hindsight knows about that venue, whichever
//! decode method consumes it. That knowledge comes in layers:
//!
//! - **Address facts** — entry points, fee collectors — are pure data and live in the address
//!   book's `[venues.<name>]` section, bound to the venue's module by [`from_name`].
//! - **Transfer-based knowledge** interprets the venue's value movements: backing the fee out of a
//!   netted flow ([`venue_fee_flow`]), Relay's solver-rebalance fills.
//! - **Calldata-based knowledge** interprets the venue's own contract input: `MetaMask`'s router
//!   ABI and the solver id it declares.
//!
//! Decode strategies (see `strategies`) call into the layer relevant to their method. Adding
//! venue knowledge therefore means extending that venue's module here — never a strategy, and
//! never a third place.
//!
//! # What happens when venue knowledge is missing
//!
//! Missing knowledge does not stop decoding — it degrades it, silently:
//!
//! - **Venue not in the address book at all**: its transactions only match when a known solver
//!   emitted a log inside them, and those decode via maker-finding, which excludes the sender — so
//!   most of the venue's trades are missed or declined. They surface as coverage gaps in `verify`,
//!   not as wrong records.
//! - **Venue registered but a fee collector is missing (or some behavior of the venue is not
//!   modeled)**: trades decode, but wrongly. The fee is not backed out, so the recovered amounts
//!   include the venue's fee — and every comparison then credits Fynd with money better routing
//!   cannot recover, inflating wins on exactly the trades the venue cares about.
//!
//! The second failure mode is why fee collectors are verified against on-chain samples (see the
//! address book's comments) before a venue is added.

pub(crate) mod metamask;
pub(crate) mod relay;

use std::collections::HashSet;

use alloy::primitives::Address;

use crate::decoder::{
    registry::{Registry, VenueAddresses},
    strategies::{netting::sender_flow, DecodeContext, GasScope, TraderFlow},
    transfer_ledger::{NetSwap, TransferLedger},
};

/// A venue's decode behavior: one implementation per module in this directory, bound to its
/// address-book section by [`from_name`].
///
/// A venue's capabilities are not fixed — some take fees, some declare their solver in
/// calldata, some submit rebalancing fills. Future capabilities (e.g. calldata-based knowledge
/// for a calldata strategy) become methods with default implementations, so a venue only
/// implements the layers it has.
pub(crate) trait VenueKnowledge: Send + Sync {
    /// Decode a transaction entered through this venue's contract.
    fn decode(&self, ctx: &VenueContext<'_>) -> Option<TraderFlow>;
}

/// The venue implementation bound to a name from the address book.
///
/// A `[venues.<name>]` section in the address book only carries addresses; this is where its
/// name gets behavior. The registry validates every configured venue name against this binding
/// at load time, so a typo'd section fails fast instead of silently never matching.
pub(crate) fn from_name(name: &str) -> Option<&'static dyn VenueKnowledge> {
    match name {
        "relay" => Some(&relay::Relay),
        "metamask" => Some(&metamask::Metamask),
        _ => None,
    }
}

/// Everything a venue decoder may read from one matched transaction, so every venue is called
/// through the same seam regardless of which inputs it uses — a venue that starts needing
/// another input extends this struct instead of every venue's signature.
pub(crate) struct VenueContext<'a> {
    /// The venue's own address-book section (entry points, fee collectors, solver aliases),
    /// resolved by the caller so implementations never look themselves up by name.
    pub addresses: &'a VenueAddresses,
    /// The transaction's flattened value movements.
    pub transfer_ledger: &'a TransferLedger,
    pub sender: Address,
    pub entry_point: Address,
    /// The transaction's root calldata, where a venue may declare its solver
    /// (`MetaMask`'s `aggregatorId`).
    pub input: &'a [u8],
    pub registry: &'a Registry,
}

impl<'a> VenueContext<'a> {
    /// The venue-relevant slice of a decode context, plus the venue's address-book section.
    pub(crate) fn new<P>(ctx: &'a DecodeContext<'_, P>, addresses: &'a VenueAddresses) -> Self {
        Self {
            addresses,
            transfer_ledger: ctx.transfer_ledger,
            sender: ctx.receipt.from,
            entry_point: ctx.entry_point,
            input: ctx.input,
            registry: ctx.registry,
        }
    }
}

/// Net the sender's flow and back the venue's fee out of it — the shared shape of every
/// fee-taking venue entry (Relay, `MetaMask`).
///
/// A trader-paid flow's gas scope narrows to the solver call's trace frame: inside a venue's
/// contract the receipt's gas includes the venue's own overhead, which is charged whichever
/// solver the venue picks and must stay out of the comparison.
///
/// One exception to the fee back-out: when the tracked trader IS a fee collector, the
/// transaction is a treasury operation — the collector's receipts are its own output, not a
/// fee, and backing them "out" would add the output to itself and double it.
pub(crate) fn venue_fee_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
    fee_collectors: &HashSet<Address>,
) -> Option<TraderFlow> {
    let mut flow = sender_flow(transfer_ledger, sender, entry_point)?;
    if flow.gas_scope == GasScope::WholeTransaction {
        flow.gas_scope = GasScope::SolverFrame;
    }
    if fee_collectors.contains(&flow.tracked) {
        return Some(flow);
    }
    Some(back_out_venue_fees(flow, transfer_ledger, fee_collectors))
}

/// Back a venue fee out of a decoded user flow.
///
/// The venue can take its fee on either side. An input-side fee is subtracted
/// from `amount_in` (the user's gross spend included money that never entered
/// the swap) and an output-side fee is added back into `amount_out` (the
/// swap produced more than the user kept), so both sides are the amounts
/// actually swapped — the like-for-like basis vs Fynd.
fn back_out_venue_fees(
    flow: TraderFlow,
    transfer_ledger: &TransferLedger,
    fee_collectors: &HashSet<Address>,
) -> TraderFlow {
    let fees = transfer_ledger.received_by(fee_collectors);
    let venue_fee_in = fees
        .get(&flow.swap.token_in)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_in =
        venue_fee_in.map_or(flow.swap.amount_in, |fee| flow.swap.amount_in.saturating_sub(fee));
    let venue_fee_out = fees
        .get(&flow.swap.token_out)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_out =
        venue_fee_out.map_or(flow.swap.amount_out, |fee| flow.swap.amount_out.saturating_add(fee));
    TraderFlow {
        tracked: flow.tracked,
        swap: NetSwap { amount_in, amount_out, ..flow.swap },
        venue_fee_in,
        venue_fee_out,
        solver_override: flow.solver_override,
        gas_scope: flow.gas_scope,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    #[test]
    fn venue_fee_flow_input_fee() {
        // The router sends part of the input token to the collector; the rest goes to the pool.
        let user = addr(1);
        let router = addr(2);
        let collector = addr(99);
        let token_in = addr(10);
        let token_out = addr(11);
        let pool = addr(50);
        let collectors = HashSet::from([collector]);

        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = venue_fee_flow(&transfer_ledger, user, router, &collectors).unwrap();
        assert_eq!(flow.swap, swap(token_in, 960, token_out, 2000));
        assert_eq!(flow.venue_fee_in, Some(U256::from(40)));
        assert_eq!(flow.venue_fee_out, None);
    }

    #[test]
    fn venue_fee_flow_fee_free_trade() {
        // Nothing reached a fee wallet, nothing is backed out.
        let user = addr(1);
        let pool = addr(50);
        let collectors = HashSet::from([addr(99)]);

        let logs = vec![
            make_transfer_log(addr(10), user, pool, U256::from(1000)),
            make_transfer_log(addr(11), pool, user, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = venue_fee_flow(&transfer_ledger, user, pool, &collectors).unwrap();
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
    }
}
