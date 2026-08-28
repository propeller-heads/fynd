use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use actix_web::{dev::ServerHandle, App, HttpServer};
use anyhow::{Context, Result};
use fynd_core::{
    encoding::encoder::Encoder, worker_pool::pool::WorkerPool, FyndBuilder, SolverBuildError,
};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{error, info};
use tycho_simulation::tycho_common::models::{chain_config::TvlThresholdTier, Chain};
use utoipa::openapi::OpenApi;

use crate::{
    api::{configure_app, docs::OpenApiConfigurator, AppState, HealthTracker, RouteConfigurator},
    config::{defaults, PoolConfig},
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
    /// Hosted gateway URL advertised by the `/docs/hosted/` Swagger UI. Unset by default, which
    /// leaves that UI unregistered.
    hosted_swagger_url: Option<String>,
    /// Caller routes registered before the defaults; see [`Self::configure_routes`].
    route_overrides: Option<RouteConfigurator>,
    /// Rewrites the OpenAPI document before it is served. Unset by default.
    openapi_overrides: Option<OpenApiConfigurator>,
}

impl FyndRPCBuilder {
    /// Creates a new builder with required fields.
    ///
    /// All solver configuration options have sensible defaults and can be overridden via the
    /// setter methods below.
    ///
    /// # Errors
    ///
    /// Returns [`SolverBuildError`] if any worker pool's `connector_tokens` contains a malformed
    /// hex address.
    pub fn new(
        chain: Chain,
        pools: HashMap<String, PoolConfig>,
        tycho_url: String,
        rpc_url: String,
        protocols: Vec<String>,
    ) -> Result<Self, SolverBuildError> {
        // Override FyndBuilder's generous 10 s standalone router timeout with the tighter
        // HTTP service default; callers can still override via worker_router_timeout().
        let fynd_builder = pools
            .iter()
            .try_fold(
                FyndBuilder::new(
                    chain,
                    tycho_url,
                    rpc_url,
                    protocols,
                    chain.default_tvl_threshold(TvlThresholdTier::Low),
                ),
                |sb, (name, cfg)| sb.add_pool(name, cfg),
            )?
            .worker_router_timeout(Duration::from_millis(defaults::WORKER_ROUTER_TIMEOUT_MS));
        Ok(Self {
            fynd_builder,
            http_host: defaults::HTTP_HOST.to_owned(),
            http_port: defaults::HTTP_PORT,
            gas_price_stale_threshold: None,
            hosted_swagger_url: None,
            route_overrides: None,
            openapi_overrides: None,
        })
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

    /// Sets the minimum TVL filter (default: chain-specific `TvlThresholdTier::Low`).
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

    /// Sets the blocklist configuration for filtering components.
    pub fn blocklist(mut self, blocklist: HashSet<String>) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .blocklisted_components(blocklist);
        self
    }

