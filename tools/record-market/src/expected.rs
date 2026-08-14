use std::time::Duration;

use fynd_core::{QuoteOptions, QuoteRequest, QuoteStatus, Solver};
use fynd_test_fixtures::{
    DerivedDataMetrics, ExpectedFile, ExpectedMetadata, ExpectedOutput, ExpectedScenario,
    MarketRecording,
};
use num_bigint::BigUint;

/// Generate expected outputs by replaying a recording through the full pipeline.
///
/// The chain is taken from the recording's metadata so expected outputs can never
/// disagree with the fixture they were generated from.
pub async fn generate_expected_outputs(
    recording: MarketRecording,
    pools_toml: &str,
    pairs_json: &str,
) -> anyhow::Result<ExpectedFile> {
    let chain = fynd_core::types::parse_chain(&recording.metadata.chain)
        .map_err(|e| anyhow::anyhow!("recording has unsupported chain: {e}"))?;
    let gas_price = recording
        .metadata
        .gas_price_as_biguint();
    let pools = fynd_test_fixtures::parse_pools_toml(pools_toml)?;

    let solver = Solver::from_recording(chain, recording.updates, pools, gas_price)
        .await
        .map_err(|e| anyhow::anyhow!("failed to build solver from recording: {e}"))?;

    solver
        .wait_until_ready(Duration::from_secs(120))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    tracing::info!("pipeline ready, running scenarios...");

    let scenarios = fynd_test_fixtures::load_test_scenarios(pairs_json)?;
    let mut expected_scenarios = Vec::new();

    for scenario in &scenarios {
        let order = scenario.to_order();
        let request = QuoteRequest::new(vec![order], QuoteOptions::default());
        let result = solver.quote(request).await;

        let expected = match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                ExpectedOutput {
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
            Err(_e) => ExpectedOutput {
                status: QuoteStatus::NoRouteFound,
                amount_out_net_gas: BigUint::ZERO,
                gas_estimate: BigUint::ZERO,
                num_swaps: 0,
                solve_time_ms: 0,
            },
        };

        tracing::info!(
            name = %scenario.name,
            status = ?expected.status,
            "scenario complete"
        );

        expected_scenarios.push(ExpectedScenario { scenario: scenario.clone(), expected });
    }

    let successful = expected_scenarios
        .iter()
        .filter(|s| s.expected.status == QuoteStatus::Success)
        .count();
    tracing::info!(total = expected_scenarios.len(), successful, "generation complete");

    // Capture derived data metrics
    let derived_metrics = {
        let derived_ref = solver.derived_data();
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
        DerivedDataMetrics { components_with_spot_prices, components_with_depths, token_prices }
    };

    let market_ref = solver.market_data();
    let market = market_ref.read().await;
    // Single source of truth for the block number: the replayed market state,
    // matching what Solver::from_recording injects into the gas price.
    let block_number = market
        .last_updated()
        .map(|block| block.number())
        .unwrap_or(0);
    let num_components = market.component_topology().len();
    let num_tokens = market.token_registry_ref().len();
    drop(market);

    solver.shutdown();

    Ok(ExpectedFile {
        metadata: ExpectedMetadata {
            block_number,
            num_components,
            num_tokens,
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
            derived_data: Some(derived_metrics),
        },
        scenarios: expected_scenarios,
    })
}
