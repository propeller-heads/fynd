use fynd_test_fixtures::expected::load_expected_file;

use crate::harness::TestHarness;

fn expected_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected_outputs.json")
}

/// Seeding the derived data a plain replay computes must produce the same quotes.
///
/// The hydrated solver skips spot prices and component depths. If the skip left the store
/// or the readiness events in a different state, the solver would not become ready, or it
/// would route on different numbers.
#[tokio::test]
async fn test_hydrated_solver_matches_plain_replay() {
    let plain = TestHarness::from_fixture().await;

    let (spot_prices, component_depths) = {
        let derived_ref = plain.solver().derived_data();
        let derived = derived_ref.read().await;
        (
            derived
                .spot_prices()
                .expect("spot prices not computed")
                .clone(),
            derived
                .component_depths()
                .expect("component depths not computed")
                .clone(),
        )
    };

    let hydrated =
        TestHarness::from_fixture_hydrated(spot_prices.clone(), component_depths.clone()).await;

    {
        let derived_ref = hydrated.solver().derived_data();
        let derived = derived_ref.read().await;
        assert_eq!(
            derived
                .spot_prices()
                .expect("seeded spot prices missing"),
            &spot_prices,
            "hydrated spot prices must stay exactly as seeded"
        );
        assert_eq!(
            derived
                .component_depths()
                .expect("seeded component depths missing"),
            &component_depths,
            "hydrated component depths must stay exactly as seeded"
        );
        assert!(
            derived.token_prices().is_some(),
            "token prices still run live and must be computed"
        );
    }

    let expected_file = load_expected_file(&expected_path())
        .expect("I/O error")
        .expect("expected_outputs.json required");
    let scenario_names: std::collections::HashSet<_> = expected_file
        .scenarios
        .iter()
        .filter(|es| es.expected.status == fynd_core::types::QuoteStatus::Success)
        .map(|es| es.scenario.name.clone())
        .collect();

    let mut compared = 0usize;
    for scenario in plain.scenarios() {
        if !scenario_names.contains(&scenario.name) {
            continue;
        }

        let plain_quote = plain
            .quote(vec![scenario.to_order()])
            .await
            .unwrap_or_else(|e| panic!("{}: plain solver error: {e}", scenario.name));
        let hydrated_quote = hydrated
            .quote(vec![scenario.to_order()])
            .await
            .unwrap_or_else(|e| panic!("{}: hydrated solver error: {e}", scenario.name));

        let plain_order = &plain_quote.orders()[0];
        let hydrated_order = &hydrated_quote.orders()[0];

        assert_eq!(
            hydrated_order.status(),
            plain_order.status(),
            "{}: status differs between plain and hydrated replay",
            scenario.name
        );
        assert_eq!(
            hydrated_order.amount_out(),
            plain_order.amount_out(),
            "{}: amount out differs between plain and hydrated replay",
            scenario.name
        );
        compared += 1;
    }

    assert!(compared > 0, "no scenarios were compared");
}
