use std::collections::{BTreeSet, HashMap};

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::{Log, TransactionReceipt},
    sol_types::SolEvent,
};
use tracing::warn;

use crate::decoder::{
    net::{decode_trade, to_primitive_log, Transfer},
    registry::is_batch_settler,
};

/// A matched transaction and how it was found.
pub(crate) struct Matched<'a> {
    pub receipt: &'a TransactionReceipt,
    pub entry_point: Address,
    /// The swapper is an order maker, not the sender: either the tx was discovered via a known
    /// aggregator log (`tx.to` is a filler) or `tx.to` is a batch settler entered by a solver.
    pub intent_fill: bool,
}

/// Match a receipt by entry point or by a known aggregator log emitter.
pub(crate) fn match_receipt<'a>(
    receipt: &'a TransactionReceipt,
    names: &HashMap<Address, &'static str>,
    aggregators: &HashMap<Address, &'static str>,
) -> Option<Matched<'a>> {
    if !receipt.status() {
        return None;
    }
    let entry_point = receipt.to?;
    if names.contains_key(&entry_point) {
        // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is an
        // order maker's net flow — decode it like a filler-initiated intent fill.
        return Some(Matched { receipt, entry_point, intent_fill: is_batch_settler(&entry_point) });
    }
    let via_log = receipt
        .logs()
        .iter()
        .any(|log| aggregators.contains_key(&log.address()));
    via_log.then_some(Matched { receipt, entry_point, intent_fill: true })
}

/// Find the order maker's trade in a filler-initiated intent fill.
///
/// The transaction sender is the filler, not the swapper, so we look for the
/// account whose net flow is a clean two-token swap. Pools net the inverse
/// swap, so we exclude contracts (via `eth_getCode`) and keep the
/// externally-owned maker. Known registry contracts and the excluded
/// addresses (filler, entry point) never qualify.
///
/// v0 limitations (tracked for a decode/attribution rework):
/// - **One maker per transaction.** The first clean-net EOA wins, so a batch that settles several
///   retail orders in one tx contributes a single decoded trade; the rest surface as "Allium only"
///   gaps and batch volume is under-counted.
/// - **No settlement-tied tiebreak.** When several non-excluded EOAs each net to a clean two-token
///   swap, the winner is just the first in `maker_candidates`' address-ordered iteration, so a
///   decode can attribute the wrong account's flow.
pub(crate) async fn find_maker_trade<P: Provider>(
    provider: &P,
    logs: &[Log],
    native: &[(Address, Address, U256)],
    exclude: &[Address],
    names: &HashMap<Address, &'static str>,
    code_cache: &mut HashMap<Address, bool>,
) -> Option<(Address, (Address, U256, Address, U256))> {
    // Prefer externally-owned accounts; pools and routers carry code.
    let mut maker = None;
    for (candidate, trade) in maker_candidates(logs, native, exclude, names) {
        if !is_contract(provider, candidate, code_cache).await {
            return Some((candidate, trade));
        }
        maker.get_or_insert((candidate, trade));
    }
    maker
}

/// Addresses with a clean two-token net swap, excluding the zero address, the
/// excluded addresses, and known registry contracts. Ordered by address for
/// deterministic selection.
fn maker_candidates(
    logs: &[Log],
    native: &[(Address, Address, U256)],
    exclude: &[Address],
    names: &HashMap<Address, &'static str>,
) -> Vec<(Address, (Address, U256, Address, U256))> {
    let mut candidates: BTreeSet<Address> = BTreeSet::new();
    for log in logs {
        if let Ok(transfer) = Transfer::decode_log(&to_primitive_log(log)) {
            candidates.insert(transfer.from);
            candidates.insert(transfer.to);
        }
    }
    for &(from, to, _) in native {
        candidates.insert(from);
        candidates.insert(to);
    }
    candidates.remove(&Address::ZERO);
    candidates.retain(|address| !exclude.contains(address) && !names.contains_key(address));

    let mut swaps = Vec::new();
    for candidate in candidates {
        if let Some(trade) = decode_trade(logs, native, candidate) {
            swaps.push((candidate, trade));
        }
    }
    swaps
}

/// Whether an address has contract code, cached per block. On RPC failure the
/// address is treated as a contract so it is not mistaken for an EOA maker.
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
    use super::*;
    use crate::decoder::{
        registry::known_names,
        test_utils::{addr, make_transfer_log},
    };

    #[test]
    fn maker_candidates_finds_swap_sides_excluding_filler() {
        // Intent fill: the maker sells token_a for token_b; the pool is the
        // counterparty. The filler is excluded.
        let names = known_names();
        let maker = addr(100);
        let pool = addr(101);
        let filler = addr(102);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, maker, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, maker, U256::from(2000)),
        ];

        let found: HashMap<Address, _> = maker_candidates(&logs, &[], &[filler], names)
            .into_iter()
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[&maker], (token_a, U256::from(1000), token_b, U256::from(2000)));
        // The pool nets the inverse swap; the EOA filter discards it later.
        assert_eq!(found[&pool], (token_b, U256::from(2000), token_a, U256::from(1000)));
    }

    #[test]
    fn maker_candidates_drops_excluded_and_known() {
        let names = known_names();
        let maker = addr(100);
        let pool = addr(101);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, maker, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, maker, U256::from(2000)),
        ];

        // Excluding the maker leaves only the pool.
        let candidates = maker_candidates(&logs, &[], &[maker], names);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, pool);
    }
}
