//! Bracket-pair sandwich detection.
//!
//! For a victim trade, scans the receipts immediately around it — already fetched by
//! [`super::Decoder::decode_block`] in one `eth_getBlockReceipts` call, so detection makes no
//! extra RPC calls — for a front-run/back-run pair that both link to one attacker and both touch
//! a pool the victim's swap touched. See
//! `docs/superpowers/specs/2026-07-13-hindsight-sandwich-detection-design.md` for the heuristic
//! and its known coarseness (Uniswap V4's singleton pool manager collapses per-pool overlap to
//! per-protocol).

use std::collections::HashSet;

use alloy::{
    primitives::{Address, TxHash},
    rpc::types::TransactionReceipt,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{ledger::Transfer, registry::Registry, trace::PERMIT2};

sol! {
    /// ERC-20 `Approval` — emitted by token contracts, never by pools, so its emitters are
    /// filtered out of pool candidates like `Transfer`'s.
    event Approval(address indexed owner, address indexed spender, uint256 value);
}

/// Transactions scanned on each side of the victim for a bracket pair.
const WINDOW: usize = 2;

/// Evidence that a victim trade was bracketed by a front-run and a back-run sharing an attacker
/// and a pool.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SandwichEvidence {
    pub front_tx: TxHash,
    pub back_tx: TxHash,
    /// The linking address: shared sender, or the shared non-registry-known target contract.
    pub attacker: Address,
    /// The overlapping pool contracts.
    pub pools: Vec<Address>,
}

/// Scan the window around `victim_index` for the closest front/back pair that brackets the
/// victim trade (see the module docs for the two conditions a pair must satisfy).
///
/// `receipts` is the block's full receipt list in block order; `victim_index` is the victim's
/// position in that slice, and `victim_sender` its trader (excluded as a linking address — a
/// trader on both sides of their own trade is not a sandwich). Candidate pairs are tried closest
/// front first, then closest back, so the first match found is the tightest bracket.
pub(crate) fn detect(
    receipts: &[TransactionReceipt],
    victim_index: usize,
    victim_sender: Address,
    registry: &Registry,
) -> Option<SandwichEvidence> {
    let victim_pools = pool_addresses(&receipts[victim_index], registry);
    if victim_pools.is_empty() {
        return None;
    }

    let front_start = victim_index.saturating_sub(WINDOW);
    let back_end = (victim_index + WINDOW).min(receipts.len().saturating_sub(1));

    for front_index in (front_start..victim_index).rev() {
        for back_index in (victim_index + 1)..=back_end {
            let front = &receipts[front_index];
            let back = &receipts[back_index];
            let Some(attacker) = shared_attacker(front, back, victim_sender, registry) else {
                continue;
            };
            let Some(pools) = overlapping_pools(&victim_pools, front, back, registry) else {
                continue;
            };
            return Some(SandwichEvidence {
                front_tx: front.transaction_hash,
                back_tx: back.transaction_hash,
                attacker,
                pools,
            });
        }
    }
    None
}

/// Whether `front` and `back` share a link that plausibly identifies one attacker running both
/// legs: the same sender, or the same target contract that is not a registry-known venue or
/// solver. The registry exclusion keeps two unrelated users entering the same popular router
/// (Universal Router, 1inch) from tripping the same-`to` check — real sandwich bots settle
/// through private contracts. A link matching the victim's own sender is excluded: a trader on
/// both sides of their own trade is not a sandwich.
fn shared_attacker(
    front: &TransactionReceipt,
    back: &TransactionReceipt,
    victim_sender: Address,
    registry: &Registry,
) -> Option<Address> {
    if front.from == back.from && front.from != victim_sender {
        return Some(front.from);
    }
    let (Some(front_to), Some(back_to)) = (front.to, back.to) else {
        return None;
    };
    if front_to == back_to && front_to != victim_sender && !registry.is_known(front_to) {
        return Some(front_to);
    }
    None
}

/// The victim's pool contracts (see [`pool_addresses`]) that `front` and `back` each
/// independently re-emitted a log from — `front` and `back` need not touch the same one. `None`
/// when either leg missed the overlap.
fn overlapping_pools(
    victim_pools: &HashSet<Address>,
    front: &TransactionReceipt,
    back: &TransactionReceipt,
    registry: &Registry,
) -> Option<Vec<Address>> {
    let front_overlap: HashSet<Address> = pool_addresses(front, registry)
        .intersection(victim_pools)
        .copied()
        .collect();
    let back_overlap: HashSet<Address> = pool_addresses(back, registry)
        .intersection(victim_pools)
        .copied()
        .collect();
    if front_overlap.is_empty() || back_overlap.is_empty() {
        return None;
    }
    let mut pools: Vec<Address> = front_overlap
        .union(&back_overlap)
        .copied()
        .collect();
    pools.sort();
    Some(pools)
}

