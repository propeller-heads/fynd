//! Tycho Router - Main entry point.
//!
//! This binary starts the Tycho Router service with:
//! - HTTP API server (Actix Web)
//! - OrderManager with multiple solver pools
//! - Tycho indexer for market data

use std::sync::Arc;

use actix_web::{App, HttpServer};
use tokio::sync::RwLock;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use tycho_simulation::tycho_common::models::Chain;
use tycho_solver::{
    api::{configure_app, AppState, HealthTracker},
    feed::market_data::SharedMarketData,
    order_manager::{OrderManager, OrderManagerConfig, SolverPoolHandle},
    task_queue::{TaskQueue, TaskQueueConfig},
    worker::WorkerConfig,
    worker_pool::{AlgorithmType, WorkerPoolBuilder},
    TychoFeed, TychoFeedConfig,
};

/// Application configuration.
///
/// TODO: Load from environment variables or config file.
struct Config {
    chain: Chain,
    http_host: String,
    http_port: u16,
    num_workers_per_pool: usize,
    task_queue_capacity: usize,
    tycho_url: String,
    tycho_api_key: String,
    rpc_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chain: Chain::Ethereum,
            http_host: "0.0.0.0".to_string(),
            http_port: 3000,
            num_workers_per_pool: num_cpus::get(),
            task_queue_capacity: 1000,
            tycho_url: "wss://tycho.propellerheads.xyz".to_string(),
            tycho_api_key: String::new(),
            rpc_url: String::new(),
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("failed to set tracing subscriber");

    // Load configuration
    let config = Config::default();

    info!(
        host = %config.http_host,
        port = config.http_port,
        workers_per_pool = config.num_workers_per_pool,
        "starting tycho router"
    );

    // Create shared market data
    let market_data = Arc::new(RwLock::new(SharedMarketData::new()));

    // Create health tracker (shared between TychoFeed and API)
    let health_tracker = HealthTracker::new();

    let tycho_feed_config = TychoFeedConfig::new(
        config.tycho_url,
        config.chain,
        config.tycho_api_key,
        vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
        10.0,
        config.rpc_url,
    );

    let (tycho_feed, event_rx) =
        TychoFeed::new(tycho_feed_config, Arc::clone(&market_data), health_tracker.clone());

    // Create worker pools - each pool has its own task queue
    // Currently we only have MostLiquid, but this is where you'd add more algorithms
    let mut solver_pool_handles = Vec::new();
    let mut worker_pools = Vec::new();

    // Pool 1: MostLiquid algorithm
    let worker_config = WorkerConfig::default();
    let task_queue = TaskQueue::new(TaskQueueConfig { capacity: config.task_queue_capacity });
    let (task_handle, task_rx) = task_queue.split();

    let worker_pool = WorkerPoolBuilder::new()
        .name("most_liquid")
        .algorithm_type(AlgorithmType::MostLiquid)
        .num_workers(config.num_workers_per_pool)
        .worker_config(worker_config)
        .build(task_rx, Arc::clone(&market_data), event_rx);

    info!(
        name = %worker_pool.name(),
        algorithm = %worker_pool.algorithm_type(),
        num_workers = worker_pool.num_workers(),
        "worker pool started"
    );

    solver_pool_handles.push(SolverPoolHandle::new(
        worker_pool.name(),
        worker_pool.algorithm_type().to_string(),
        task_handle,
    ));
    worker_pools.push(worker_pool);

    // Future: Add more worker pools here with different algorithms
    // Example:
    // let task_queue2 = TaskQueue::new(TaskQueueConfig { capacity: config.task_queue_capacity });
    // let (task_handle2, task_rx2) = task_queue2.split();
    // let worker_pool2 = WorkerPoolBuilder::new()
    //     .name("fast_heuristic")
    //     .algorithm_type(AlgorithmType::FastHeuristic)
    //     .num_workers(2)
    //     .solver_config(SolverConfig::default())
    //     .build(task_rx2, Arc::clone(&market_data), event_rx.resubscribe());
    // solver_pool_handles.push(SolverPoolHandle::new(...));
    // worker_pools.push(worker_pool2);

    // Create OrderManager with all solver pool handles
    let order_manager_config = OrderManagerConfig::default();
    let order_manager = OrderManager::new(solver_pool_handles, order_manager_config);

    info!(num_pools = order_manager.num_pools(), "order manager created");

    // Start Tycho feed in background
    let feed_handle = tokio::spawn(async move {
        if let Err(e) = tycho_feed.run().await {
            tracing::error!(error = %e, "tycho feed error");
        }
    });

    // Create app state with OrderManager
    let app_state = AppState::new(order_manager, health_tracker);

    // Start HTTP server
    let server = HttpServer::new(move || {
        App::new()
            .wrap(tracing_actix_web::TracingLogger::default())
            .configure(|cfg| configure_app(cfg, app_state.clone()))
    })
    .bind((config.http_host.as_str(), config.http_port))?
    .run();

    info!("HTTP server started");

    // Wait for shutdown signal
    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!(error = %e, "HTTP server error");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal received");
        }
    }

    // Cleanup
    info!("shutting down...");
    feed_handle.abort();
    for pool in worker_pools {
        pool.shutdown();
    }

    info!("shutdown complete");
    Ok(())
}
