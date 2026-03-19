use crate::harness::TestHarness;
use crate::scenarios::{load_golden_file, load_test_scenarios};
use fynd_core::types::QuoteStatus;

/// All trading pairs should return successful solutions.
#[tokio::test]
async fn test_all_golden_pairs_return_solutions() {
    let harness = TestHarness::from_fixture().await;
    let golden = load_golden_file().expect("golden_outputs.json required for tests");
    // Build a lookup map: scenario name -> golden output
    let _golden_map: std::collections::HashMap<_, _> = golden
        .scenarios
        .iter()
        .map(|gs| (gs.scenario.name.clone(), gs))
        .collect();

    let scenarios = load_test_scenarios();
    let mut failures = Vec::new();
    for scenario in &scenarios {
        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        match result {
            Ok(quote) => {
                let order_quote = &quote.orders()[0];
                if order_quote.status() != QuoteStatus::Success {
                    failures.push(format!(
                        "{}: expected Success, got {:?}",
                        scenario.name,
                        order_quote.status()
                    ));
                }
            }
            Err(e) => {
                failures.push(format!("{}: solver error: {}", scenario.name, e));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "solution availability failures:\n{}",
        failures.join("\n")
    );
}

/// Unknown tokens should return an error, not panic.
#[tokio::test]
async fn test_unknown_token_returns_error() {
    let harness = TestHarness::from_fixture().await;

    let fake_token: tycho_simulation::tycho_common::models::Address =
        "0x0000000000000000000000000000000000000BAD"
            .parse()
            .unwrap();
    let golden = load_golden_file().expect("golden_outputs.json required");
    let known_token = golden.scenarios[0].scenario.token_in.clone();

    let order = fynd_core::types::Order::new(
        fake_token,
        known_token,
        num_bigint::BigUint::from(1_000_000_000_000_000_000u64),
        fynd_core::types::OrderSide::Sell,
        tycho_simulation::tycho_common::models::Address::zero(20),
    );

    let result = harness.quote(vec![order]).await;
    // Should either return an error or a quote with non-Success status.
    // It must NOT panic.
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

/// Quality: each pair's amount_out_net_gas should be within 1% of golden baseline.
#[tokio::test]
async fn test_quality_within_golden_baseline() {
    if std::env::var("BLESS_GOLDEN").is_ok() {
        // Skip quality check when blessing — we're regenerating the baseline.
        return;
    }

    let harness = TestHarness::from_fixture().await;
    let golden = load_golden_file().expect("golden_outputs.json required for quality tests");
    let golden_map: std::collections::HashMap<_, _> = golden
        .scenarios
        .iter()
        .map(|gs| (gs.scenario.name.clone(), &gs.expected))
        .collect();
    let scenarios = load_test_scenarios();

    let mut regressions = Vec::new();
    for scenario in &scenarios {
        let Some(expected_output) = golden_map.get(&scenario.name) else {
            continue; // Scenario not in golden file — skip quality check
        };
        let order = scenario.to_order();
        let result = harness.quote(vec![order]).await;

        if let Ok(quote) = result {
            let order_quote = &quote.orders()[0];
            if order_quote.status() == QuoteStatus::Success {
                let actual = order_quote.amount_out_net_gas();
                let expected = &expected_output.amount_out_net_gas;

                if expected.gt(&num_bigint::BigUint::ZERO) {
                    // Calculate percentage difference
                    let actual_f64 = actual.to_string().parse::<f64>().unwrap_or(0.0);
                    let expected_f64 = expected.to_string().parse::<f64>().unwrap_or(0.0);
                    let diff_pct = (actual_f64 - expected_f64) / expected_f64 * 100.0;

                    if diff_pct < -1.0 {
                        regressions.push(format!(
                            "{}: degraded by {:.2}% (expected {}, got {})",
                            scenario.name,
                            diff_pct.abs(),
                            expected,
                            actual
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

/// Quality invariant: all successful quotes should have positive net output.
#[tokio::test]
async fn test_quality_invariants() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = load_test_scenarios();

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
                if let Some(route) = oq.route() {
                    assert!(
                        route.hop_count() <= 3,
                        "{}: hops {} exceeds max_hops 3",
                        scenario.name,
                        route.hop_count()
                    );
                }
            }
        }
    }
}
