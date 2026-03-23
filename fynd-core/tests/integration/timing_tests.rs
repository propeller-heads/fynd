use std::collections::HashMap;

use fynd_core::recording::golden::{load_golden_file, load_test_scenarios};
use serde::Deserialize;

use crate::harness::TestHarness;

#[derive(Debug, Deserialize)]
struct PoolsFile {
    pools: HashMap<String, PoolEntry>,
}

#[derive(Debug, Deserialize)]
struct PoolEntry {
    #[serde(default)]
    timeout_ms: u64,
}

fn max_pool_timeout_ms() -> u64 {
    let pools_toml = include_str!("../../../worker_pools.toml");
    let config: PoolsFile = toml::from_str(pools_toml).expect("failed to parse worker_pools.toml");
    config
        .pools
        .values()
        .map(|p| p.timeout_ms)
        .max()
        .unwrap_or(5000)
}

/// P95 solve time should stay within a reasonable multiple of the golden baseline.
/// We use 4x the golden baseline as the threshold to account for CI hardware variance.
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

    assert!(!solve_times_ms.is_empty(), "no successful solves to measure timing");

    solve_times_ms.sort_unstable();
    let p95_idx = (solve_times_ms.len() as f64 * 0.95).ceil() as usize - 1;
    let p95 = solve_times_ms[p95_idx.min(solve_times_ms.len() - 1)];

    let mut golden_times: Vec<u64> = golden
        .scenarios
        .iter()
        .map(|gs| gs.expected.solve_time_ms)
        .collect();
    golden_times.sort_unstable();
    let golden_p95_idx = (golden_times.len() as f64 * 0.95).ceil() as usize - 1;
    let golden_p95 = golden_times[golden_p95_idx.min(golden_times.len() - 1)];

    let relative_threshold = golden_p95.saturating_mul(4);
    let absolute_threshold = max_pool_timeout_ms();
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

/// No individual solve should exceed the router timeout (max pool timeout + margin).
#[tokio::test]
async fn test_no_solve_exceeds_absolute_cap() {
    let harness = TestHarness::from_fixture().await;
    let scenarios = load_test_scenarios();
    // Router timeout = max pool timeout + 1s margin for scheduling overhead
    let absolute_cap_ms = max_pool_timeout_ms() + 1000;

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

    assert!(violations.is_empty(), "solve time violations:\n{}", violations.join("\n"));
}
