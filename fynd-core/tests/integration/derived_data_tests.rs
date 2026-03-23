use crate::harness::TestHarness;

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

/// Spot prices should cover the majority of pools that have simulation states.
/// Pools without states (VM-backed protocols filtered during recording) are
/// excluded from the denominator since they can never compute spot prices.
#[tokio::test]
async fn test_spot_prices_coverage() {
    let harness = TestHarness::from_fixture().await;
    let market = harness.market_data.read().await;
    let derived = harness.derived_data.read().await;

    // Count only pools that have simulation states (can compute prices)
    let pools_with_states = market
        .component_topology()
        .iter()
        .filter(|(id, _)| market.get_simulation_state(id).is_some())
        .count();

    let spot_prices = derived.spot_prices().expect("spot prices not computed");
    let pools_with_prices: std::collections::HashSet<_> = spot_prices
        .keys()
        .map(|(component_id, _, _)| component_id.clone())
        .collect();

    let coverage = pools_with_prices.len() as f64 / pools_with_states as f64;
    assert!(
        coverage >= 0.95,
        "spot price coverage {:.1}% is below 95% threshold ({} of {} pools with states)",
        coverage * 100.0,
        pools_with_prices.len(),
        pools_with_states
    );
}

/// Pool depths should cover the majority of pools that have spot prices.
#[tokio::test]
async fn test_pool_depths_coverage() {
    let harness = TestHarness::from_fixture().await;
    let derived = harness.derived_data.read().await;

    let spot_prices = derived.spot_prices().expect("spot prices not computed");
    let pool_depths = derived.pool_depths().expect("pool depths not computed");

    let pools_with_prices: std::collections::HashSet<_> = spot_prices
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();
    let pools_with_depths: std::collections::HashSet<_> = pool_depths
        .keys()
        .map(|(id, _, _)| id.clone())
        .collect();

    let coverage = pools_with_depths.len() as f64 / pools_with_prices.len() as f64;
    assert!(
        coverage >= 0.90,
        "pool depth coverage {:.1}% is below 90% threshold ({} of {} pools with spot prices)",
        coverage * 100.0,
        pools_with_depths.len(),
        pools_with_prices.len()
    );
}

/// Token gas prices should cover the majority of tokens connected to the gas token.
/// Tokens unreachable from the gas token through pools with simulation states
/// cannot have gas prices computed, so we use a lower threshold.
#[tokio::test]
async fn test_token_gas_prices_coverage() {
    let harness = TestHarness::from_fixture().await;
    let market = harness.market_data.read().await;
    let derived = harness.derived_data.read().await;

    let total_tokens = market.token_registry_ref().len();
    let token_prices = derived.token_prices().expect("token prices not computed");

    let coverage = token_prices.len() as f64 / total_tokens as f64;
    assert!(
        coverage >= 0.60,
        "token gas price coverage {:.1}% is below 60% threshold ({} of {} tokens)",
        coverage * 100.0,
        token_prices.len(),
        total_tokens
    );
}
