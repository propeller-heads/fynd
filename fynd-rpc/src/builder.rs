use std::{collections::HashMap, sync::Arc, time::Duration};

use actix_web::{dev::ServerHandle, App, HttpServer};
use anyhow::{Context, Result};
use fynd_core::{
    algorithm::AlgorithmConfig,
    derived::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef},
    encoding::encoder::Encoder,
    feed::{
        gas::GasPriceFetcher, market_data::SharedMarketData, tycho_feed::TychoFeed, TychoFeedConfig,
    },
    types::constants::native_token,
    worker_pool::pool::{WorkerPool, WorkerPoolBuilder},
    worker_pool_router::{config::WorkerPoolRouterConfig, SolverPoolHandle, WorkerPoolRouter},
};
use tokio::{sync::RwLock, task::JoinHandle};
use tracing::{error, info, warn};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
use tycho_simulation::{tycho_common::models::Chain, tycho_ethereum::rpc::EthereumRpcClient};

use crate::{
    api::{configure_app, AppState, HealthTracker},
    config::{defaults, BlacklistConfig, PoolConfig},
};
/// Builder that assembles Fynd and returns a running server handle.
///
/// The builder does the following:
/// - Creates a new Tycho feed
/// - Creates worker pools (one task queue per pool)
/// - Creates a new WorkerPoolRouter
/// - Creates a new HTTP server
/// - Returns a running server handle
pub struct FyndBuilder {
    chain: Chain,
    http_host: String,
    http_port: u16,
    pools: HashMap<String, PoolConfig>,
    tycho_url: String,
    tycho_api_key: Option<String>,
    /// Use TLS for Tycho WebSocket connection.
    tycho_use_tls: bool,
    rpc_url: String,
    protocols: Vec<String>,
    min_tvl: f64,
    min_token_quality: i32,
    tvl_buffer_multiplier: f64,
    gas_refresh_interval: Duration,
    reconnect_delay: Duration,
    worker_router_timeout: Duration,
    worker_router_min_responses: usize,
    /// Blacklist configuration for filtering components and protocols.
    blacklist: BlacklistConfig,
    /// Custom encoder override. If `None`, a default encoder is created during build.
    encoder: Option<Encoder>,
}

impl FyndBuilder {
    /// Creates a new builder with required fields.
    pub fn new(
        chain: Chain,
        pools: HashMap<String, PoolConfig>,
        tycho_url: String,
        rpc_url: String,
        protocols: Vec<String>,
    ) -> Self {
        Self {
            chain,
            http_host: defaults::HTTP_HOST.to_owned(),
            http_port: defaults::HTTP_PORT,
            pools,
            tycho_url,
            tycho_api_key: None,
            tycho_use_tls: true, // Default to TLS enabled for Tycho WebSocket connection
            rpc_url,
            protocols,
            min_tvl: defaults::MIN_TVL,
            min_token_quality: defaults::MIN_TOKEN_QUALITY,
            tvl_buffer_multiplier: defaults::TVL_BUFFER_MULTIPLIER,
            gas_refresh_interval: Duration::from_secs(defaults::GAS_REFRESH_INTERVAL_SECS),
            reconnect_delay: Duration::from_secs(defaults::RECONNECT_DELAY_SECS),
            worker_router_timeout: Duration::from_millis(defaults::WORKER_ROUTER_TIMEOUT_MS),
            worker_router_min_responses: defaults::WORKER_ROUTER_MIN_RESPONSES,
            blacklist: BlacklistConfig::default(),
            encoder: None,
        }
    }

    /// Sets the HTTP host (default: "0.0.0.0").
    pub fn http_host(mut self, host: String) -> Self {
        self.http_host = host;
        self
    }

    /// Sets the HTTP port (default: 3000).
    pub fn http_port(mut self, port: u16) -> Self {
        self.http_port = port;
        self
    }

    /// Sets the minimum TVL filter (default: 10.0).
    pub fn min_tvl(mut self, min_tvl: f64) -> Self {
        self.min_tvl = min_tvl;
        self
    }

    /// Sets the minimum token quality filter.
    pub fn min_token_quality(mut self, min_token_quality: i32) -> Self {
        self.min_token_quality = min_token_quality;
        self
    }

    /// Sets the TVL buffer multiplier (default: 1.1).
    pub fn tvl_buffer_multiplier(mut self, multiplier: f64) -> Self {
        self.tvl_buffer_multiplier = multiplier;
        self
    }

    /// Sets the gas price refresh interval (default: 30 seconds).
    pub fn gas_refresh_interval(mut self, interval: Duration) -> Self {
        self.gas_refresh_interval = interval;
        self
    }

    /// Sets the reconnect delay on connection failure (default: 5 seconds).
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// Sets the worker router timeout (default: 100ms).
    pub fn worker_router_timeout(mut self, timeout: Duration) -> Self {
        self.worker_router_timeout = timeout;
        self
    }

