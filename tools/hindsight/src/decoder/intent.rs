//! Maker-finding for intent fills and batch settlements.
//!
//! Covers transactions where the sender is not the trader: filler-initiated
//! intent fills (`UniswapX`, 1inch limit orders) and batch settlements (`CoW`),
//! where the real swap is an order maker's net flow.

use std::collections::HashMap;

use alloy::{primitives::Address, providers::Provider};
use tracing::warn;

use crate::decoder::{
    ledger::{NetSwap, TransferLedger},
    registry::Registry,
    strategies::Flow,
};

/// Find the order maker's trade in a filler-initiated intent fill.
///
/// The transaction sender is the filler, not the swapper, so we look for the
/// externally-owned account whose net flow is a clean two-token swap. Contracts
/// never qualify (checked via `eth_getCode`): pools and routers net the inverse
/// swap or leftover dust, and recording an intermediary's dust as the trade
/// produces absurd "swaps" (seen live: WETH → 2.4e-7 AAVE). Known registry
/// contracts and the excluded addresses (filler, entry point) are skipped too.
/// A fill with no clean-net EOA is declined rather than guessed.
///
/// v0 limitations (tracked for a decode/attribution rework):
/// - **One maker per transaction.** The first clean-net EOA wins, so a batch that settles several
///   retail orders in one tx contributes a single decoded trade; the rest surface as "Allium only"
///   gaps and batch volume is under-counted.
/// - **No settlement-tied tiebreak.** When several non-excluded EOAs each net to a clean two-token
///   swap, the winner is just the first in `maker_candidates`' address-ordered iteration, so a
///   decode can attribute the wrong account's flow.
/// - **Smart-wallet makers are declined.** A maker behind contract code (account abstraction,
///   EIP-7702 delegation) is indistinguishable from a pool here, so its fills are dropped.
pub(crate) async fn find_maker_trade<P: Provider>(
    provider: &P,
    ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
    code_cache: &mut HashMap<Address, bool>,
) -> Option<Flow> {
    for (candidate, trade) in maker_candidates(ledger, exclude, registry) {
        if !is_contract(provider, candidate, code_cache).await {
            return Some(Flow::without_fees(candidate, trade));
        }
    }
    None
}

/// Addresses with a clean two-token net swap, excluding the zero address, the
/// excluded addresses, and known registry contracts. Ordered by address for
/// deterministic selection.
fn maker_candidates(
    ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
) -> Vec<(Address, NetSwap)> {
    let mut candidates = ledger.participants();
    candidates.remove(&Address::ZERO);
    candidates.retain(|address| !exclude.contains(address) && !registry.is_known(*address));

    let mut swaps = Vec::new();
    for candidate in candidates {
        if let Some(trade) = ledger.net_swap(candidate) {
            swaps.push((candidate, trade));
        }
    }
    swaps
}

/// Whether an address has contract code, cached across blocks. On RPC failure
/// the address is treated as a contract so it is not mistaken for an EOA
/// maker.
///
/// v0 limitation: an EIP-7702-delegated account carries code, so a 7702 maker EOA is classified as
/// a contract and dropped. 7702 is not yet widely used, so this is accepted for now.
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

    /// The maker/pool inverse-swap fixture: maker sells `token_a` for `token_b`, pool nets the
    /// inverse.
    fn inverse_swap_ledger() -> TransferLedger {
        let logs = vec![
            make_transfer_log(addr(10), addr(100), addr(101), U256::from(1000)),
            make_transfer_log(addr(11), addr(101), addr(100), U256::from(2000)),
        ];
        TransferLedger::from_transaction(&logs, &[])
    }

    #[tokio::test]
    async fn find_maker_trade_picks_eoa_maker() {
        let asserter = Asserter::new();
        // Candidates in address order: addr(100) first — an EOA (empty code).
        asserter.push_success(&Bytes::default());
        let provider = mocked_provider(&asserter);

        let registry = Registry::ethereum();
        let mut cache = HashMap::new();
        let flow = find_maker_trade(&provider, &inverse_swap_ledger(), &[], &registry, &mut cache)
            .await
            .unwrap();
        assert_eq!(flow.tracked, addr(100));
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
    }

    #[tokio::test]
    async fn find_maker_trade_declines_when_all_candidates_are_contracts() {
        let asserter = Asserter::new();
        // Both candidates carry code: a routing intermediary and a pool. Guessing one would net
        // residue dust as an absurd swap, so the fill must be declined.
        asserter.push_success(&Bytes::from(vec![0xfe]));
        asserter.push_success(&Bytes::from(vec![0xfe]));
        let provider = mocked_provider(&asserter);

        let registry = Registry::ethereum();
        let mut cache = HashMap::new();
        let flow =
            find_maker_trade(&provider, &inverse_swap_ledger(), &[], &registry, &mut cache).await;
        assert!(flow.is_none());
    }

    #[test]
    fn maker_candidates_finds_swap_sides_excluding_filler() {
        // Intent fill: the maker sells token_a for token_b; the pool is the
        // counterparty. The filler is excluded.
        let registry = Registry::ethereum();
        let maker = addr(100);
        let pool = addr(101);
        let filler = addr(102);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, maker, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, maker, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);

        let found: HashMap<Address, _> = maker_candidates(&ledger, &[filler], &registry)
            .into_iter()
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[&maker], swap(token_a, 1000, token_b, 2000));
        // The pool nets the inverse swap; the EOA filter discards it later.
        assert_eq!(found[&pool], swap(token_b, 2000, token_a, 1000));
    }

    #[test]
    fn maker_candidates_drops_excluded_and_known() {
        let registry = Registry::ethereum();
        let maker = addr(100);
        let pool = addr(101);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, maker, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, maker, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);

        // Excluding the maker leaves only the pool.
        let candidates = maker_candidates(&ledger, &[maker], &registry);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, pool);
    }
}
