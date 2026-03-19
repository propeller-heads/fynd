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
    recording::MarketRecording,
    types::{Order, OrderSide, QuoteOptions, QuoteRequest},
    worker_pool::pool::WorkerPoolBuilder,
    worker_pool_router::{SolverPoolHandle, WorkerPoolRouter},
    WorkerPoolRouterConfig,
};
use num_bigint::BigUint;
use serde::Serialize;
use tokio::sync::{broadcast, RwLock};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
use tycho_simulation::{
    tycho_common::models::{Address, Chain},
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

#[derive(Serialize)]
pub struct GoldenFile {
    pub metadata: GoldenMetadata,
    pub scenarios: Vec<GoldenScenario>,
}

#[derive(Serialize)]
pub struct GoldenMetadata {
    pub block_number: u64,
    pub num_pools: usize,
    pub num_tokens: usize,
    pub fynd_version: String,
}

#[derive(Serialize)]
pub struct GoldenScenario {
    pub name: String,
    pub token_in: String,
    pub token_out: String,
    pub amount: String,
    pub side: String,
    pub expected: GoldenOutput,
}

#[derive(Serialize)]
pub struct GoldenOutput {
    pub status: String,
    pub amount_out_net_gas: String,
    pub gas_estimate: String,
    pub num_swaps: usize,
    pub solve_time_ms: u64,
}

struct Scenario {
    name: String,
    token_in: Address,
    token_out: Address,
    amount: BigUint,
}

/// Generate golden outputs by replaying a recording through the full pipeline.
pub async fn generate_golden_outputs(recording: MarketRecording) -> anyhow::Result<GoldenFile> {
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
    // Keep a receiver alive so handle_tycho_message's broadcast doesn't fail
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

    // 2. Inject a synthetic gas price so token_prices can compute.
    // In replay mode there's no RPC to fetch live gas prices. We use a
    // realistic value (10 gwei) which is close enough for golden baseline
    // generation — the exact gas price only affects gas cost deductions.
    {
        let mut market = market_data.write().await;
        market.update_gas_price(BlockGasPrice {
            block_number: 0,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy {
                gas_price: BigUint::from(10_000_000_000u64), // 10 gwei
            },
        });
    }

    // 3. Compute derived data
    let gas_token = find_weth_address(&market_data).await;
    let config = ComputationManagerConfig::default().with_gas_token(gas_token);
    let (computation_manager, derived_events_rx) =
        ComputationManager::new(config, market_data.clone())?;
    let derived_data = computation_manager.store();

    let (market_tx, market_rx) = broadcast::channel(64);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let _cm_handle = tokio::spawn(computation_manager.run(market_rx, shutdown_rx));

    // 3. Build worker pool + router BEFORE sending MarketUpdated, so workers
    // are subscribed to derived_events_rx and receive DerivedDataEvent broadcasts.
    let market_event_rx = market_tx.subscribe();
    let algo_config = AlgorithmConfig::new(1, 3, Duration::from_millis(2000), None)
        .expect("valid algorithm config");
    let (_worker_pool, task_handle) = WorkerPoolBuilder::new()
        .algorithm("most_liquid")
        .algorithm_config(algo_config)
        .num_workers(4)
        .build(market_data.clone(), derived_data.clone(), market_event_rx, derived_events_rx)?;
    let pool_handle = SolverPoolHandle::new("golden_pool", task_handle);

    let router_config = WorkerPoolRouterConfig::default()
        .with_timeout(Duration::from_millis(5000));
    let registry = SwapEncoderRegistry::new(Chain::Ethereum)
        .add_default_encoders(None)?;
    let encoder = Encoder::new(Chain::Ethereum, registry)?;
    let router = WorkerPoolRouter::new(vec![pool_handle], router_config, encoder);

    // 4. Now trigger derived data computation by sending MarketUpdated
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

    // Wait for all derived data (spot prices, pool depths, token prices)
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

    // 5. Load scenarios from pairs.json and run them
    let scenarios = load_scenarios();
    let mut golden_scenarios = Vec::new();

    for scenario in &scenarios {
        let order = Order::new(
            scenario.token_in.clone(),
            scenario.token_out.clone(),
            scenario.amount.clone(),
            OrderSide::Sell,
            Address::zero(20),
        );
        let request = QuoteRequest::new(vec![order], QuoteOptions::default());
        let result = router.quote(request).await;

        let expected = match result {
            Ok(quote) => {
                let oq = &quote.orders()[0];
                GoldenOutput {
                    status: format!("{:?}", oq.status()),
                    amount_out_net_gas: oq.amount_out_net_gas().to_string(),
                    gas_estimate: oq.gas_estimate().to_string(),
                    num_swaps: oq.route().map(|r| r.hop_count()).unwrap_or(0),
                    solve_time_ms: quote.solve_time_ms(),
                }
            }
            Err(e) => GoldenOutput {
                status: format!("Error: {e}"),
                amount_out_net_gas: "0".to_string(),
                gas_estimate: "0".to_string(),
                num_swaps: 0,
                solve_time_ms: 0,
            },
        };

        let status_str = &expected.status;
        tracing::info!(name = %scenario.name, status = %status_str, "scenario complete");

        golden_scenarios.push(GoldenScenario {
            name: scenario.name.clone(),
            token_in: format!("{}", scenario.token_in),
            token_out: format!("{}", scenario.token_out),
            amount: scenario.amount.to_string(),
            side: "Sell".to_string(),
            expected,
        });
    }

    let successful = golden_scenarios
        .iter()
        .filter(|s| s.expected.status.contains("Success"))
        .count();
    tracing::info!(
        total = golden_scenarios.len(),
        successful,
        "golden output generation complete"
    );

    drop(shutdown_tx);

    Ok(GoldenFile {
        metadata: GoldenMetadata {
            block_number: 0,
            num_pools,
            num_tokens,
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        scenarios: golden_scenarios,
    })
}

async fn find_weth_address(market_data: &SharedMarketDataRef) -> Address {
    let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .expect("invalid WETH address");
    let market = market_data.read().await;
    if market.token_registry_ref().contains_key(&weth) {
        return weth;
    }
    Address::zero(20)
}

fn load_scenarios() -> Vec<Scenario> {
    let content = include_str!("../../../tools/benchmark/src/pairs.json");
    let raw: serde_json::Value =
        serde_json::from_str(content).expect("failed to parse pairs.json");

    let tokens: std::collections::HashMap<String, (Address, u32)> = raw["tokens"]
        .as_array()
        .expect("tokens array")
        .iter()
        .map(|t| {
            let symbol = t["symbol"].as_str().expect("symbol").to_string();
            let address: Address = t["address"].as_str().expect("address").parse().expect("addr");
            let decimals = t["decimals"].as_u64().expect("decimals") as u32;
            (symbol, (address, decimals))
        })
        .collect();

    raw["pairs"]
        .as_array()
        .expect("pairs array")
        .iter()
        .map(|pair| {
            let in_sym = pair["token_in"].as_str().expect("token_in");
            let out_sym = pair["token_out"].as_str().expect("token_out");
            let (token_in, decimals) = &tokens[in_sym];
            let (token_out, _) = &tokens[out_sym];

            let human_amount = pair["amounts"][0].as_f64().expect("amount");
            let raw_amount = human_amount * 10_f64.powi(*decimals as i32);

            Scenario {
                name: format!("{in_sym}_to_{out_sym}_{human_amount}"),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                amount: BigUint::from(raw_amount as u128),
            }
        })
        .collect()
}