    /// Enables partial block (flashblock) updates from the Tycho stream (default: `false`).
    ///
    /// When enabled, the stream delivers component state updates mid-block rather than only at
    /// finalization, reducing latency. Only supported for on-chain protocols; RFQ streams are
    /// unaffected.
    pub fn partial_blocks(mut self, enabled: bool) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .partial_blocks(enabled);
        self
    }

    /// Overrides the default encoder with a custom one.
    pub fn encoder(mut self, encoder: Encoder) -> Self {
        self.fynd_builder = self.fynd_builder.encoder(encoder);
        self
    }

    /// Sets a watermark appended to every encoded transaction's calldata (e.g. `"fynd"`), so
    /// on-chain observers can attribute router calls to this deployment. Default: no watermark.
    pub fn calldata_watermark(mut self, watermark: impl Into<Vec<u8>>) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .calldata_watermark(watermark);
        self
    }

    /// Sets the gas price staleness threshold. Health returns 503 when exceeded.
    pub fn gas_price_stale_threshold(mut self, threshold: Option<Duration>) -> Self {
        self.gas_price_stale_threshold = threshold;
        self
    }

    /// Sets the hosted gateway URL the `/docs/hosted/` Swagger UI sends requests to.
    ///
    /// Leaving it unset (the default) leaves that UI unregistered, so self-hosted deployments
    /// only serve `/docs/`.
    pub fn hosted_swagger_url(mut self, url: Option<String>) -> Self {
        self.hosted_swagger_url = url;
        self
    }

    /// Registers routes ahead of the built-in ones.
    ///
    /// `f` runs once per Actix worker with the shared [`AppState`], adding routes to the `/v1`
    /// scope before the defaults. Paths are relative to `/v1` (e.g. `"/quote"`) — an override
    /// cannot add or shadow a route outside `/v1`. A route it adds for a path and method that
    /// fynd also serves shadows the default; everything else (remaining endpoints, error
    /// handlers, docs, 404) is unchanged. Shadowing a default route does not change the OpenAPI
    /// spec served at `/docs/`; pair it with [`Self::configure_openapi`] to describe the
    /// replacement. Calling this more than once replaces the earlier configurator
    /// rather than combining them — the last call wins. For example:
    ///
    /// ```no_run
    /// use std::collections::HashMap;
    ///
    /// use actix_web::web;
    /// use fynd_rpc::builder::FyndRPCBuilder;
    /// use tycho_simulation::tycho_common::models::Chain;
    ///
    /// async fn my_quote() -> actix_web::HttpResponse {
    ///     actix_web::HttpResponse::Ok().finish()
    /// }
    ///
    /// let builder = FyndRPCBuilder::new(
    ///     Chain::Ethereum,
    ///     HashMap::new(),
    ///     "https://tycho.example".to_string(),
    ///     "https://rpc.example".to_string(),
    ///     vec![],
    /// )
    /// .unwrap();
    /// let builder = builder.configure_routes(|scope, _state| {
    ///     scope.route("/quote", web::post().to(my_quote))
    /// });
    /// # let _ = builder;
    /// ```
    pub fn configure_routes<F>(mut self, f: F) -> Self
    where
        F: Fn(actix_web::Scope, &AppState) -> actix_web::Scope + Send + Sync + 'static,
    {
        self.route_overrides = Some(Arc::new(f));
        self
    }

    /// Rewrites the OpenAPI document served at `/api-docs/openapi.json` (and, derived from it,
    /// the hosted variant) before it is published.
    ///
    /// Without it fynd documents the routes it serves itself. A binary that replaces one of those
    /// routes uses this hook to publish a document that matches its handler, typically by removing
    /// the replaced path and merging its own `#[derive(OpenApi)]` document with
    /// [`OpenApi::merge`](utoipa::openapi::OpenApi::merge). Calling this more than once replaces
    /// the earlier hook; the last call wins.
    pub fn configure_openapi<F>(mut self, f: F) -> Self
    where
        F: Fn(OpenApi) -> OpenApi + Send + Sync + 'static,
    {
        self.openapi_overrides = Some(Arc::new(f));
        self
    }

    /// Enables or disables the price guard.
    ///
    /// When enabled, default providers are auto-registered if none were added
    /// manually. When disabled, per-request attempts to enable the guard return an error.
    pub fn price_guard_enabled(mut self, enabled: bool) -> Self {
        self.fynd_builder = self
            .fynd_builder
            .price_guard_enabled(enabled);
        self
    }

    /// Assemble all components and return a running [`FyndRPC`] server handle.
    ///
    /// Starts all worker pools and binds the HTTP listener. Returns an error if any component
    /// fails to initialise.
    pub fn build(self) -> Result<FyndRPC> {
        info!(
            host = %self.http_host,
            port = self.http_port,
            "starting fynd"
        );

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

        let chain = parts.chain();
        let chain_id = chain.id();
        let router_address = parts.router_address().cloned();
        let permit2_address = {
            use fynd_core::encoding::encoder::PERMIT2_ADDRESS;
            let hex = PERMIT2_ADDRESS
                .strip_prefix("0x")
                .unwrap_or(PERMIT2_ADDRESS);
            hex::decode(hex)
                .context("failed to decode PERMIT2_ADDRESS")?
                .into()
        };

        let health_tracker =
            HealthTracker::new(parts.market_data().clone(), Arc::clone(parts.derived_data()))
                .with_gas_price_stale_threshold(self.gas_price_stale_threshold);

        #[cfg(feature = "experimental")]
        let gas_token = {
            use fynd_core::types::constants::native_token;
            native_token(&chain).context("gas token not configured for chain")?
        };

        // Taken before `into_components` moves the parts: that call drops the fee tier task's
        // `JoinHandle`, and this handle is what still stops the task.
        let fee_tier_abort = parts.fee_tier_abort_handle();
        let (
            router,
            worker_pools,
            _market_data,
            _derived_data,
            feed_handle,
            gas_price_handle,
            metrics_sampler_handle,
            router_fee_handle,
            computation_handle,
            computation_shutdown_tx,
        ) = parts.into_components();

        let app_state = AppState::new(
            router,
            health_tracker,
            chain_id,
            router_address,
            permit2_address,
            #[cfg(feature = "experimental")]
            Arc::clone(&_derived_data),
            #[cfg(feature = "experimental")]
            gas_token,
            #[cfg(feature = "experimental")]
            _market_data.clone(),
        );

        let hosted_swagger_url = self.hosted_swagger_url;
        let route_overrides = self.route_overrides;
        let openapi_overrides = self.openapi_overrides;
        let server = HttpServer::new(move || {
            App::new()
                .wrap(tracing_actix_web::TracingLogger::default())
                .wrap(actix_web::middleware::from_fn(
                    crate::api::middleware::http_metrics_middleware,
                ))
                .configure(|cfg| {
                    configure_app(
                        cfg,
                        app_state.clone(),
                        hosted_swagger_url.clone(),
                        route_overrides.as_ref(),
                        openapi_overrides.as_ref(),
                    )
                })
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
            metrics_sampler_handle,
            router_fee_worker_handle: router_fee_handle,
            fee_tier_abort,
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
    metrics_sampler_handle: JoinHandle<()>,
    router_fee_worker_handle: JoinHandle<()>,
    fee_tier_abort: AbortHandle,
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
            metrics_sampler_handle,
            router_fee_worker_handle,
            fee_tier_abort,
            mut computation_manager_handle,
            computation_shutdown_tx,
        } = self;

        info!("HTTP server started");

        // Set when a monitored task exits in a way that should fail the process (non-zero exit).
        let mut fatal_error: Option<std::io::Error> = None;

        // Monitor server, feed, and gas price worker. If any errors, shutdown everything.
        tokio::select! {
            server_result = &mut server_task => {
                // Server completed first
                if let Err(e) = server_result {
                    error!(error = %e, "Server task error");
                }
                info!("shutting down: HTTP server stopped, aborting feed and computation");
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
                info!("shutting down: feed error path");
            }
            _ = &mut gas_price_worker_handle => {
                // Gas price worker completed, which means it errored
                error!("Gas price worker error detected, shutting down solver");
                server_handle.stop(true).await;
                server_task.await.ok();
                feed_handle.abort();
                let _ = computation_shutdown_tx.send(());
                computation_manager_handle.abort();
                info!("shutting down: gas price error path");
            }
            _ = &mut computation_manager_handle => {
                // The derived-data pipeline task ended (event channel closed, shutdown, or a
                // panic). It is never respawned, so continuing here would serve quotes on
                // frozen derived data indefinitely with no path to recovery. Treat it as fatal,
                // mirroring the feed/gas arms: stop the server gracefully and exit non-zero so
                // the orchestrator restarts the instance (crash-only).
                error!("Computation manager stopped unexpectedly, shutting down solver");
                server_handle.stop(true).await;
                server_task.await.ok();
                feed_handle.abort();
                gas_price_worker_handle.abort();
                fatal_error =
                    Some(std::io::Error::other("computation manager stopped unexpectedly"));
            }
        }

        metrics_sampler_handle.abort();
        router_fee_worker_handle.abort();
        fee_tier_abort.abort();

        info!("shutting down worker pools");
        for pool in worker_pools {
            let name = pool.name().to_owned();
            info!(name, "shutting down pool");
            pool.shutdown();
            info!(name, "pool shut down");
        }

        info!("shutdown complete");

        match fatal_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