    /// Sets the minimum number of solver responses before early return (default: 0, wait for all).
    pub fn worker_router_min_responses(mut self, min: usize) -> Self {
        self.worker_router_min_responses = min;
        self
    }

    /// Sets the Tycho API key
    pub fn tycho_api_key(mut self, tycho_api_key: String) -> Self {
        self.tycho_api_key = Some(tycho_api_key);
        self
    }

    /// Disables TLS for Tycho WebSocket connection (TLS is enabled by default).
    pub fn disable_tls(mut self) -> Self {
        self.tycho_use_tls = false;
        self
    }

    /// Sets the blacklist configuration for filtering components.
    pub fn blacklist(mut self, blacklist: BlacklistConfig) -> Self {
        self.blacklist = blacklist;
        self
    }

    /// Overrides the default encoder with a custom one.
    pub fn encoder(mut self, encoder: Encoder) -> Self {
        self.encoder = Some(encoder);
        self
    }

    pub fn build(self) -> Result<Fynd> {
        info!(
            host = %self.http_host,
            port = self.http_port,
            pools = self.pools.len(),
            "starting fynd"
        );

        // Shared state
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));

        // Tycho feed
        let tycho_feed_config = TychoFeedConfig::new(
            self.tycho_url.clone(),
            self.chain,
            self.tycho_api_key.clone(),
            self.tycho_use_tls,
            self.protocols.clone(),
            self.min_tvl,
        )
        .tvl_buffer_multiplier(self.tvl_buffer_multiplier)
        .gas_refresh_interval(self.gas_refresh_interval)
        .reconnect_delay(self.reconnect_delay)
        .min_token_quality(self.min_token_quality)
        .blacklisted_components(self.blacklist.components);

        let ethereum_client = EthereumRpcClient::new(self.rpc_url.as_str())
            .map_err(|e| anyhow::anyhow!("failed to create ethereum client: {}", e))?;

        let (mut gas_price_fetcher, gas_price_worker_signal_tx) =
            GasPriceFetcher::new(ethereum_client, Arc::clone(&market_data));

        let mut tycho_feed = TychoFeed::new(tycho_feed_config, Arc::clone(&market_data));

        tycho_feed = tycho_feed.with_gas_price_worker_signal_tx(gas_price_worker_signal_tx);

        // Computation manager for derived data (token prices, pool depths)
        let gas_token = native_token(&self.chain).context("gas token not configured for chain")?;
        let computation_config = ComputationManagerConfig::new()
            .with_gas_token(gas_token.clone())
            .with_depth_slippage_threshold(defaults::DEPTH_SLIPPAGE_THRESHOLD);
        let (computation_manager, _derived_event_rx) =
            ComputationManager::new(computation_config, Arc::clone(&market_data))
                .map_err(|e| anyhow::anyhow!("failed to create computation manager: {}", e))?;
        let derived_data: SharedDerivedDataRef = computation_manager.store();
        let health_tracker =
            HealthTracker::new(Arc::clone(&market_data), Arc::clone(&derived_data));
        let computation_event_rx = tycho_feed.subscribe();
        let (computation_shutdown_tx, computation_shutdown_rx) = tokio::sync::broadcast::channel(1);

        // Worker pools (one task queue per pool)
        let mut solver_pool_handles = Vec::new();
        let mut worker_pools = Vec::new();

        // Get the derived event sender for workers to subscribe
        let derived_event_tx = computation_manager.event_sender();

        for (pool_name, pool_cfg) in self.pools.iter() {
            // Each pool gets its own subscription to feed events
            let pool_event_rx = tycho_feed.subscribe();

            // Convert pool's config to AlgorithmConfig
            let algorithm_config = AlgorithmConfig::new(
                pool_cfg.min_hops,
                pool_cfg.max_hops,
                Duration::from_millis(pool_cfg.timeout_ms),
                pool_cfg.max_routes,
            )
            .context(format!("invalid algorithm configuration for pool '{}'", pool_name))?;

            let (worker_pool, task_handle) = WorkerPoolBuilder::new()
                .name(pool_name.clone())
                .algorithm(pool_cfg.algorithm.clone())
                .algorithm_config(algorithm_config)
                .num_workers(pool_cfg.num_workers)
                .task_queue_capacity(pool_cfg.task_queue_capacity)
                .build(
                    Arc::clone(&market_data),
                    Arc::clone(&derived_data),
                    pool_event_rx,
                    derived_event_tx.subscribe(),
                )
                .context("failed to create worker pool")?;

            info!(
                name = %worker_pool.name(),
                algorithm = %worker_pool.algorithm(),
                num_workers = worker_pool.num_workers(),
                "worker pool started"
            );

            solver_pool_handles.push(SolverPoolHandle::new(worker_pool.name(), task_handle));
            worker_pools.push(worker_pool);
        }

        // Spawn feed after all subscriptions are created
        let feed_handle = tokio::spawn(async move {
            if let Err(e) = tycho_feed.run().await {
                error!(error = %e, "tycho feed error");
            }
        });

        // Start gas price fetcher in background
        let gas_price_worker_handle = tokio::spawn(async move {
            if let Err(e) = gas_price_fetcher.run().await {
                tracing::error!(error = %e, "gas price fetcher error");
            }
        });

        // Start computation manager in background
        let computation_manager_handle = tokio::spawn(async move {
            computation_manager
                .run(computation_event_rx, computation_shutdown_rx)
                .await;
        });

        let encoder = match self.encoder {
            Some(encoder) => encoder,
            None => {
                let swap_encoder_registry =
                    SwapEncoderRegistry::new(self.chain).add_default_encoders(None)?;
                Encoder::new(self.chain, swap_encoder_registry)
                    .map_err(|e| anyhow::anyhow!("failed to create encoder: {}", e))?
            }
        };

        let worker_router_config = WorkerPoolRouterConfig::default()
            .with_timeout(self.worker_router_timeout)
            .with_min_responses(self.worker_router_min_responses);
        let worker_router =
            WorkerPoolRouter::new(solver_pool_handles, worker_router_config, encoder);

        let app_state = AppState::new(
            worker_router,
            health_tracker,
            #[cfg(feature = "experimental")]
            Arc::clone(&derived_data),
            #[cfg(feature = "experimental")]
            gas_token,
        );

        let server = HttpServer::new(move || {
            App::new()
                .wrap(tracing_actix_web::TracingLogger::default())
                .configure(|cfg| configure_app(cfg, app_state.clone()))
        })
        .bind((self.http_host.as_str(), self.http_port))
        .context("failed to bind HTTP server")?
        .run();

        let server_handle = server.handle();
        let server_task = tokio::spawn(async move {
            if let Err(e) = server.await {
                tracing::error!(error = %e, "HTTP server error");
            }
        });

        Ok(Fynd {
            server_handle,
            server_task,
            worker_pools,
            feed_handle,
            gas_price_worker_handle,
            computation_manager_handle,
            computation_shutdown_tx,
        })
    }
}

