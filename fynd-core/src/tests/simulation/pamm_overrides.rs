use alloy::primitives::{Address, B256, U256};
use tycho_simulation::{
    price_level_stream::overrides::AccountOverride as TychoAccountOverride, tycho_common::Bytes,
};

use super::{fixtures::*, *};
use crate::tests::metrics::recorded_metrics;

fn alloy_address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn alloy_word(byte: u8) -> B256 {
    B256::from(U256::from(byte))
}

/// The value `merged` holds for `slot` of the account whose address repeats `byte`.
fn slot_of(merged: &StateOverride, byte: u8, slot: u8) -> Option<B256> {
    merged
        .get(&alloy_address(byte))?
        .state_diff
        .as_ref()?
        .get(&alloy_word(slot))
        .copied()
}

#[test]
fn test_conversion_keeps_every_field() {
    let account = TychoAccountOverride {
        balance: Some(Bytes::from(vec![0x1b, 0x60, 0x08, 0xea])),
        nonce: Some(0xe40),
        state_diff: [(word(0), word(0x04))]
            .into_iter()
            .collect(),
    };
    let swap = pamm_swap("pamm", Some(overrides(Some(26_201_954), [(0x48, account)])));

    let converted = swap
        .simulation_overrides()
        .expect("a pAMM swap with overrides");

    assert_eq!(converted.block_number(), Some(26_201_954));
    assert_eq!(converted.block_timestamp(), Some(timestamp_of(26_201_954)));
    let account = &converted.accounts()[&alloy_address(0x48)];
    assert_eq!(account.balance, Some(U256::from(0x1b6008ea_u64)));
    assert_eq!(account.nonce, Some(0xe40));
    assert_eq!(
        account
            .state_diff
            .as_ref()
            .and_then(|slots| slots.get(&alloy_word(0)).copied()),
        Some(alloy_word(0x04))
    );
    assert_eq!(account.state, None);
    assert_eq!(account.code, None);
}

/// An account that overrides no storage gets no `stateDiff` at all, rather than an empty one.
#[test]
fn test_conversion_storage_free_account() {
    let account =
        TychoAccountOverride { balance: None, nonce: Some(1), state_diff: Default::default() };
    let swap = pamm_swap("pamm", Some(overrides(Some(1), [(0x48, account)])));

    let converted = swap
        .simulation_overrides()
        .expect("a pAMM swap with overrides");

    assert_eq!(converted.accounts()[&alloy_address(0x48)].state_diff, None);
}

#[rstest::rstest]
#[case::short_address(Bytes::from(vec![0x48; 19]), storage(&[(0, 1)]))]
#[case::wide_balance(
    address(0x48),
    TychoAccountOverride { balance: Some(Bytes::from(vec![1; 33])), nonce: None, state_diff: Default::default() }
)]
#[case::short_slot(
    address(0x48),
    TychoAccountOverride {
        balance: None,
        nonce: None,
        state_diff: [(Bytes::from(vec![0; 31]), word(1))].into_iter().collect(),
    }
)]
#[case::short_word(
    address(0x48),
    TychoAccountOverride {
        balance: None,
        nonce: None,
        state_diff: [(word(0), Bytes::from(vec![1; 31]))].into_iter().collect(),
    }
)]
fn test_conversion_refuses_a_wrong_width(
    #[case] account_address: Bytes,
    #[case] account: TychoAccountOverride,
) {
    let mut entry = overrides(Some(1), []);
    entry
        .accounts
        .insert(account_address, account);
    let swap = pamm_swap("pamm", Some(entry));

    assert!(matches!(SwapOverrides::of(&swap), SwapOverrides::ConversionFailed(_)));
    assert_eq!(swap.simulation_overrides(), None);
}

#[test]
fn test_swap_overrides_by_state() {
    assert!(matches!(SwapOverrides::of(&pool_swap("pool")), SwapOverrides::NotPamm));
    assert!(matches!(SwapOverrides::of(&pamm_swap("pamm", None)), SwapOverrides::Missing));
    assert!(matches!(
        SwapOverrides::of(&pamm_swap("pamm", Some(overrides(Some(1), [])))),
        SwapOverrides::Attached(_)
    ));
}

#[test]
fn test_collect_route_without_pamm_swaps() {
    let (merged, report) = collect(&route(vec![pool_swap("a"), pool_swap("b")]));

    assert_eq!(merged, None);
    assert_eq!(report, OverridesReport::default());
    assert!(!report.has_pamm_swap());
}

