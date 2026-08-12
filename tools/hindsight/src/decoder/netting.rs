//! Transfer-netting: recover a swap from what actually moved. The one netting module.
//!
//! The evidence is the ERC-20 `Transfer` events plus the native transfers recovered from the
//! trace (see `transfer_ledger`) — what actually moved, not what any contract or calldata
//! declared. It needs no knowledge of any router's format.
//!
//! This module is the engine plus the two generic netting decoders. The engine — `sender_flow`,
//! `venue_flow`, and `find_intent_trade` — is what every netting decoder builds on. The decoders
//! are `SenderNetting` (direct solver swaps: the sender is the trader) and `IntentNetting` (the
//! Intent-role fallback: the sender acts for the swapper, so the swapper's net flow is found
//! instead). Venue netting decoders live in `super::venues` and call the engine with their own
//! corrections.
//!
//! Netting requires the trader to both pay and receive. When the swap's output is delivered to a
//! different receiver, nothing nets against the trader's input and the transaction is declined —
//! a coverage miss, never wrong amounts (see `transfer_ledger` for the model's assumptions).

use std::collections::{HashMap, HashSet};

use alloy::{primitives::Address, providers::Provider};
use async_trait::async_trait;
use tracing::warn;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    registry::Registry,
    transfer_ledger::{NetSwap, TransferLedger},
};

/// Net the sender's flow. When the sender nets nothing, fall back to the contract the transaction
/// entered through (`tx.to`), for the rare shape where the swap output is delivered to that
/// contract rather than back to the sender.
pub(crate) fn sender_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<TraderFlow> {
    transfer_ledger
        .net_swap(sender)
        .map(|swap| TraderFlow::without_fees(sender, swap))
        .or_else(|| {
            transfer_ledger
                .net_swap(entry_point)
                .map(|swap| TraderFlow::without_fees(entry_point, swap))
        })
}

/// Net the sender's flow and back the venue's fee out of it — the shared shape of every
/// fee-taking venue entry. Venue decoders call this, then add what is specific to them.
///
/// One exception to the fee back-out: when the tracked trader IS a fee collector, the transaction
/// is a treasury operation — the collector's receipts are its own output, not a fee, and backing
/// them "out" would add the output to itself and double it.
pub(crate) fn venue_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
    fee_collectors: &HashSet<Address>,
) -> Option<TraderFlow> {
    let flow = sender_flow(transfer_ledger, sender, entry_point)?;
    if fee_collectors.contains(&flow.tracked) {
        return Some(flow);
    }
    Some(back_out_venue_fees(flow, transfer_ledger, fee_collectors))
}

/// Back a venue fee out of a decoded user flow.
///
/// The venue can take its fee on either side. An input-side fee is subtracted from `amount_in`
/// (the user's gross spend included money that never entered the swap) and an output-side fee is
/// added back into `amount_out` (the swap produced more than the user kept), so both sides are the
/// amounts actually swapped — the like-for-like basis vs Fynd.
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
    }
}

/// Direct solver swaps: the sender is the trader, so net the sender's flow.
pub(crate) struct SenderNetting;

#[async_trait]
impl TradeDecoder for SenderNetting {
    fn name(&self) -> &'static str {
        "sender-netting"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        sender_flow(ctx.transfer_ledger, ctx.receipt.from, ctx.entry_point)
    }
}

/// Solver-initiated intent fills and batch settlements: the sender acts on the swapper's behalf, so
/// the real swap is the swapper's net flow.
pub(crate) struct IntentNetting;

#[async_trait]
impl TradeDecoder for IntentNetting {
    fn name(&self) -> &'static str {
        "intent-netting"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        find_intent_trade(
            ctx.provider,
            ctx.transfer_ledger,
            &[ctx.entry_point, ctx.receipt.from],
            ctx.registry,
            ctx.code_cache,
        )
        .await
    }
}

/// Find the order swapper's trade in a solver-initiated intent fill.
///
/// The transaction sender is the solver, not the swapper, so we look for the
/// externally-owned account whose net flow is a clean two-token swap. Contracts
/// never qualify (checked via `eth_getCode`): pools and routers net the inverse
/// swap or leftover dust, and recording an intermediary's dust as the trade
/// produces absurd "swaps" (seen live: WETH → 2.4e-7 AAVE). Known registry
/// contracts and the excluded addresses (solver, entry point) are skipped too.
/// A fill with no clean-net EOA is declined rather than guessed.
///
/// v0 limitations (tracked for a decode/attribution rework):
/// - **One swapper per transaction.** The first clean-net EOA wins, so a batch that settles several
///   retail orders in one tx contributes a single decoded trade; the rest surface as "Allium only"
///   gaps and batch volume is under-counted.
/// - **No settlement-tied tiebreak.** When several non-excluded EOAs each net to a clean two-token
///   swap, the winner is just the first in `intent_candidates`' address-ordered iteration, so a
///   decode can attribute the wrong account's flow.
/// - **Smart-wallet swappers are declined.** A swapper behind contract code (account abstraction,
///   EIP-7702 delegation) is indistinguishable from a pool here, so its fills are dropped.
pub(crate) async fn find_intent_trade<P: Provider>(
    provider: &P,
    transfer_ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
    code_cache: &mut HashMap<Address, bool>,
) -> Option<TraderFlow> {
    for (candidate, trade) in intent_candidates(transfer_ledger, exclude, registry) {
        if !is_contract(provider, candidate, code_cache).await {
            return Some(TraderFlow::without_fees(candidate, trade));
        }
    }
    None
}

