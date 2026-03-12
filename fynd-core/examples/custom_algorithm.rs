//! Custom algorithm example for fynd-core
//!
//! Demonstrates how to implement the [`Algorithm`] trait for a custom type and plug it
//! into a [`WorkerPoolBuilder`] via [`WorkerPoolBuilder::with_algorithm`], without
//! modifying fynd-core itself.
//!
//! [`MyAlgorithm`] here is a thin wrapper around [`MostLiquidAlgorithm`]. In a real
//! integration you would replace the delegation with your own routing logic.
//!
//! # Prerequisites
//!
//! ```bash
//! export TYCHO_API_KEY="your-api-key"
//! export RPC_URL="https://eth.llamarpc.com"
//! export TYCHO_URL="tycho-beta.propellerheads.xyz"  # Optional
//! cargo run --package fynd-core --example custom_algorithm
//! ```

use std::{
    env,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fynd_core::{
    derived::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef},
    encoding::encoder::Encoder,
    feed::{
        gas::GasPriceFetcher,
        market_data::{SharedMarketData, SharedMarketDataRef},
        tycho_feed::TychoFeed,
        TychoFeedConfig,
    },
    types::{constants::native_token, RouteResult},
    Algorithm, AlgorithmConfig, AlgorithmError, ComputationRequirements, EncodingOptions,
    MostLiquidAlgorithm, Order, OrderManager, OrderManagerConfig, OrderSide, QuoteOptions,
    QuoteRequest, SolverPoolHandle, WorkerPoolBuilder,
};
use num_bigint::BigUint;
use tokio::sync::RwLock;
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
use tycho_simulation::{
    evm::tycho_models::Chain, tycho_core::Bytes, tycho_ethereum::rpc::EthereumRpcClient,
};

// =============================================================================
// Custom algorithm implementation
// =============================================================================

/// A custom algorithm that wraps [`MostLiquidAlgorithm`].
///
/// Replace the delegation in [`Algorithm::find_best_route`] with your own routing
/// logic to use a fully custom algorithm.
struct MyAlgorithm {
    inner: MostLiquidAlgorithm,
}

impl MyAlgorithm {
    fn new(config: AlgorithmConfig) -> Self {
        let inner =
            MostLiquidAlgorithm::with_config(config).expect("invalid algorithm configuration");
        Self { inner }
    }
}

impl Algorithm for MyAlgorithm {
    // Reuse the built-in graph type and manager so the worker infrastructure
    // (graph initialisation, event handling, edge weight updates) works unchanged.
    type GraphType = <MostLiquidAlgorithm as Algorithm>::GraphType;
    type GraphManager = <MostLiquidAlgorithm as Algorithm>::GraphManager;

