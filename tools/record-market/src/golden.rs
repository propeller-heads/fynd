use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fynd_core::{
    algorithm::AlgorithmConfig,
    derived::{ComputationManager, ComputationManagerConfig},
    encoding::encoder::Encoder,
    feed::{
        market_data::{SharedMarketData, SharedMarketDataRef},
        tycho_feed::TychoFeed,
        MarketEvent, TychoFeedConfig,
    },
    recording::{
        GoldenFile, GoldenMetadata, GoldenOutput, GoldenScenario, MarketRecording,
    },
    types::{constants::native_token, QuoteOptions, QuoteRequest, QuoteStatus},
    worker_pool::pool::WorkerPoolBuilder,
    worker_pool_router::{SolverPoolHandle, WorkerPoolRouter},
    WorkerPoolRouterConfig,
};
use num_bigint::BigUint;
use serde::Deserialize;
use tokio::sync::{broadcast, RwLock};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
use tycho_simulation::{
    tycho_common::models::Chain,
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

/// Minimal struct for parsing pool config from worker_pools.toml.
#[derive(Debug, Deserialize)]
struct PoolsFile {
    pools: HashMap<String, PoolEntry>,
}

#[derive(Debug, Deserialize)]
struct PoolEntry {
    algorithm: String,
    #[serde(default = "default_num_workers")]
    num_workers: usize,
    #[serde(default = "default_min_hops")]
    min_hops: usize,
    #[serde(default = "default_max_hops")]
    max_hops: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    max_routes: Option<usize>,
}

fn default_num_workers() -> usize { num_cpus::get() }
fn default_min_hops() -> usize { 1 }
fn default_max_hops() -> usize { 3 }
fn default_timeout_ms() -> u64 { 100 }

/// Generate golden outputs by replaying a recording through the full pipeline.
pub async fn generate_golden_outputs(recording: MarketRecording) -> anyhow::Result<GoldenFile> {
    let block_number = recording.last_block_number();

    // Determine gas price: use recorded value or fall back to 10 gwei
    let gas_price_gwei = recording.metadata.gas_price_gwei.unwrap_or(10.0);
    let gas_price_wei = (gas_price_gwei * 1e9) as u64;

    // 1. Replay recording through TychoFeed
    let market_data: SharedMarketDataRef = Arc::new(RwLock::new(SharedMarketData::new()));
    let feed_config = TychoFeedConfig::new(
        "ws://replay".to_string(),
        Chain::Ethereum,
        None,
        false,
        vec![],
        0.0,
    );
    let feed = TychoFeed::new(feed_config, market_data.clone());
    let _feed_rx = feed.subscribe();

    let num_updates = recording.updates.len();
    for (i, recorded_update) in recording.updates.into_iter().enumerate() {
        if i % 10 == 0 {
            tracing::debug!("replaying update {i}/{num_updates}");
        }
        let update = recorded_update.into();
        feed.handle_tycho_message(update).await.map_err(|e| {
            anyhow::anyhow!("replay failed at update {i}: {e}")
        })?;
    }

    // 2. Inject gas price (recorded from RPC, or default 10 gwei)
    {
        let mut market = market_data.write().await;
        market.update_gas_price(BlockGasPrice {
            block_number,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy {
                gas_price: BigUint::from(gas_price_wei),
            },
        });
    }

    // 3. Create ComputationManager
    let gas_token = native_token(&Chain::Ethereum)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let config = ComputationManagerConfig::default().with_gas_token(gas_token);
    let (computation_manager, _derived_events_rx) =
        ComputationManager::new(config, market_data.clone())?;
    let derived_data = computation_manager.store();
    let derived_event_tx = computation_manager.event_sender();

    let (market_tx, market_rx) = broadcast::channel(64);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let _cm_handle = tokio::spawn(computation_manager.run(market_rx, shutdown_rx));

    // 4. Build worker pools from worker_pools.toml BEFORE sending MarketUpdated
    let pools_toml = include_str!("../../../worker_pools.toml");
    let pools_config: PoolsFile = toml::from_str(pools_toml)?;

    let mut solver_pool_handles = Vec::new();
    let mut worker_pools = Vec::new();
    let mut max_timeout_ms = 0u64;

    for (name, pool_entry) in &pools_config.pools {
        let algo_config = AlgorithmConfig::new(
            pool_entry.min_hops,
            pool_entry.max_hops,
            Duration::from_millis(pool_entry.timeout_ms),
            pool_entry.max_routes,
        )
        .map_err(|e| anyhow::anyhow!("invalid config for pool '{name}': {e}"))?;

        let market_event_rx = market_tx.subscribe();
        let derived_event_rx = derived_event_tx.subscribe();

        let (worker_pool, task_handle) = WorkerPoolBuilder::new()
            .name(name.clone())
            .algorithm(pool_entry.algorithm.clone())
            .algorithm_config(algo_config)
            .num_workers(pool_entry.num_workers)
            .build(
                market_data.clone(),
                derived_data.clone(),
                market_event_rx,
                derived_event_rx,
            )?;

        tracing::info!(
            pool = %name,
            algorithm = %pool_entry.algorithm,
            workers = pool_entry.num_workers,
            hops = format!("{}-{}", pool_entry.min_hops, pool_entry.max_hops),
            timeout_ms = pool_entry.timeout_ms,
            "worker pool started"
        );

        solver_pool_handles.push(SolverPoolHandle::new(worker_pool.name(), task_handle));
        max_timeout_ms = max_timeout_ms.max(pool_entry.timeout_ms);
        worker_pools.push(worker_pool);
    }

    let router_config = WorkerPoolRouterConfig::default()
        .with_timeout(Duration::from_millis(max_timeout_ms.max(5000)))
        .with_min_responses(0);
    let registry = SwapEncoderRegistry::new(Chain::Ethereum)
        .add_default_encoders(None)?;
    let encoder = Encoder::new(Chain::Ethereum, registry)?;
    let router = WorkerPoolRouter::new(solver_pool_handles, router_config, encoder);

    // 5. Trigger derived data computation
    let market_read = market_data.read().await;
    let added = market_read.component_topology();
    let num_pools = added.len();
    let num_tokens = market_read.token_registry_ref().len();
    drop(market_read);

    market_tx.send(MarketEvent::MarketUpdated {
        added_components: added,
        removed_components: vec![],
        updated_components: vec![],
    })?;

    // Wait for all derived data
    let timeout = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => {
                anyhow::bail!("derived data computation timed out after 120s");
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                let d = derived_data.read().await;
                if d.spot_prices().is_some() && d.token_prices().is_some() {
                    break;
                }
            }
        }
    }

    tracing::info!(num_pools, num_tokens, "pipeline ready, running scenarios...");

    // 6. Load scenarios and run them
    let scenarios = fynd_core::recording::golden::load_test_scenarios();
    let mut golden_scenarios = Vec::new();

    for scenario in &scenarios {
        let order = scenario.to_order();
        let request = QuoteRequest::new(vec![order], QuoteOptions::default());
        let result = router.quote(request).await;

        let expected = match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                GoldenOutput {
                    status: oq.status(),
                    amount_out_net_gas: oq.amount_out_net_gas().clone(),
                    gas_estimate: oq.gas_estimate().clone(),
                    num_swaps: oq.route().map(|r| r.hop_count()).unwrap_or(0),
                    solve_time_ms: quote.solve_time_ms(),
                }
            }
            Err(_e) => GoldenOutput {
                status: QuoteStatus::NoRouteFound,
                amount_out_net_gas: BigUint::ZERO,
                gas_estimate: BigUint::ZERO,
                num_swaps: 0,
                solve_time_ms: 0,
            },
        };

        let status_str = format!("{:?}", expected.status);
        tracing::info!(name = %scenario.name, status = %status_str, "scenario complete");

        golden_scenarios.push(GoldenScenario {
            scenario: scenario.clone(),
            expected,
        });
    }

    let successful = golden_scenarios
        .iter()
        .filter(|s| s.expected.status == QuoteStatus::Success)
        .count();
    tracing::info!(
        total = golden_scenarios.len(),
        successful,
        "golden output generation complete"
    );

    drop(shutdown_tx);
    for pool in worker_pools {
        pool.shutdown();
    }

    Ok(GoldenFile {
        metadata: GoldenMetadata {
            block_number,
            num_pools,
            num_tokens,
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        scenarios: golden_scenarios,
    })
}