/// A route whose pAMM swaps carry nothing to merge still reports them, so the quote is counted.
#[test]
fn test_collect_pamm_swaps_without_overrides() {
    let mut unconvertible = overrides(Some(1), []);
    unconvertible
        .accounts
        .insert(Bytes::from(vec![0x48; 19]), storage(&[(0, 1)]));
    let (merged, report) = collect(&route(vec![
        pamm_swap("missing", None),
        pamm_swap("unconvertible", Some(unconvertible)),
        pool_swap("pool"),
    ]));

    assert_eq!(merged, None);
    assert_eq!(report.missing, 1);
    assert_eq!(report.conversion_failures.len(), 1);
    assert_eq!(report.attached, 0);
    assert!(report.has_pamm_swap());
}

#[test]
fn test_collect_unions_accounts_and_slots() {
    let (merged, report) = collect(&route(vec![
        pamm_swap("first", Some(overrides(Some(10), [(0xa1, storage(&[(0, 1)]))]))),
        pool_swap("pool"),
        pamm_swap(
            "second",
            Some(overrides(Some(10), [(0xa1, storage(&[(1, 2)])), (0xb2, storage(&[(0, 3)]))])),
        ),
    ]));
    let merged = merged.expect("two swaps with overrides");

    assert_eq!(slot_of(&merged.accounts, 0xa1, 0), Some(alloy_word(1)));
    assert_eq!(slot_of(&merged.accounts, 0xa1, 1), Some(alloy_word(2)));
    assert_eq!(slot_of(&merged.accounts, 0xb2, 0), Some(alloy_word(3)));
    assert_eq!(merged.block_number, Some(10));
    assert_eq!(merged.block_timestamp, Some(timestamp_of(10)));
    assert_eq!(
        report,
        OverridesReport { attached: 2, ..OverridesReport::default() },
        "the swaps agree on everything they share"
    );
}

/// Where two swaps set one value differently, the later swap in the route wins and the
/// disagreement is counted. Setting the same value twice is no conflict.
#[test]
fn test_collect_later_swap_wins_a_conflict() {
    let earlier = TychoAccountOverride {
        balance: Some(Bytes::from(vec![1])),
        nonce: Some(1),
        state_diff: [(word(0), word(1)), (word(1), word(7))]
            .into_iter()
            .collect(),
    };
    let later = TychoAccountOverride {
        balance: Some(Bytes::from(vec![2])),
        nonce: Some(2),
        state_diff: [(word(0), word(2)), (word(1), word(7))]
            .into_iter()
            .collect(),
    };
    let (merged, report) = collect(&route(vec![
        pamm_swap("earlier", Some(overrides(Some(10), [(0xa1, earlier)]))),
        pamm_swap("later", Some(overrides(Some(10), [(0xa1, later)]))),
    ]));
    let account = &merged.expect("overrides").accounts[&alloy_address(0xa1)];

    assert_eq!(account.balance, Some(U256::from(2)));
    assert_eq!(account.nonce, Some(2));
    assert_eq!(
        account
            .state_diff
            .as_ref()
            .and_then(|slots| slots.get(&alloy_word(0)).copied()),
        Some(alloy_word(2))
    );
    assert_eq!(report.storage_conflicts, 1);
    assert_eq!(report.balance_conflicts, 1);
    assert_eq!(report.nonce_conflicts, 1);
}

#[test]
fn test_collect_takes_the_newest_block() {
    let (merged, report) = collect(&route(vec![
        pamm_swap("newer", Some(overrides(Some(11), []))),
        pamm_swap("older", Some(overrides(Some(10), []))),
    ]));
    let merged = merged.expect("overrides");

    assert_eq!(merged.block_number, Some(11));
    assert_eq!(merged.block_timestamp, Some(timestamp_of(11)));
    assert!(report.block_mismatch);
}

/// A swap whose overrides name no block contributes its accounts and no block, and is counted.
#[test]
fn test_collect_overrides_without_a_block() {
    let mut no_timestamp = overrides(Some(10), [(0xa1, storage(&[(0, 1)]))]);
    no_timestamp.block_timestamp = None;
    let (merged, report) = collect(&route(vec![
        pamm_swap("no-block", Some(overrides(None, [(0xb2, storage(&[(0, 2)]))]))),
        pamm_swap("no-timestamp", Some(no_timestamp)),
    ]));
    let merged = merged.expect("overrides");

    assert_eq!(merged.block_number, Some(10));
    assert_eq!(merged.block_timestamp, None);
    assert_eq!(slot_of(&merged.accounts, 0xb2, 0), Some(alloy_word(2)));
    assert_eq!(report.missing_block_number, 1);
    assert_eq!(report.missing_block_timestamp, 2);
    assert!(!report.block_mismatch, "a swap naming no block names no other block");
}

