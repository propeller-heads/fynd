use fynd_core::types::QuoteStatus;
use fynd_test_fixtures::expected::load_expected_file;

use crate::harness::TestHarness;

fn expected_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected_outputs.json")
}

/// Scenarios that succeeded in expected baseline should also succeed in replay.
#[tokio::test]
async fn test_all_expected_pairs_return_solutions() {
    let harness = TestHarness::from_fixture().await;
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let expected_map: std::collections::HashMap<_, _> = expected_file
        .scenarios
        .iter()
        .map(|es| (es.scenario.name.clone(), &es.expected))
        .collect();

    let scenarios = harness.scenarios();
    let mut failures = Vec::new();
    for scenario in &scenarios {
        let Some(expected) = expected_map.get(&scenario.name) else {
            continue;
        };
        if expected.status != QuoteStatus::Success {
            continue;
        }

        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                if oq.status() != QuoteStatus::Success {
                    failures.push(format!(
                        "{}: expected Success, got {:?}",
                        scenario.name,
                        oq.status()
                    ));
                }
            }
            Err(e) => {
                failures.push(format!("{}: solver error: {}", scenario.name, e));
            }
        }
    }

    assert!(failures.is_empty(), "solution availability failures:\n{}", failures.join("\n"));
}

/// Unknown tokens should return an error, not panic.
#[tokio::test]
async fn test_unknown_token_returns_error() {
    let harness = TestHarness::from_fixture().await;

    let fake_token: tycho_simulation::tycho_common::models::Address =
        "0x0000000000000000000000000000000000000BAD"
            .parse()
            .unwrap();
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let known_token = expected_file.scenarios[0]
        .scenario
        .token_in
        .clone();

    let order = fynd_core::types::Order::new(
        fake_token,
        known_token,
        num_bigint::BigUint::from(1_000_000_000_000_000_000u64),
        fynd_core::types::OrderSide::Sell,
        tycho_simulation::tycho_common::models::Address::zero(20),
    );

    let result = harness.quote(vec![order]).await;
    match result {
        Ok(quote) => {
            assert_ne!(
                quote.orders()[0].status(),
                QuoteStatus::Success,
                "unknown token should not produce a successful quote"
            );
        }
        Err(_) => { /* Expected */ }
    }
}

/// Quality: each pair's amount_out_net_gas should be within 1% of expected baseline.
#[tokio::test]
async fn test_quality_within_expected_baseline() {
    let harness = TestHarness::from_fixture().await;
    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let expected_map: std::collections::HashMap<_, _> = expected_file
        .scenarios
        .iter()
        .map(|es| (es.scenario.name.clone(), &es.expected))
        .collect();
    let scenarios = harness.scenarios();

    let mut regressions = Vec::new();
    for scenario in &scenarios {
        let Some(expected_output) = expected_map.get(&scenario.name) else {
            continue;
        };
        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        if let Ok(quote) = result {
            let oq = &quote.orders()[0];
            if oq.status() == QuoteStatus::Success {
                let actual = oq.amount_out_net_gas();
                let expected = &expected_output.amount_out_net_gas;

                // Use BigUint arithmetic to avoid f64 precision loss on
                // large wei-denominated values (>2^53).
                // Regression = actual * 100 < expected * 99 (i.e. >1% drop).
                if expected.gt(&num_bigint::BigUint::ZERO) {
                    let actual_scaled = actual * &num_bigint::BigUint::from(100u32);
                    let threshold = expected * &num_bigint::BigUint::from(99u32);

                    if actual_scaled < threshold {
                        regressions.push(format!(
                            "{}: degraded >1% (expected {}, got {})",
                            scenario.name, expected, actual,
                        ));
                    }
                }
            }
        }
    }

    assert!(
        regressions.is_empty(),
        "quality regressions (>1% degradation):\n{}",
        regressions.join("\n")
    );
}

/// Regenerate expected_outputs.json from the existing recording.
///
/// Run manually after algorithm changes:
///   cargo nextest run -p fynd-core --test integration regenerate_expected --all-features
/// --run-ignored ignored-only
#[tokio::test]
#[ignore]
async fn regenerate_expected_outputs() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = harness.scenarios();
    let mut expected_scenarios = Vec::new();

    for scenario in &scenarios {
        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        let expected = match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                fynd_test_fixtures::ExpectedOutput {
                    status: oq.status(),
                    amount_out_net_gas: oq.amount_out_net_gas().clone(),
                    gas_estimate: oq.gas_estimate().clone(),
                    num_swaps: oq
                        .route()
                        .map(|r| r.hop_count())
                        .unwrap_or(0),
                    solve_time_ms: quote.solve_time_ms(),
                }
            }
            Err(_) => fynd_test_fixtures::ExpectedOutput {
                status: QuoteStatus::NoRouteFound,
                amount_out_net_gas: num_bigint::BigUint::ZERO,
                gas_estimate: num_bigint::BigUint::ZERO,
                num_swaps: 0,
                solve_time_ms: 0,
            },
        };

        eprintln!("{}: {:?}", scenario.name, expected.status);

        expected_scenarios
            .push(fynd_test_fixtures::ExpectedScenario { scenario: scenario.clone(), expected });
    }

    let derived_metrics = {
        let derived_ref = harness.solver().derived_data();
        let d = derived_ref.read().await;
        let components_with_spot_prices = d
            .spot_prices()
            .map(|sp| {
                sp.keys()
                    .map(|(id, _, _)| id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            })
            .unwrap_or(0);
        let components_with_depths = d
            .component_depths()
            .map(|pd| {
                pd.keys()
                    .map(|(id, _, _)| id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            })
            .unwrap_or(0);
        let token_prices = d
            .token_prices()
            .map(|tp| tp.len())
            .unwrap_or(0);
        fynd_test_fixtures::DerivedDataMetrics {
            components_with_spot_prices,
            components_with_depths,
            token_prices,
        }
    };

    let market_ref = harness.solver().market_data();
    let market = market_ref.read().await;
    // Same block-number source as record-market's generate_expected_outputs:
    // the replayed market state.
    let block_number = market
        .last_updated()
        .map(|block| block.number())
        .unwrap_or(0);
    let num_components = market.component_topology().len();
    let num_tokens = market.token_registry_ref().len();
    drop(market);

    let expected_file = fynd_test_fixtures::ExpectedFile {
        metadata: fynd_test_fixtures::ExpectedMetadata {
            block_number,
            num_components,
            num_tokens,
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
            derived_data: Some(derived_metrics),
        },
        scenarios: expected_scenarios,
    };

    let json = serde_json::to_string_pretty(&expected_file).expect("serialization failed");
    std::fs::write(expected_path(), json).expect("failed to write expected_outputs.json");
    eprintln!("wrote {}", expected_path().display());
}

/// Quality invariant: all successful quotes should have positive net output.
#[tokio::test]
async fn test_quality_invariants() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = harness.scenarios();

    for scenario in &scenarios {
        let order = scenario.to_order();
        if let Ok(quote) = harness.quote(vec![order]).await {
            let oq = &quote.orders()[0];
            if oq.status() == QuoteStatus::Success {
                assert!(
                    oq.amount_out_net_gas() > &num_bigint::BigUint::ZERO,
                    "{}: amount_out_net_gas should be positive",
                    scenario.name
                );
                assert!(
                    oq.gas_estimate() > &num_bigint::BigUint::ZERO,
                    "{}: gas_estimate should be positive",
                    scenario.name
                );
                assert!(
                    oq.route().is_some(),
                    "{}: successful quote should have a route",
                    scenario.name
                );
            }
        }
    }
}