/// Addresses with a clean two-token net swap, excluding the zero address, the
/// excluded addresses, and known registry contracts. Ordered by address for
/// deterministic selection.
fn intent_candidates(
    transfer_ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
) -> Vec<(Address, NetSwap)> {
    let mut candidates = transfer_ledger.participants();
    candidates.remove(&Address::ZERO);
    candidates.retain(|address| !exclude.contains(address) && !registry.is_known(*address));

    let mut swaps = Vec::new();
    for candidate in candidates {
        if let Some(trade) = transfer_ledger.net_swap(candidate) {
            swaps.push((candidate, trade));
        }
    }
    swaps
}

/// Whether an address has contract code, cached across blocks. On RPC failure
/// the address is treated as a contract so it is not mistaken for an EOA
/// swapper.
///
/// v0 limitation: an EIP-7702-delegated account carries code, so a 7702 swapper EOA is classified
/// as a contract and dropped. 7702 is not yet widely used, so this is accepted for now.
async fn is_contract<P: Provider>(
    provider: &P,
    address: Address,
    cache: &mut HashMap<Address, bool>,
) -> bool {
    if let Some(is_contract) = cache.get(&address) {
        return *is_contract;
    }
    let is_contract = match provider.get_code_at(address).await {
        Ok(code) => !code.is_empty(),
        Err(error) => {
            warn!(%address, %error, "failed to fetch code; treating as contract");
            true
        }
    };
    cache.insert(address, is_contract);
    is_contract
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{Bytes, U256},
        providers::RootProvider,
        rpc::client::RpcClient,
        transports::mock::Asserter,
    };

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    fn mocked_provider(asserter: &Asserter) -> RootProvider {
        RootProvider::new(RpcClient::mocked(asserter.clone()))
    }

    /// The swapper/pool inverse-swap fixture: swapper sells `token_a` for `token_b`, pool nets the
    /// inverse.
    fn inverse_swap_ledger() -> TransferLedger {
        let logs = vec![
            make_transfer_log(addr(10), addr(100), addr(101), U256::from(1000)),
            make_transfer_log(addr(11), addr(101), addr(100), U256::from(2000)),
        ];
        TransferLedger::from_transaction(&logs, &[])
    }

    #[tokio::test]
    async fn test_find_intent_trade_eoa_candidate() {
        let asserter = Asserter::new();
        // Candidates in address order: addr(100) first — an EOA (empty code).
        asserter.push_success(&Bytes::default());
        let provider = mocked_provider(&asserter);

        let registry = Registry::ethereum();
        let mut cache = HashMap::new();
        let flow = find_intent_trade(&provider, &inverse_swap_ledger(), &[], &registry, &mut cache)
            .await
            .unwrap();
        assert_eq!(flow.tracked, addr(100));
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
    }

    #[tokio::test]
    async fn test_find_intent_trade_all_candidates_contracts() {
        let asserter = Asserter::new();
        // Both candidates carry code: a routing intermediary and a pool. Guessing one would net
        // residue dust as an absurd swap, so the fill must be declined.
        asserter.push_success(&Bytes::from(vec![0xfe]));
        asserter.push_success(&Bytes::from(vec![0xfe]));
        let provider = mocked_provider(&asserter);

        let registry = Registry::ethereum();
        let mut cache = HashMap::new();
        let flow =
            find_intent_trade(&provider, &inverse_swap_ledger(), &[], &registry, &mut cache).await;
        assert!(flow.is_none());
    }

    #[test]
    fn test_intent_candidates_swap_sides() {
        // Intent fill: the swapper sells token_a for token_b; the pool is the
        // counterparty. The solver is excluded.
        let registry = Registry::ethereum();
        let swapper = addr(100);
        let pool = addr(101);
        let solver = addr(102);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, swapper, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, swapper, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let found: HashMap<Address, _> = intent_candidates(&transfer_ledger, &[solver], &registry)
            .into_iter()
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[&swapper], swap(token_a, 1000, token_b, 2000));
        // The pool nets the inverse swap; the EOA filter discards it later.
        assert_eq!(found[&pool], swap(token_b, 2000, token_a, 1000));
    }

    #[test]
    fn test_intent_candidates_excluded_and_known() {
        let registry = Registry::ethereum();
        let swapper = addr(100);
        let pool = addr(101);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, swapper, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, swapper, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        // Excluding the swapper leaves only the pool.
        let candidates = intent_candidates(&transfer_ledger, &[swapper], &registry);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, pool);
    }
}
