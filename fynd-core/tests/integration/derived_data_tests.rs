use crate::harness::TestHarness;
use fynd_core::recording::golden::load_golden_file;

/// All derived data fields should be computed after pipeline initialization.
#[tokio::test]
async fn test_all_derived_fields_computed() {
    let harness = TestHarness::from_fixture().await;
    let derived = harness.derived_data.read().await;

    assert!(
        derived.spot_prices().is_some(),
        "spot_prices should be computed"
    );
    assert!(
        derived.pool_depths().is_some(),
        "pool_depths should be computed"
    );
    assert!(
        derived.token_prices().is_some(),
        "token_prices should be computed"
    );
}

/// Derived data metrics should exactly match the golden baseline.
/// Since replay is deterministic (same recording → same derived data),
/// any deviation indicates a real bug, not expected variance.
#[tokio::test]
async fn test_derived_data_matches_golden() {
    let harness = TestHarness::from_fixture().await;
    let golden = load_golden_file().expect("golden_outputs.json required");
    let expected = golden
        .metadata
        .derived_data
        .expect("golden file missing derived_data metrics — regenerate with record-market");

    let market = harness.market_data.read().await;
    let derived = harness.derived_data.read().await;

    let spot_prices = derived.spot_prices().expect("spot prices not computed");
    let actual_spot_price_pools: std::collections::HashSet<_> = spot_prices
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();

    let pool_depths = derived.pool_depths().expect("pool depths not computed");
    let actual_pool_depth_pools: std::collections::HashSet<_> = pool_depths
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();

    let token_prices = derived.token_prices().expect("token prices not computed");

    // Also verify pool/token counts match golden metadata
    let actual_pools = market.component_topology().len();
    let actual_tokens = market.token_registry_ref().len();

    assert_eq!(
        actual_pools, golden.metadata.num_pools,
        "pool count mismatch (actual vs golden)"
    );
    assert_eq!(
        actual_tokens, golden.metadata.num_tokens,
        "token count mismatch (actual vs golden)"
    );
    assert_eq!(
        actual_spot_price_pools.len(),
        expected.spot_price_pools,
        "spot_price pool count mismatch (actual vs golden)"
    );
    assert_eq!(
        actual_pool_depth_pools.len(),
        expected.pool_depth_pools,
        "pool_depth pool count mismatch (actual vs golden)"
    );
    assert_eq!(
        token_prices.len(),
        expected.token_prices,
        "token_prices count mismatch (actual vs golden)"
    );
}
