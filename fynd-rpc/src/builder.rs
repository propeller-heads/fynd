use std::{collections::HashMap, sync::Arc, time::Duration};

use actix_web::{dev::ServerHandle, App, HttpServer};
use anyhow::{Context, Result};
use fynd_core::{encoding::encoder::Encoder, worker_pool::pool::WorkerPool, FyndBuilder};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use tycho_simulation::tycho_common::models::Chain;

use crate::{
    api::{configure_app, AppState, HealthTracker},
    config::{defaults, BlacklistConfig, PoolConfig},
};

/// Builder that assembles Fynd and returns a running server handle.
///
/// Wraps [`FyndBuilder`] for all solver configuration and adds HTTP server concerns on top.
#[must_use]
pub struct FyndRPCBuilder {
    fynd_builder: FyndBuilder,
    http_host: String,
    http_port: u16,
    /// Gas price staleness threshold. Health returns 503 when exceeded. Disabled by default.
    gas_price_stale_threshold: Option<Duration>,
}

impl FyndRPCBuilder {
    /// Creates a new builder with required fields.
    ///
    /// All solver configuration options have sensible defaults and can be overridden via the
    /// setter methods below.
    pub fn new(
        chain: Chain,
        pools: HashMap<String, PoolConfig>,
        tycho_url: String,
        rpc_url: String,
        protocols: Vec<String>,
    ) -> Self {
        // Override FyndBuilder's generous 10 s standalone router timeout with the tighter
        // HTTP service default; callers can still override via worker_router_timeout().
        let fynd_builder = pools
            .iter()
            .fold(
                FyndBuilder::new(chain, tycho_url, rpc_url, protocols, defaults::MIN_TVL),
                |sb, (name, cfg)| sb.add_pool(name, cfg),
            )
            .worker_router_timeout(Duration::from_millis(defaults::WORKER_ROUTER_TIMEOUT_MS));
        Self {
            fynd_builder,
            http_host: defaults::HTTP_HOST.to_owned(),
            http_port: defaults::HTTP_PORT,
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
        self.fynd_builder = self.fynd_builder.min_tvl(min_tvl);
        self
    }

    /// Sets the minimum token quality filter (default: 100).
    pub fn min_token_quality(mut self, quality: i32) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .min_token_quality(quality);
        self
    }

    /// Sets the traded_n_days_ago used to filter tokens (default: 3).
    pub fn traded_n_days_ago(mut self, days: u64) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .traded_n_days_ago(days);
        self
    }

    /// Sets the ratio used to define the lower bound of the TVL filter for hysteresis (default:
    /// 1.1).
    pub fn tvl_buffer_ratio(mut self, ratio: f64) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .tvl_buffer_ratio(ratio);
        self
    }

    /// Sets the gas price refresh interval (default: 30 seconds).
    pub fn gas_refresh_interval(mut self, interval: Duration) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .gas_refresh_interval(interval);
        self
    }

    /// Sets the reconnect delay on connection failure (default: 5 seconds).
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.fynd_builder = self.fynd_builder.reconnect_delay(delay);
        self
    }

    /// Sets the worker router timeout (default: 100ms).
    pub fn worker_router_timeout(mut self, timeout: Duration) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .worker_router_timeout(timeout);
        self
    }

    /// Sets the minimum number of solver responses before early return (default: 0, wait for all).
    pub fn worker_router_min_responses(mut self, min: usize) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .worker_router_min_responses(min);
        self
    }

    /// Sets the Tycho API key.
    pub fn tycho_api_key(mut self, key: String) -> Self {
        self.fynd_builder = self.fynd_builder.tycho_api_key(key);
        self
    }

    /// Disables TLS for the Tycho WebSocket connection (TLS is enabled by default).
    pub fn disable_tls(mut self) -> Self {
        self.fynd_builder = self.fynd_builder.tycho_use_tls(false);
        self
    }

    /// Sets the blacklist configuration for filtering components.
    pub fn blacklist(mut self, blacklist: BlacklistConfig) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .blacklisted_components(blacklist.into_components());
        self
    }

    /// Overrides the default encoder with a custom one.
    pub fn encoder(mut self, encoder: Encoder) -> Self {
        self.fynd_builder = self.fynd_builder.encoder(encoder);
        self
    }

    /// Sets the gas price staleness threshold. Health returns 503 when exceeded.
    pub fn gas_price_stale_threshold(mut self, threshold: Option<Duration>) -> Self {
        self.gas_price_stale_threshold = threshold;
        self
    }

    pub fn build(self) -> Result<FyndRPC> {
        info!(
            host = %self.http_host,
            port = self.http_port,
            "starting fynd"
        );

        #[cfg(feature = "experimental")]
        let chain = self.fynd_builder.chain();

        let parts = self
            .fynd_builder
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .into_parts();

        for pool in parts.worker_pools() {
            info!(
                name = %pool.name(),
                algorithm = %pool.algorithm(),
                num_workers = pool.num_workers(),
                "worker pool started"
            );
        }

        let health_tracker =
            HealthTracker::new(Arc::clone(parts.market_data()), Arc::clone(parts.derived_data()))
                .with_gas_price_stale_threshold(self.gas_price_stale_threshold);

        #[cfg(feature = "experimental")]
        let gas_token = {
            use fynd_core::types::constants::native_token;
            native_token(&chain).context("gas token not configured for chain")?
        };

        let (
            router,
            worker_pools,
            _market_data,
            _derived_data,
            feed_handle,
            gas_price_handle,
            computation_handle,
            computation_shutdown_tx,
        ) = parts.into_components();

        let app_state = AppState::new(
            router,
            health_tracker,
            #[cfg(feature = "experimental")]
            Arc::clone(&_derived_data),
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

        Ok(FyndRPC {
            server_handle,
            server_task,
            worker_pools,
            feed_handle,
            gas_price_worker_handle: gas_price_handle,
            computation_manager_handle: computation_handle,
            computation_shutdown_tx,
        })
    }
}

/// Running Fynd RPC server. Call `run` to block until shutdown and perform cleanup.
#[must_use]
pub struct FyndRPC {
    server_handle: ServerHandle,
    server_task: JoinHandle<()>,
    worker_pools: Vec<WorkerPool>,
    feed_handle: JoinHandle<()>,
    gas_price_worker_handle: JoinHandle<()>,
    computation_manager_handle: JoinHandle<()>,
    computation_shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

impl FyndRPC {
    /// Returns a handle to the HTTP server for graceful shutdown.
    pub fn server_handle(&self) -> ServerHandle {
        self.server_handle.clone()
    }

    /// Runs the solver until shutdown. Performs cleanup on exit.
    pub async fn run(self) -> std::io::Result<()> {
        let FyndRPC {
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