/// Running Fynd. Call `run` to block until shutdown and perform cleanup.
pub struct Fynd {
    server_handle: ServerHandle,
    server_task: JoinHandle<()>,
    worker_pools: Vec<WorkerPool>,
    feed_handle: JoinHandle<()>,
    gas_price_worker_handle: JoinHandle<()>,
    computation_manager_handle: JoinHandle<()>,
    computation_shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

impl Fynd {
    /// Returns a handle to the HTTP server for graceful shutdown.
    pub fn server_handle(&self) -> ServerHandle {
        self.server_handle.clone()
    }

    /// Runs the solver until shutdown. Performs cleanup on exit.
    pub async fn run(self) -> std::io::Result<()> {
        let Fynd {
            server_handle,
            mut server_task,
            worker_pools,
            mut feed_handle,
            mut gas_price_worker_handle,
            mut computation_manager_handle,
            computation_shutdown_tx,
        } = self;

        info!("HTTP server started");

        // Monitor server, feed, and gas price worker. If any errors, shutdown everything.
        tokio::select! {
            server_result = &mut server_task => {
                // Server completed first
                if let Err(e) = server_result {
                    error!(error = %e, "Server task error");
                }
                info!("shutting down...");
                feed_handle.abort();
                gas_price_worker_handle.abort();
                let _ = computation_shutdown_tx.send(());
                computation_manager_handle.abort();
            }
            _ = &mut feed_handle => {
                // Feed handle completed, which means it errored (feed.run() only returns on error)
                error!("Tycho feed error detected, shutting down solver");
                server_handle.stop(true).await;
                server_task.await.ok();
                gas_price_worker_handle.abort();
                let _ = computation_shutdown_tx.send(());
                computation_manager_handle.abort();
                info!("shutting down...");
            }
            _ = &mut gas_price_worker_handle => {
                // Gas price worker completed, which means it errored
                error!("Gas price worker error detected, shutting down solver");
                server_handle.stop(true).await;
                server_task.await.ok();
                feed_handle.abort();
                let _ = computation_shutdown_tx.send(());
                computation_manager_handle.abort();
                info!("shutting down...");
            }
            _ = &mut computation_manager_handle => {
                // Computation manager completed unexpectedly
                warn!("Computation manager stopped unexpectedly");
                // Continue running - derived data won't be updated but solver can still work
            }
        }

        for pool in worker_pools {
            pool.shutdown();
        }

        info!("shutdown complete");
        Ok(())
    }
}

pub fn parse_chain(chain: &str) -> Result<Chain> {
    let candidate = format!("\"{}\"", chain.to_ascii_lowercase());
    serde_json::from_str::<Chain>(&candidate)
        .map_err(|_| anyhow::anyhow!("unsupported chain '{}'. Try values like 'Ethereum'", chain))
}