/// A later swap of the newest block fills in a timestamp the first one lacked.
#[test]
fn test_collect_timestamp_of_the_newest_block() {
    let mut no_timestamp = overrides(Some(10), []);
    no_timestamp.block_timestamp = None;
    let (merged, _) = collect(&route(vec![
        pamm_swap("no-timestamp", Some(no_timestamp)),
        pamm_swap("older", Some(overrides(Some(9), []))),
        pamm_swap("with-timestamp", Some(overrides(Some(10), []))),
    ]));

    assert_eq!(
        merged
            .expect("overrides")
            .block_timestamp,
        Some(timestamp_of(10))
    );
}

#[test]
fn test_add_venue_accounts_keeps_the_funding() {
    let sender = alloy_address(0x01);
    let token = alloy_address(0x02);
    let mut funding = StateOverride::from_iter([
        (
            sender,
            AccountOverride { balance: Some(U256::from(1)), nonce: Some(1), ..Default::default() },
        ),
        (
            token,
            AccountOverride {
                state_diff: Some(B256HashMap::from_iter([(alloy_word(0), alloy_word(1))])),
                ..Default::default()
            },
        ),
    ]);
    let venues = StateOverride::from_iter([
        (
            sender,
            AccountOverride { balance: Some(U256::from(9)), nonce: Some(1), ..Default::default() },
        ),
        (
            token,
            AccountOverride {
                state_diff: Some(B256HashMap::from_iter([
                    (alloy_word(0), alloy_word(9)),
                    (alloy_word(5), alloy_word(5)),
                ])),
                ..Default::default()
            },
        ),
        (alloy_address(0x03), AccountOverride { nonce: Some(4), ..Default::default() }),
    ]);

    let collisions = add_venue_accounts(&mut funding, &venues);

    assert_eq!(collisions, 2, "the sender's balance and the token's slot 0");
    assert_eq!(funding[&sender].balance, Some(U256::from(1)));
    assert_eq!(slot_of(&funding, 0x02, 0), Some(alloy_word(1)));
    assert_eq!(slot_of(&funding, 0x02, 5), Some(alloy_word(5)));
    assert_eq!(funding[&alloy_address(0x03)].nonce, Some(4));
}

#[test]
fn test_record_counts_a_quote_with_pamm_swaps() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let report = OverridesReport {
        attached: 2,
        missing: 1,
        storage_conflicts: 3,
        block_mismatch: true,
        ..OverridesReport::default()
    };

    metrics::with_local_recorder(&recorder, || report.record("order"));

    let recorded = recorded_metrics(&snapshotter);
    let count = |name: &str, label: Option<&str>| {
        recorded
            .iter()
            .find(|(recorded_name, labels, _)| {
                recorded_name == name &&
                    label.is_none_or(|label| labels.contains(&label.to_string()))
            })
            .map(|(_, _, value)| value)
    };
    use metrics_util::debugging::DebugValue::Counter;
    assert_eq!(count(LEGS_TOTAL, Some("outcome=attached")), Some(&Counter(2)));
    assert_eq!(count(LEGS_TOTAL, Some("outcome=missing")), Some(&Counter(1)));
    assert_eq!(count(LEGS_TOTAL, Some("outcome=conversion_failed")), Some(&Counter(0)));
    assert_eq!(count(CONFLICTS_TOTAL, Some("field=storage")), Some(&Counter(3)));
    assert_eq!(count(BLOCK_MISMATCH_TOTAL, None), Some(&Counter(1)));
}

#[test]
fn test_record_ignores_a_quote_without_pamm_swaps() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || OverridesReport::default().record("order"));

    assert!(recorded_metrics(&snapshotter).is_empty());
}

#[test]
fn test_register_metrics_at_zero() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, register_metrics);

    let recorded = recorded_metrics(&snapshotter);
    for name in [
        LEGS_TOTAL,
        MISSING_BLOCK_TOTAL,
        CONFLICTS_TOTAL,
        BLOCK_MISMATCH_TOTAL,
        FUNDING_COLLISIONS_TOTAL,
        SIMULATIONS_TOTAL,
    ] {
        assert!(
            recorded
                .iter()
                .any(|(recorded_name, ..)| recorded_name == name),
            "{name} is not registered"
        );
    }
    assert!(recorded
        .iter()
        .all(|(.., value)| *value == metrics_util::debugging::DebugValue::Counter(0)));
}