/// The addresses that emitted a log in a transaction, minus everything known not to be a pool —
/// candidate pool contracts.
///
/// `Transfer` and `Approval` emitters are token contracts, and the wrapped-native token
/// (`Deposit`/`Withdrawal`) and Permit2 (its own permit events) log on most swaps without being
/// pools: counting any of them would give two transactions that merely share a token or its
/// plumbing a trivial "pool" overlap.
fn pool_addresses(receipt: &TransactionReceipt, registry: &Registry) -> HashSet<Address> {
    let mut pools = HashSet::new();
    for log in receipt.logs() {
        let token_event = log
            .topics()
            .first()
            .is_some_and(|topic| {
                *topic == Transfer::SIGNATURE_HASH || *topic == Approval::SIGNATURE_HASH
            });
        if token_event || log.address() == registry.wrapped_native() || log.address() == PERMIT2 {
            continue;
        }
        pools.insert(log.address());
    }
    pools
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, U256};

    use super::*;
    use crate::decoder::test_utils::{addr, make_pool_log, make_transfer_log, receipt, tx_hash};

    #[test]
    fn detects_same_from_bracket() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(pool)]),
        ];

        let evidence = detect(&receipts, 1, victim_sender, &Registry::ethereum()).unwrap();
        assert_eq!(evidence.front_tx, tx_hash(1));
        assert_eq!(evidence.back_tx, tx_hash(3));
        assert_eq!(evidence.attacker, attacker);
        assert_eq!(evidence.pools, vec![pool]);
    }

    #[test]
    fn detects_same_to_when_not_registry_known() {
        let victim_sender = addr(1);
        let shared_to = addr(77);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(shared_to), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(shared_to), vec![make_pool_log(pool)]),
        ];

        let evidence = detect(&receipts, 1, victim_sender, &Registry::ethereum()).unwrap();
        assert_eq!(evidence.attacker, shared_to);
    }

    #[test]
    fn same_to_registry_known_is_not_flagged() {
        // 1inch is a registered solver: two unrelated traders entering it must not read as a
        // shared-attacker link.
        let victim_sender = addr(1);
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(oneinch), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(oneinch), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn attacker_link_without_pool_overlap_is_not_flagged() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let unrelated_pool = addr(51);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(unrelated_pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(unrelated_pool)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn front_only_pool_overlap_is_not_flagged() {
        // Both legs must re-touch a victim pool: an attacker-linked pair where only the front
        // leg overlaps is not a bracket.
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(addr(51))]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn back_only_pool_overlap_is_not_flagged() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(addr(51))]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn pool_overlap_without_attacker_link_is_not_flagged() {
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(addr(30)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(addr(31)), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn pairs_outside_window_are_ignored() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        // Victim at index 3; the matching pair sits at distance 3 on each side (indices 0 and 6),
        // one step outside the W=2 window, so no bracket must be reported.
        let receipts = vec![
            receipt(tx_hash(0), attacker, Some(addr(40)), vec![make_pool_log(pool)]),
            receipt(tx_hash(1), addr(21), Some(addr(41)), vec![]),
            receipt(tx_hash(2), addr(22), Some(addr(42)), vec![]),
            receipt(tx_hash(3), victim_sender, Some(addr(43)), vec![make_pool_log(pool)]),
            receipt(tx_hash(4), addr(24), Some(addr(44)), vec![]),
            receipt(tx_hash(5), addr(25), Some(addr(45)), vec![]),
            receipt(tx_hash(6), attacker, Some(addr(46)), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 3, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn closest_bracket_pair_wins() {
        let close_attacker = addr(90);
        let far_attacker = addr(91);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(0), far_attacker, Some(addr(20)), vec![make_pool_log(pool)]),
            receipt(tx_hash(1), close_attacker, Some(addr(21)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(22)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), close_attacker, Some(addr(23)), vec![make_pool_log(pool)]),
            receipt(tx_hash(4), far_attacker, Some(addr(24)), vec![make_pool_log(pool)]),
        ];

        let evidence = detect(&receipts, 2, victim_sender, &Registry::ethereum()).unwrap();
        assert_eq!(evidence.attacker, close_attacker);
    }

    #[test]
    fn self_sandwich_by_shared_sender_excluded() {
        // The victim's own address appears on both sides — not a sandwich.
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), victim_sender, Some(addr(10)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), victim_sender, Some(addr(12)), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn self_sandwich_by_shared_target_excluded() {
        // The victim's own address is the shared `to` — also not a sandwich.
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(victim_sender), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), addr(30), Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(victim_sender), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn transfer_only_victim_logs_yield_no_pools() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let token = addr(60);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(addr(50))]),
            receipt(
                tx_hash(2),
                victim_sender,
                Some(addr(11)),
                vec![make_transfer_log(token, victim_sender, addr(11), U256::from(1u64))],
            ),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(addr(50))]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn wrapped_native_logs_are_not_pools() {
        // Every ETH-wrapping transaction logs from the WETH contract (Deposit/Withdrawal are not
        // Transfer events), so WETH alone must never count as a shared pool.
        let registry = Registry::ethereum();
        let attacker = addr(90);
        let victim_sender = addr(1);
        let weth = registry.wrapped_native();
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(weth)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(weth)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(weth)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &registry).is_none());
    }

    #[test]
    fn approval_logs_are_not_pools() {
        // ERC-20 Approval is emitted by token contracts; a shared token approval is not a
        // shared pool.
        let attacker = addr(90);
        let victim_sender = addr(1);
        let token = addr(60);
        let approval = |owner: Address| {
            let primitive = alloy::primitives::Log::new_unchecked(
                token,
                vec![Approval::SIGNATURE_HASH, owner.into_word(), addr(70).into_word()],
                alloy::primitives::Bytes::new(),
            );
            alloy::rpc::types::Log { inner: primitive, ..Default::default() }
        };
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![approval(attacker)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![approval(victim_sender)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![approval(attacker)]),
        ];

        assert!(detect(&receipts, 1, victim_sender, &Registry::ethereum()).is_none());
    }

    #[test]
    fn victim_at_block_edge_does_not_panic() {
        let victim_sender = addr(1);
        let receipts =
            vec![receipt(tx_hash(0), victim_sender, Some(addr(9)), vec![make_pool_log(addr(50))])];

        assert!(detect(&receipts, 0, victim_sender, &Registry::ethereum()).is_none());
    }
}
