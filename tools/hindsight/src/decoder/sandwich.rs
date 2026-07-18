//! Bracket-pair sandwich detection.
//!
//! For each victim trade, scan the transactions immediately around it in the block for a
//! front-run/back-run pair that satisfies three conditions: both legs link to one attacker,
//! both touch a pool the victim's swap touched, and the attacker accumulates the victim's
//! output token before the victim trades and disposes of it after. The block's receipts were
//! already fetched for decoding, so detection costs no extra RPC calls.
//!
//! See `docs/superpowers/specs/2026-07-13-hindsight-sandwich-detection-design.md` for the
//! heuristic and its known coarseness (Uniswap V4's singleton pool manager collapses per-pool
//! overlap to per-protocol).

use std::collections::HashSet;

use alloy::{
    primitives::{Address, TxHash, U256},
    rpc::types::TransactionReceipt,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    registry::Registry,
    transfer_ledger::{to_primitive_log, Transfer},
    DecodedTrade,
};

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
/// victim trade (see the module docs for the three conditions a pair must satisfy).
///
/// `receipts` is the block's full receipt list in block order; `victim_index` is the victim's
/// position in that slice. The victim's sender is excluded as a linking address — a trader on
/// both sides of their own trade is not a sandwich. Candidate pairs are tried closest front
/// first, then closest back, so the first match found is the tightest bracket.
pub(crate) fn detect(
    receipts: &[TransactionReceipt],
    victim_index: usize,
    victim: &DecodedTrade,
    registry: &Registry,
) -> Option<SandwichEvidence> {
    let victim_pools = pool_addresses(&receipts[victim_index], registry);
    if victim_pools.is_empty() {
        return None;
    }
    let token = direction_token(victim.token_out, registry);

    let front_start = victim_index.saturating_sub(WINDOW);
    let back_end = (victim_index + WINDOW).min(receipts.len().saturating_sub(1));

    for front_index in (front_start..victim_index).rev() {
        for back_index in (victim_index + 1)..=back_end {
            let front = &receipts[front_index];
            let back = &receipts[back_index];
            let Some(attacker) = shared_attacker(front, back, victim.sender, registry) else {
                continue;
            };
            let Some(pools) = overlapping_pools(&victim_pools, front, back, registry) else {
                continue;
            };
            let entities = direction_entities(front, back, attacker, victim.sender, registry);
            if !accumulates_then_disposes(front, back, &entities, token) {
                continue;
            }
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
/// legs: the same sender, or the same target contract.
///
/// A shared target only counts when it is not a known venue or solver — otherwise two unrelated
/// users entering the same popular router (Universal Router, 1inch) would look linked, while
/// real sandwich bots settle through private contracts. A link matching the victim's own sender
/// is also excluded: a trader on both sides of their own trade is not a sandwich.
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
/// `Transfer` and `Approval` emitters are token contracts, and the registry's infrastructure
/// addresses (the wrapped-native token's `Deposit`/`Withdrawal`, Permit2's permit events) log on
/// most swaps without being pools: counting any of them would give two transactions that merely
/// share a token or an infrastructure contract a trivial "pool" overlap.
fn pool_addresses(receipt: &TransactionReceipt, registry: &Registry) -> HashSet<Address> {
    let mut pools = HashSet::new();
    for log in receipt.logs() {
        let token_event = log
            .topics()
            .first()
            .is_some_and(|topic| {
                *topic == Transfer::SIGNATURE_HASH || *topic == Approval::SIGNATURE_HASH
            });
        if token_event || registry.is_infrastructure(log.address()) {
            continue;
        }
        pools.insert(log.address());
    }
    pools
}

/// The token whose flow confirms sandwich direction: the victim's output token, in the form
/// receipts can see. Native ETH moves emit no log, so the wrapped form stands in — pools hold
/// and transfer WETH even when the trader's side settles native.
fn direction_token(victim_token_out: Address, registry: &Registry) -> Address {
    if victim_token_out == Address::ZERO {
        registry.wrapped_native()
    } else {
        victim_token_out
    }
}

/// The addresses whose token flow can confirm the attacker's direction: the linking address
/// itself, plus the pair's shared target contract when the link was a shared sender — a bot's
/// inventory usually sits in its private contract, not in the EOA that signs.
fn direction_entities(
    front: &TransactionReceipt,
    back: &TransactionReceipt,
    attacker: Address,
    victim_sender: Address,
    registry: &Registry,
) -> Vec<Address> {
    let mut entities = vec![attacker];
    if let (Some(front_to), Some(back_to)) = (front.to, back.to) {
        if front_to == back_to &&
            front_to != attacker &&
            front_to != victim_sender &&
            !registry.is_known(front_to)
        {
            entities.push(front_to);
        }
    }
    entities
}

/// Whether any linked entity accumulated `token` in the front leg and disposed of it in the back
/// leg — the shape of a real sandwich (buy the victim's output token before them, sell it after).
///
/// A linked pair without this flow is far more often benign repeat activity on a busy pool (an
/// arbitrage bot trading it twice in the window) than a sandwich, so it is not flagged. The cost
/// is missing an attacker whose legs move only native ETH — invisible in receipts — which is rare:
/// bots keep inventory wrapped precisely because pools settle in WETH.
fn accumulates_then_disposes(
    front: &TransactionReceipt,
    back: &TransactionReceipt,
    entities: &[Address],
    token: Address,
) -> bool {
    entities.iter().any(|&entity| {
        let (front_received, front_sent) = token_flow(front, entity, token);
        let (back_received, back_sent) = token_flow(back, entity, token);
        front_received > front_sent && back_sent > back_received
    })
}

/// `(received, sent)` totals of `token` for `entity` across a receipt's ERC-20 `Transfer` logs.
fn token_flow(receipt: &TransactionReceipt, entity: Address, token: Address) -> (U256, U256) {
    let mut received = U256::ZERO;
    let mut sent = U256::ZERO;
    for log in receipt.logs() {
        if log.address() != token {
            continue;
        }
        let Ok(transfer) = Transfer::decode_log(&to_primitive_log(log)) else {
            continue;
        };
        if transfer.to == entity {
            received = received.saturating_add(transfer.value);
        }
        if transfer.from == entity {
            sent = sent.saturating_add(transfer.value);
        }
    }
    (received, sent)
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{address, U256},
        rpc::types::Log,
    };

    use super::*;
    use crate::decoder::{
        test_utils::{addr, make_pool_log, make_transfer_log, receipt, tx_hash},
        AttributionSource,
    };

    /// The victim fixture's output token, unless a test overrides it.
    fn token_out() -> Address {
        addr(60)
    }

    fn victim(sender: Address) -> DecodedTrade {
        victim_swapping_into(sender, token_out())
    }

    fn victim_swapping_into(sender: Address, token_out: Address) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 1,
            tx_index: 0,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender,
            token_in: addr(59),
            token_out,
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(2_000u64),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        }
    }

    /// Front-leg logs: touch `pool` and accumulate `token` into `entity` (the pool pays out).
    fn buys(pool: Address, token: Address, entity: Address) -> Vec<Log> {
        vec![make_pool_log(pool), make_transfer_log(token, pool, entity, U256::from(500u64))]
    }

    /// Back-leg logs: touch `pool` and dispose of `token` from `entity` (paid into the pool).
    fn sells(pool: Address, token: Address, entity: Address) -> Vec<Log> {
        vec![make_pool_log(pool), make_transfer_log(token, entity, pool, U256::from(500u64))]
    }

    #[test]
    fn same_from_bracket() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), buys(pool, token_out(), attacker)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), sells(pool, token_out(), attacker)),
        ];

        let evidence = detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).unwrap();
        assert_eq!(evidence.front_tx, tx_hash(1));
        assert_eq!(evidence.back_tx, tx_hash(3));
        assert_eq!(evidence.attacker, attacker);
        assert_eq!(evidence.pools, vec![pool]);
    }

    #[test]
    fn same_to_bracket_not_registry_known() {
        let victim_sender = addr(1);
        let shared_to = addr(77);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(shared_to), buys(pool, token_out(), shared_to)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(shared_to), sells(pool, token_out(), shared_to)),
        ];

        let evidence = detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).unwrap();
        assert_eq!(evidence.attacker, shared_to);
    }

    #[test]
    fn same_to_bracket_registry_known() {
        // 1inch is a registered solver: two unrelated traders entering it must not read as a
        // shared-attacker link, even when the token flows happen to line up.
        let victim_sender = addr(1);
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(oneinch), buys(pool, token_out(), oneinch)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(oneinch), sells(pool, token_out(), oneinch)),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn attacker_link_without_pool_overlap() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let unrelated_pool = addr(51);
        let receipts = vec![
            receipt(
                tx_hash(1),
                attacker,
                Some(addr(10)),
                buys(unrelated_pool, token_out(), attacker),
            ),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(
                tx_hash(3),
                attacker,
                Some(addr(12)),
                sells(unrelated_pool, token_out(), attacker),
            ),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn front_only_pool_overlap() {
        // Both legs must re-touch a victim pool: an attacker-linked pair where only the front
        // leg overlaps is not a bracket.
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), buys(pool, token_out(), attacker)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), sells(addr(51), token_out(), attacker)),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn back_only_pool_overlap() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), buys(addr(51), token_out(), attacker)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), sells(pool, token_out(), attacker)),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn pool_overlap_without_attacker_link() {
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), addr(20), Some(addr(30)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), addr(21), Some(addr(31)), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn linked_pair_without_token_flow() {
        // Attacker link and pool overlap hold, but neither leg moves the victim's output token
        // for any linked entity — repeat activity on a busy pool, not a bracket.
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![make_pool_log(pool)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![make_pool_log(pool)]),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn same_direction_both_legs() {
        // An arbitrage bot buying the same token on the same pool twice around an unrelated
        // victim accumulates on both legs — no dispose leg, no sandwich.
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), buys(pool, token_out(), attacker)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), buys(pool, token_out(), attacker)),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn inventory_in_shared_contract() {
        // Typical bot shape: one EOA signs both legs (the link), but the token inventory moves
        // through its private contract — the shared `to` — not the EOA itself.
        let attacker_eoa = addr(90);
        let bot_contract = addr(80);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(
                tx_hash(1),
                attacker_eoa,
                Some(bot_contract),
                buys(pool, token_out(), bot_contract),
            ),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(
                tx_hash(3),
                attacker_eoa,
                Some(bot_contract),
                sells(pool, token_out(), bot_contract),
            ),
        ];

        let evidence = detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).unwrap();
        assert_eq!(evidence.attacker, attacker_eoa);
    }

    #[test]
    fn native_output_wrapped_flow() {
        // The victim receives native ETH, which emits no log: the attacker's legs show as WETH
        // transfers instead, so direction is confirmed on the wrapped form.
        let registry = Registry::ethereum();
        let weth = registry.wrapped_native();
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), buys(pool, weth, attacker)),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), sells(pool, weth, attacker)),
        ];

        let evidence =
            detect(&receipts, 1, &victim_swapping_into(victim_sender, Address::ZERO), &registry)
                .unwrap();
        assert_eq!(evidence.attacker, attacker);
    }

    #[test]
    fn pairs_outside_window() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let pool = addr(50);
        // Victim at index 3; the matching pair sits at distance 3 on each side (indices 0 and 6),
        // one step outside the W=2 window, so no bracket must be reported.
        let receipts = vec![
            receipt(tx_hash(0), attacker, Some(addr(40)), buys(pool, token_out(), attacker)),
            receipt(tx_hash(1), addr(21), Some(addr(41)), vec![]),
            receipt(tx_hash(2), addr(22), Some(addr(42)), vec![]),
            receipt(tx_hash(3), victim_sender, Some(addr(43)), vec![make_pool_log(pool)]),
            receipt(tx_hash(4), addr(24), Some(addr(44)), vec![]),
            receipt(tx_hash(5), addr(25), Some(addr(45)), vec![]),
            receipt(tx_hash(6), attacker, Some(addr(46)), sells(pool, token_out(), attacker)),
        ];

        assert!(detect(&receipts, 3, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn competing_bracket_pairs() {
        let close_attacker = addr(90);
        let far_attacker = addr(91);
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(
                tx_hash(0),
                far_attacker,
                Some(addr(20)),
                buys(pool, token_out(), far_attacker),
            ),
            receipt(
                tx_hash(1),
                close_attacker,
                Some(addr(21)),
                buys(pool, token_out(), close_attacker),
            ),
            receipt(tx_hash(2), victim_sender, Some(addr(22)), vec![make_pool_log(pool)]),
            receipt(
                tx_hash(3),
                close_attacker,
                Some(addr(23)),
                sells(pool, token_out(), close_attacker),
            ),
            receipt(
                tx_hash(4),
                far_attacker,
                Some(addr(24)),
                sells(pool, token_out(), far_attacker),
            ),
        ];

        let evidence = detect(&receipts, 2, &victim(victim_sender), &Registry::ethereum()).unwrap();
        assert_eq!(evidence.attacker, close_attacker);
    }

    #[test]
    fn self_sandwich_by_shared_sender() {
        // The victim's own address appears on both sides — not a sandwich.
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(
                tx_hash(1),
                victim_sender,
                Some(addr(10)),
                buys(pool, token_out(), victim_sender),
            ),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(
                tx_hash(3),
                victim_sender,
                Some(addr(12)),
                sells(pool, token_out(), victim_sender),
            ),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn self_sandwich_by_shared_target() {
        // The victim's own address is the shared `to` — also not a sandwich.
        let victim_sender = addr(1);
        let pool = addr(50);
        let receipts = vec![
            receipt(
                tx_hash(1),
                addr(20),
                Some(victim_sender),
                buys(pool, token_out(), victim_sender),
            ),
            receipt(tx_hash(2), addr(30), Some(addr(11)), vec![make_pool_log(pool)]),
            receipt(
                tx_hash(3),
                addr(21),
                Some(victim_sender),
                sells(pool, token_out(), victim_sender),
            ),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn pool_addresses_transfer_only_logs() {
        let attacker = addr(90);
        let victim_sender = addr(1);
        let token = addr(60);
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), buys(addr(50), token_out(), attacker)),
            receipt(
                tx_hash(2),
                victim_sender,
                Some(addr(11)),
                vec![make_transfer_log(token, victim_sender, addr(11), U256::from(1u64))],
            ),
            receipt(tx_hash(3), attacker, Some(addr(12)), sells(addr(50), token_out(), attacker)),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn pool_addresses_wrapped_native_logs() {
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

        assert!(detect(&receipts, 1, &victim(victim_sender), &registry).is_none());
    }

    #[test]
    fn pool_addresses_approval_logs() {
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
            Log { inner: primitive, ..Default::default() }
        };
        let receipts = vec![
            receipt(tx_hash(1), attacker, Some(addr(10)), vec![approval(attacker)]),
            receipt(tx_hash(2), victim_sender, Some(addr(11)), vec![approval(victim_sender)]),
            receipt(tx_hash(3), attacker, Some(addr(12)), vec![approval(attacker)]),
        ];

        assert!(detect(&receipts, 1, &victim(victim_sender), &Registry::ethereum()).is_none());
    }

    #[test]
    fn victim_at_block_edge() {
        let victim_sender = addr(1);
        let receipts =
            vec![receipt(tx_hash(0), victim_sender, Some(addr(9)), vec![make_pool_log(addr(50))])];

        assert!(detect(&receipts, 0, &victim(victim_sender), &Registry::ethereum()).is_none());
    }
}