    fn name(&self) -> &str {
        "my_custom_algo"
    }

    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: SharedMarketDataRef,
        derived: Option<SharedDerivedDataRef>,
        order: &fynd_core::Order,
    ) -> Result<RouteResult, AlgorithmError> {
        // Delegate to the inner algorithm.  Replace this with custom logic.
        self.inner
            .find_best_route(graph, market, derived, order)
            .await
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        self.inner.computation_requirements()
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Configuration from environment
    let tycho_url =
        env::var("TYCHO_URL").unwrap_or_else(|_| "tycho-beta.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").ok();
    let rpc_url = env::var("RPC_URL").expect("RPC_URL env var not set");
    let chain = Chain::Ethereum;

    // 2. Market data and Tycho feed
    let market_data = Arc::new(RwLock::new(SharedMarketData::new()));

    let tycho_feed_config = TychoFeedConfig::new(
        tycho_url,
        chain,
        tycho_api_key,
        true,
        vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
        10.0,
    )
    .min_token_quality(100);

    // 3. Gas price fetcher
    let ethereum_client = EthereumRpcClient::new(rpc_url.as_str())
        .map_err(|e| format!("failed to create ethereum client: {}", e))?;

    let (mut gas_price_fetcher, gas_price_worker_signal_tx) =
        GasPriceFetcher::new(ethereum_client, Arc::clone(&market_data));

    let mut tycho_feed = TychoFeed::new(tycho_feed_config, Arc::clone(&market_data));
    tycho_feed = tycho_feed.with_gas_price_worker_signal_tx(gas_price_worker_signal_tx);

    // 4. Derived data computation manager
    let gas_token = native_token(&chain).expect("gas token not configured for chain");
    let computation_config = ComputationManagerConfig::new()
        .with_gas_token(gas_token)
        .with_depth_slippage_threshold(0.01);

    let (computation_manager, _derived_event_rx) =
        ComputationManager::new(computation_config, Arc::clone(&market_data))
            .map_err(|e| format!("failed to create computation manager: {}", e))?;

    let derived_data = computation_manager.store();
    let derived_event_tx = computation_manager.event_sender();

    // 5. Event subscriptions
    let computation_event_rx = tycho_feed.subscribe();
    let pool_event_rx = tycho_feed.subscribe();
    let (computation_shutdown_tx, computation_shutdown_rx) = tokio::sync::broadcast::channel(1);

    // 6. Worker pool using the custom algorithm via with_algorithm()
    //
    // Key difference from solve_order.rs: instead of .algorithm("most_liquid"),
    // we call .with_algorithm() with a factory closure that constructs MyAlgorithm.
    let algorithm_config = AlgorithmConfig::new(1, 2, Duration::from_millis(5000), None)?;

    let (worker_pool, task_handle) = WorkerPoolBuilder::new()
        .name("custom-solver".to_string())
        .with_algorithm("my_custom_algo", MyAlgorithm::new)
        .algorithm_config(algorithm_config)
        .num_workers(2)
        .task_queue_capacity(100)
        .build(
            Arc::clone(&market_data),
            derived_data,
            pool_event_rx,
            derived_event_tx.subscribe(),
        )?;

    println!("Worker pool algorithm: {}", worker_pool.algorithm());

    // 7. OrderManager
    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .expect("Failed to create default swap encoder registry");
    let encoder = Encoder::new(chain, swap_encoder_registry).expect("Failed to create encoder");
    let order_manager = OrderManager::new(
        vec![SolverPoolHandle::new("custom-solver", task_handle)],
        OrderManagerConfig::default().with_timeout(Duration::from_secs(10)),
        encoder,
    );

    // 8. Background tasks
    let feed_handle = tokio::spawn(async move {
        if let Err(e) = tycho_feed.run().await {
            eprintln!("Tycho feed error: {:?}", e);
        }
    });
    let _gas_price_worker_handle = tokio::spawn(async move {
        if let Err(e) = gas_price_fetcher.run().await {
            eprintln!("Gas price fetcher error: {}", e);
        }
    });
    let _computation_handle = tokio::spawn(async move {
        computation_manager
            .run(computation_event_rx, computation_shutdown_rx)
            .await;
    });

    // 9. Wait for market data
    print!("Loading market data...");
    std::io::Write::flush(&mut std::io::stdout())?;

    for attempt in 1..=10 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let market = market_data.read().await;
        let age_ms = match market.last_updated() {
            Some(block_info) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                now.saturating_sub(block_info.timestamp())
                    .saturating_mul(1000)
            }
            None => u64::MAX,
        };
        drop(market);
        if age_ms < 60_000 {
            break;
        }
        if attempt == 10 {
            eprintln!("\nWarning: market data may be stale");
        }
    }

    // Wait for derived data computations to complete
    tokio::time::sleep(Duration::from_secs(60)).await;
    println!(" done");

    // 10. Solve an order: Sell 1000 USDC for WBTC
    let usdc_addr = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?;
    let wbtc_addr = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599")?;

    let order = Order::new(
        usdc_addr,
        wbtc_addr,
        BigUint::from(1_000_000_000u128),
        OrderSide::Sell,
        "0x0000000000000000000000000000000000000000".parse()?,
    )
    .with_id("custom-algo-order".to_string());

    print!("Solving: 1000 USDC → WBTC with MyAlgorithm...");
    std::io::Write::flush(&mut std::io::stdout())?;

    let options = QuoteOptions::default().with_encoding_options(EncodingOptions::new(0.01));
    let request = QuoteRequest::new(vec![order], options);
    let solution = order_manager.quote(request).await?;
    println!(" done ({}ms)\n", solution.solve_time_ms());

    // 11. Display results
    let order_quote = &solution.orders()[0];
    if let Some(route) = order_quote.route() {
        let market = market_data.read().await;
        let final_swap = route.swaps().last().unwrap();
        let final_token_out = market
            .get_token(final_swap.token_out())
            .unwrap();
        let final_amount_out = final_swap
            .amount_out()
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0) /
            10f64.powi(final_token_out.decimals as i32);
        println!("Result: {:.2} {}", final_amount_out, final_token_out.symbol);
        println!("Gas:    {}\n", route.total_gas());
        println!("Route ({} hops):", route.swaps().len());
        for (i, swap) in route.swaps().iter().enumerate() {
            let token_in = market
                .get_token(swap.token_in())
                .unwrap();
            let token_out = market
                .get_token(swap.token_out())
                .unwrap();

            let amount_in_f64 = swap
                .amount_in()
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0) /
                10f64.powi(token_in.decimals as i32);
            let amount_out_f64 = swap
                .amount_out()
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0) /
                10f64.powi(token_out.decimals as i32);
            println!(
                "  {}. {:.6} {} → {:.6} {} ({})",
                i + 1,
                amount_in_f64,
                token_in.symbol,
                amount_out_f64,
                token_out.symbol,
                swap.protocol()
            );
        }
        if let Some(tx) = order_quote.transaction() {
            let calldata: String = tx
                .data()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            println!("\nEncoded tx: to={}, calldata=0x{}", tx.to(), calldata);
        }
    } else {
        println!("No route found (status: {:?})", order_quote.status());
    }

    let _ = computation_shutdown_tx.send(());
    worker_pool.shutdown();
    feed_handle.abort();

    Ok(())
}
