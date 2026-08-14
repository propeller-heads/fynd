use fynd_test_fixtures::expected::load_expected_file;

use crate::harness::TestHarness;

fn expected_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected_outputs.json")
}

/// All derived data fields should be computed after pipeline initialization.
#[tokio::test]
async fn test_all_derived_fields_computed() {
    let harness = TestHarness::from_fixture().await;
    let derived_ref = harness.solver().derived_data();
    let derived = derived_ref.read().await;

    assert!(derived.spot_prices().is_some(), "spot_prices should be computed");
    assert!(derived.component_depths().is_some(), "component_depths should be computed");
    assert!(derived.token_prices().is_some(), "token_prices should be computed");
}

/// Derived data metrics should exactly match the expected baseline.
/// Since replay is deterministic (same recording + same code = same result),
/// any deviation indicates a real bug, not expected variance.
#[tokio::test]
async fn test_derived_data_matches_expected() {
    let harness = TestHarness::from_fixture().await;
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let expected = expected_file
        .metadata
        .derived_data
        .expect("expected file missing derived_data metrics — regenerate with record-market");

    let market_ref = harness.solver().market_data();
    let market = market_ref.read().await;
    let derived_ref = harness.solver().derived_data();
    let derived = derived_ref.read().await;

    let spot_prices = derived
        .spot_prices()
        .expect("spot prices not computed");
    let actual_components_with_spot_prices: std::collections::HashSet<_> = spot_prices
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();

    let component_depths = derived
        .component_depths()
        .expect("component depths not computed");
    let actual_components_with_depths: std::collections::HashSet<_> = component_depths
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();

    let token_prices = derived
        .token_prices()
        .expect("token prices not computed");

    assert_eq!(
        market.component_topology().len(),
        expected_file.metadata.num_components,
        "component count mismatch"
    );
    assert_eq!(
        market.token_registry_ref().len(),
        expected_file.metadata.num_tokens,
        "token count mismatch"
    );
    assert_eq!(
        actual_components_with_spot_prices.len(),
        expected.components_with_spot_prices,
        "spot-price component count mismatch"
    );
    assert_eq!(
        actual_components_with_depths.len(),
        expected.components_with_depths,
        "component-depth component count mismatch"
    );
    assert_eq!(token_prices.len(), expected.token_prices, "token_prices count mismatch");
}
