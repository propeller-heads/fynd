//! Generic intent decoding: the fallback for the Intent role.
//!
//! Covers transactions where the sender is not the trader — solver-initiated intent fills
//! (`UniswapX`, 1inch limit orders) and batch settlements — by finding the order swapper's net
//! flow. `IntentNetting` is the decoder; `find_intent_trade` does the finding. A source with a
//! richer signal (see `super::cow`) is tried ahead of this.
//!
//! The one fact this needs beyond the transfer ledger — whether a candidate address is a
//! contract or an EOA — is prefetched by the decoder driver before decode runs (see
//! `decoder::Decoder::prefetch_contract_flags`), so `find_intent_trade` issues no RPC of its own;
//! `contract_flags` is a plain lookup into facts already gathered.

use std::collections::HashMap;

use alloy::primitives::Address;
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    registry::Registry,
    transfer_ledger::{NetSwap, TransferLedger},
};

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
            ctx.transfer_ledger,
            &[ctx.entry_point, ctx.receipt.from],
            ctx.registry,
            ctx.contract_flags,
        )
    }
}

/// Find the order swapper's trade in a solver-initiated intent fill.
///
/// The transaction sender is the solver, not the swapper, so we look for the
/// externally-owned account whose net flow is a clean two-token swap. Contracts
/// never qualify: pools and routers net the inverse swap or leftover dust, and
/// recording an intermediary's dust as the trade produces absurd "swaps" (seen
/// live: WETH → 2.4e-7 AAVE). Known registry contracts and the excluded
/// addresses (solver, entry point) are skipped too. A fill with no clean-net EOA
/// is declined rather than guessed.
///
/// Whether a candidate is a contract comes from `contract_flags`, prefetched by the decoder
/// driver for exactly this candidate set (see `intent_candidates`) before decode ran. A candidate
/// absent from the map is treated as a contract — the same conservative default the old
/// RPC-failure path used, so a prefetch gap declines rather than guesses.
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
pub(crate) fn find_intent_trade(
    transfer_ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
    contract_flags: &HashMap<Address, bool>,
) -> Option<TraderFlow> {
    for (candidate, trade) in intent_candidates(transfer_ledger, exclude, registry) {
        let is_contract = contract_flags
            .get(&candidate)
            .copied()
            .unwrap_or(true);
        if !is_contract {
            return Some(TraderFlow::without_fees(candidate, trade));
        }
    }
    None
}

/// Addresses with a clean two-token net swap, excluding the zero address, the
/// excluded addresses, and known registry contracts. Ordered by address for
/// deterministic selection.
///
/// Also the candidate set the decoder driver's contract-flag prefetch enumerates ahead of decode
/// — the two call sites must agree on `exclude` for the prefetch to cover what `find_intent_trade`
/// then looks up.
pub(crate) fn intent_candidates(
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

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    /// The swapper/pool inverse-swap fixture: swapper sells `token_a` for `token_b`, pool nets the
    /// inverse.
    fn inverse_swap_ledger() -> TransferLedger {
        let logs = vec![
            make_transfer_log(addr(10), addr(100), addr(101), U256::from(1000)),
            make_transfer_log(addr(11), addr(101), addr(100), U256::from(2000)),
        ];
        TransferLedger::from_transaction(&logs, &[])
    }

    #[test]
    fn test_find_intent_trade_eoa_candidate() {
        // Candidates in address order: addr(100) first — an EOA.
        let registry = Registry::ethereum();
        let contract_flags = HashMap::from([(addr(100), false)]);
        let flow =
            find_intent_trade(&inverse_swap_ledger(), &[], &registry, &contract_flags).unwrap();
        assert_eq!(flow.tracked, addr(100));
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
    }

    #[test]
    fn test_find_intent_trade_all_candidates_contracts() {
        // Both candidates carry code: a routing intermediary and a pool. Guessing one would net
        // residue dust as an absurd swap, so the fill must be declined.
        let registry = Registry::ethereum();
        let contract_flags = HashMap::from([(addr(100), true), (addr(101), true)]);
        let flow = find_intent_trade(&inverse_swap_ledger(), &[], &registry, &contract_flags);
        assert!(flow.is_none());
    }

    #[test]
    fn test_find_intent_trade_unknown_candidate_declines() {
        // A candidate absent from the prefetched facts (a prefetch gap, not expected in
        // practice) is treated as a contract rather than guessed as an EOA.
        let registry = Registry::ethereum();
        let flow = find_intent_trade(&inverse_swap_ledger(), &[], &registry, &HashMap::new());
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
