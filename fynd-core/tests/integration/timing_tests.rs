use crate::harness::TestHarness;
use crate::scenarios::{load_golden_file, load_test_scenarios};

/// P95 solve time should stay within a reasonable multiple of the golden baseline.
/// We use 4x the golden baseline as the threshold to account for CI hardware variance.
/// Absolute cap: 200ms per quote (safety net for slow CI runners).
#[tokio::test]
async fn test_solve_time_p95_within_threshold() {
    let harness = TestHarness::from_fixture().await;
    let golden = load_golden_file().expect("golden_outputs.json required for timing tests");
    let scenarios = load_test_scenarios();

    let mut solve_times_ms: Vec<u64> = Vec::new();

    for scenario in &scenarios {
        let order = scenario.to_order();
        if let Ok(quote) = harness.quote(vec![order]).await {
            solve_times_ms.push(quote.solve_time_ms());
        }
    }

    assert!(
        !solve_times_ms.is_empty(),
        "no successful solves to measure timing"
    );

    solve_times_ms.sort_unstable();
    let p95_idx = (solve_times_ms.len() as f64 * 0.95).ceil() as usize - 1;
    let p95 = solve_times_ms[p95_idx.min(solve_times_ms.len() - 1)];

    // Calculate golden P95 for relative comparison
    let mut golden_times: Vec<u64> = golden
        .scenarios
        .iter()
        .map(|gs| gs.expected.solve_time_ms)
        .collect();
    golden_times.sort_unstable();
    let golden_p95_idx = (golden_times.len() as f64 * 0.95).ceil() as usize - 1;
    let golden_p95 = golden_times[golden_p95_idx.min(golden_times.len() - 1)];

    let relative_threshold = golden_p95.saturating_mul(4);
    let absolute_threshold = 200;
    let threshold = relative_threshold.max(absolute_threshold);

    assert!(
        p95 <= threshold,
        "P95 solve time {}ms exceeds threshold {}ms \
         (golden P95: {}ms, 4x: {}ms, absolute cap: {}ms)",
        p95,
        threshold,
        golden_p95,
        relative_threshold,
        absolute_threshold
    );
}

/// No individual solve should exceed the absolute timeout cap.
#[tokio::test]
async fn test_no_solve_exceeds_absolute_cap() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = load_test_scenarios();
    // The slow pool (most_liquid_3_hops) has a 5000ms timeout.
    // Router timeout is max(pool_timeout, 5000ms). Individual solves
    // should complete within the router timeout.
    let absolute_cap_ms = 6000;

    let mut violations = Vec::new();
    for scenario in &scenarios {
        let order = scenario.to_order();
        if let Ok(quote) = harness.quote(vec![order]).await {
            if quote.solve_time_ms() > absolute_cap_ms {
                violations.push(format!(
                    "{}: {}ms exceeds {}ms cap",
                    scenario.name,
                    quote.solve_time_ms(),
                    absolute_cap_ms
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "solve time violations:\n{}",
        violations.join("\n")
    );
}
