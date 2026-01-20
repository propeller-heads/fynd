//! Tycho Router - Main entry point.
//!
//! This binary starts the Tycho Router service with:
//! - HTTP API server (Actix Web)
//! - Worker pool for solving
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
    task_queue::{TaskQueue, TaskQueueConfig},
    worker::WorkerConfig,
    worker_pool::{WorkerPoolBuilder, WorkerPoolConfig},
    TychoFeed, TychoFeedConfig,
};

/// Application configuration.
///
/// TODO: Load from environment variables or config file.
struct Config {
    chain: Chain,
    http_host: String,
    http_port: u16,
    num_workers: usize,
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
            num_workers: num_cpus::get(),
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
        workers = config.num_workers,
        "starting tycho router"
    );

    // Create shared market data
    let market_data = Arc::new(RwLock::new(SharedMarketData::new()));

    // Create task queue
    let task_queue = TaskQueue::new(TaskQueueConfig { capacity: config.task_queue_capacity });
    let (task_handle, task_rx) = task_queue.split();

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

    // Create worker pool
    let worker_config = WorkerConfig::default();
    let worker_pool_config = WorkerPoolConfig { num_workers: config.num_workers, worker_config };

    let worker_pool = WorkerPoolBuilder::new()
        .num_workers(worker_pool_config.num_workers)
        .worker_config(worker_pool_config.worker_config)
        .build(task_rx, Arc::clone(&market_data), event_rx);

    info!(num_workers = worker_pool.num_workers(), "worker pool started");

    // Start Tycho feed in background
    let feed_handle = tokio::spawn(async move {
        if let Err(e) = tycho_feed.run().await {
            tracing::error!(error = %e, "tycho feed error");
        }
    });

    // Create app state
    let app_state = AppState::new(task_handle, health_tracker);

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
    worker_pool.shutdown();

    info!("shutdown complete");
    Ok(())
}
