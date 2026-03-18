use std::{collections::HashMap, sync::Arc, time::Duration};

use actix_web::{dev::ServerHandle, App, HttpServer};
use anyhow::{Context, Result};
use fynd_core::{encoding::encoder::Encoder, worker_pool::pool::WorkerPool, SolverBuilder};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use tycho_simulation::tycho_common::models::Chain;

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
    traded_n_days_ago: u64,
    tvl_buffer_ratio: f64,
    gas_refresh_interval: Duration,
    reconnect_delay: Duration,
    worker_router_timeout: Duration,
    worker_router_min_responses: usize,
    /// Blacklist configuration for filtering components and protocols.
    blacklist: BlacklistConfig,
    /// Custom encoder override. If `None`, a default encoder is created during build.
    encoder: Option<Encoder>,
    /// Gas price staleness threshold. Health returns 503 when exceeded. Disabled by default.
    gas_price_stale_threshold: Option<Duration>,
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
            traded_n_days_ago: defaults::TRADED_N_DAYS_AGO,
            tvl_buffer_ratio: defaults::TVL_BUFFER_RATIO,
            gas_refresh_interval: Duration::from_secs(defaults::GAS_REFRESH_INTERVAL_SECS),
            reconnect_delay: Duration::from_secs(defaults::RECONNECT_DELAY_SECS),
            worker_router_timeout: Duration::from_millis(defaults::WORKER_ROUTER_TIMEOUT_MS),
            worker_router_min_responses: defaults::WORKER_ROUTER_MIN_RESPONSES,
            blacklist: BlacklistConfig::default(),
            encoder: None,
            gas_price_stale_threshold: None,
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

    /// Sets the traded_n_days_ago used to filter tokens (default: 3).
    pub fn traded_n_days_ago(mut self, days: u64) -> Self {
        self.traded_n_days_ago = days;
        self
    }

    /// Sets the ratio used to define the lower bound of the TVL filter for hysteresis (default:
    /// 1.1).
    pub fn tvl_buffer_ratio(mut self, multiplier: f64) -> Self {
        self.tvl_buffer_ratio = multiplier;
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

    /// Sets the gas price staleness threshold. Health returns 503 when exceeded.
    pub fn gas_price_stale_threshold(mut self, threshold: Option<Duration>) -> Self {
        self.gas_price_stale_threshold = threshold;
        self
    }

    pub fn build(self) -> Result<Fynd> {
        info!(
            host = %self.http_host,
            port = self.http_port,
            pools = self.pools.len(),
            "starting fynd"
        );

        let mut solver_builder = SolverBuilder::new(
            self.chain,
            self.tycho_url,
            self.rpc_url,
            self.protocols,
            self.min_tvl,
        )
        .tycho_api_key_opt(self.tycho_api_key)
        .tycho_use_tls(self.tycho_use_tls)
        .min_token_quality(self.min_token_quality)
        .traded_n_days_ago(self.traded_n_days_ago)
        .tvl_buffer_ratio(self.tvl_buffer_ratio)
        .gas_refresh_interval(self.gas_refresh_interval)
        .reconnect_delay(self.reconnect_delay)
        .blacklisted_components(self.blacklist.components)
        .worker_router_timeout(self.worker_router_timeout)
        .worker_router_min_responses(self.worker_router_min_responses);

        if let Some(encoder) = self.encoder {
            solver_builder = solver_builder.encoder(encoder);
        }

        for (name, pool_cfg) in &self.pools {
            solver_builder = solver_builder.add_pool(name, pool_cfg);
        }

        let parts = solver_builder
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .into_parts();

        for pool in &parts.worker_pools {
            info!(
                name = %pool.name(),
                algorithm = %pool.algorithm(),
                num_workers = pool.num_workers(),
                "worker pool started"
            );
        }

        let health_tracker =
            HealthTracker::new(Arc::clone(&parts.market_data), Arc::clone(&parts.derived_data))
                .with_gas_price_stale_threshold(self.gas_price_stale_threshold);

        #[cfg(feature = "experimental")]
        let gas_token = {
            use fynd_core::types::constants::native_token;
            native_token(&self.chain).context("gas token not configured for chain")?
        };

        let app_state = AppState::new(
            parts.router,
            health_tracker,
            #[cfg(feature = "experimental")]
            Arc::clone(&parts.derived_data),
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
            worker_pools: parts.worker_pools,
            feed_handle: parts.feed_handle,
            gas_price_worker_handle: parts.gas_price_handle,
            computation_manager_handle: parts.computation_handle,
            computation_shutdown_tx: parts.computation_shutdown_tx,
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
