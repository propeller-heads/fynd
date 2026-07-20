//! Fynd CLI - DeFi routing service
//!
//! A command-line application that runs an HTTP RPC server for finding optimal
//! swap routes across multiple DeFi protocols. Uses [`fynd-rpc`] for the HTTP server
//! and [`fynd-core`] for the routing algorithms.
//!
//! # Usage
//!
//! ```bash
//! # All on-chain protocols are fetched from Tycho RPC by default:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz
//!
//! # Combine all on-chain protocols with specific RFQ protocols:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols all_onchain,rfq:bebop
//!
//! # Or specify protocols explicitly:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols uniswap_v2,uniswap_v3
//! ```
//!
//! `--rpc-url` defaults to a chain-specific public endpoint. For production, provide a dedicated
//! one:
//!
//! ```bash
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --rpc-url https://your-rpc-provider.com/v1/your_key
//! ```
//!
//! See `fynd --help` for all available options.

use std::{path::Path, time::Duration};

#[cfg(feature = "metrics")]
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use clap::Parser;
use fynd_core::config::{embedded_default, remote, PartialConfig};
use fynd_rpc::{
    builder::FyndRPCBuilder,
    config::{defaults, BlocklistConfig, WorkerPoolsConfig},
    protocols::resolve_protocols,
};
mod cli;
mod commands;
use cli::{Cli, Commands};
#[cfg(feature = "metrics")]
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use thiserror::Error;
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tycho_simulation::utils::default_blocklist;

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Openapi => {
            use utoipa::OpenApi as _;
            let spec = fynd_rpc::api::ApiDoc::openapi();
            // Safety: OpenAPI spec serialization only fails on non-string map keys,
            // which utoipa never produces.
            let json = serde_json::to_string_pretty(&spec).expect("spec serialization cannot fail");
            println!("{json}");
            Ok(())
        }
        Commands::Serve(serve_args) => {
            run_solver(*serve_args).map_err(|e| anyhow!("{}", e))?;
            Ok(())
        }
        Commands::DeriveConnectorTokens(args) => tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime")
            .block_on(commands::derive_connector_tokens::run(args)),
    }
}

/// Errors that can occur during solver operation.
#[derive(Debug, Error)]
pub enum SolverError {
    /// Setup error (before runtime).
    #[error("setup error: {0}")]
    SetupError(String),

    /// Solver runtime error.
    #[error("solver runtime error: {0}")]
    SolverRuntimeError(String),

    /// Shutdown error.
    #[error("shutdown error: {0}")]
    ShutdownError(String),
}

fn create_tracing_subscriber() -> Option<TracerProvider> {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .compact();

    if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        match opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.clone())
            .build()
        {
            Ok(exporter) => {
                let provider = TracerProvider::builder()
                    .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                    .with_resource(opentelemetry_sdk::Resource::new(vec![
                        opentelemetry::KeyValue::new("service.name", "fynd"),
                    ]))
                    .build();

                let otel_layer =
                    tracing_opentelemetry::layer().with_tracer(provider.tracer("fynd"));

                tracing_subscriber::registry()
                    .with(EnvFilter::from_default_env())
                    .with(fmt_layer)
                    .with(otel_layer)
                    .init();

                info!("OpenTelemetry tracing enabled, exporting to: {}", endpoint);
                Some(provider)
            }
            Err(e) => {
                // Fall back to non-OTEL tracing if exporter fails
                tracing_subscriber::registry()
                    .with(EnvFilter::from_default_env())
                    .with(fmt_layer)
                    .init();

                error!("Failed to build OTLP exporter: {}. Continuing without OTEL.", e);
                None
            }
        }
    } else {
        // OTEL disabled, use only fmt layer
        tracing_subscriber::registry()
            .with(EnvFilter::from_default_env())
            .with(fmt_layer)
            .init();

        None
    }
}

/// Creates and runs the Prometheus metrics exporter using Actix Web.
///
/// Exposes `/metrics` on a dedicated HTTP server bound to `port`.
/// Compiled only when the `metrics` feature is enabled.
#[cfg(feature = "metrics")]
fn create_metrics_exporter(port: u16) -> tokio::task::JoinHandle<()> {
    let exporter_builder = PrometheusBuilder::new();
    let handle = exporter_builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    tokio::spawn(async move {
        async fn metrics_handler(handle: PrometheusHandle) -> impl Responder {
            let metrics = handle.render();
            HttpResponse::Ok()
                .content_type("text/plain; version=0.0.4; charset=utf-8")
                .body(metrics)
        }

        if let Err(e) = HttpServer::new(move || {
            App::new().route(
                "/metrics",
                web::get().to({
                    let handle = handle.clone();
                    move || metrics_handler(handle.clone())
                }),
            )
        })
        .bind(("0.0.0.0", port))
        .expect("Failed to bind metrics server")
        .run()
        .await
        {
            error!("Metrics server failed: {}", e);
        }
    })
}

/// Resolves the Tycho WebSocket URL: uses the override if provided, otherwise looks up the
/// chain-specific Fynd default endpoint.
fn resolve_tycho_url(chain: &str, override_url: Option<&str>) -> Result<String, SolverError> {
    match override_url {
        Some(url) => Ok(url.to_string()),
        None => {
            let default = defaults::default_tycho_url(chain).map_err(SolverError::SetupError)?;
            info!("No --tycho-url provided. Using default for {}: {}", chain, default);
            Ok(default.to_string())
        }
    }
}

/// Resolves the JSON-RPC URL: uses the override if provided, otherwise falls back to the
/// chain-specific public endpoint with a warning.
fn resolve_rpc_url(chain: &str, override_url: Option<&str>) -> Result<String, SolverError> {
    match override_url {
        Some(url) => Ok(url.to_string()),
        None => {
            let default = defaults::default_rpc_url(chain).map_err(SolverError::SetupError)?;
            warn!(
                "No --rpc-url provided. Using public endpoint for {}: {}. \
                For production use, provide a dedicated RPC endpoint.",
                chain, default
            );
            Ok(default.to_string())
        }
    }
}

/// Default local config file probed in the working directory when `--config-file` is not
/// passed.
const DEFAULT_CONFIG_PATH: &str = "fynd.toml";

/// Legacy pools-only config file, still honored: it supplies the `pools` section at
/// local-file priority when no higher layer sets pools.
const LEGACY_WORKER_POOLS_PATH: &str = "worker_pools.toml";

/// Overall time budget for the remote config fetch (including its internal retries);
/// startup resolves without the remote layer when it elapses.
const REMOTE_CONFIG_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolves the layered solver config:
/// CLI flags > local config file > remote config (S3) > embedded default.
///
/// Local files fail fast when explicitly passed or malformed — a broken local setup should
/// not start silently misconfigured. The remote fetch never fails startup: on any error or
/// timeout a warning is logged and the lower layers apply unchanged.
async fn resolve_solver_config(
    args: &cli::ServeArgs,
) -> Result<fynd_core::config::Config, SolverError> {
    if args.worker_pools_config.is_some() {
        warn!(
            "--worker-pools-config is deprecated and will be removed soon; move the [pools] \
             section into a config file passed via --config-file"
        );
    }
    let overrides = args
        .explicit_config()
        .map_err(|e| SolverError::SetupError(format!("failed to build config overrides: {e:#}")))?;

    let mut local_file = match &args.config_file {
        Some(path) => Some(
            PartialConfig::from_file(path)
                .map_err(|e| SolverError::SetupError(format!("failed to load config file: {e}")))?,
        ),
        None => {
            let default_path = Path::new(DEFAULT_CONFIG_PATH);
            if default_path.exists() {
                info!("{DEFAULT_CONFIG_PATH} found; using it as the local config file layer");
                Some(PartialConfig::from_file(default_path).map_err(|e| {
                    SolverError::SetupError(format!("failed to load config file: {e}"))
                })?)
            } else {
                None
            }
        }
    };

    // The legacy worker_pools.toml keeps working and, to not break existing clients, its
    // pools take priority over the config file's (only explicit CLI overrides beat it).
    let legacy_pools_path = Path::new(LEGACY_WORKER_POOLS_PATH);
    if overrides.pools.is_none() && legacy_pools_path.exists() {
        warn!(
            "{LEGACY_WORKER_POOLS_PATH} is deprecated and will be removed soon; move its \
             [pools] section into {DEFAULT_CONFIG_PATH}. For now its pools override the \
             config file's"
        );
        let pools = WorkerPoolsConfig::load_from_file(legacy_pools_path)
            .map_err(|e| {
                SolverError::SetupError(format!("failed to load worker pools config: {e:#}"))
            })?
            .into_pools();
        local_file
            .get_or_insert_with(PartialConfig::default)
            .pools = Some(pools);
    }

    let remote_config = if args.no_remote_config {
        None
    } else {
        let url = args
            .remote_config_url
            .clone()
            .unwrap_or_else(|| remote::default_remote_config_url(args.chain));
        match tokio::time::timeout(REMOTE_CONFIG_FETCH_TIMEOUT, remote::fetch_remote_config(&url))
            .await
        {
            Ok(Ok(partial)) => {
                info!(url, "fetched remote config");
                Some(partial)
            }
            Ok(Err(e)) => {
                warn!(url, error = %e, "remote config fetch failed; continuing without it");
                None
            }
            Err(_elapsed) => {
                warn!(
                    url,
                    timeout_ms = REMOTE_CONFIG_FETCH_TIMEOUT.as_millis() as u64,
                    "remote config fetch timed out; continuing without it"
                );
                None
            }
        }
    };

    // Ascending priority: embedded default, then the remote config, then the local config
    // file, then CLI overrides.
    let config = embedded_default()
        .clone()
        .apply(&remote_config.unwrap_or_default())
        .apply(&local_file.unwrap_or_default())
        .apply(&overrides);
    config
        .validate()
        .map_err(|e| SolverError::SetupError(format!("failed to resolve solver config: {e}")))?;
    Ok(config)
}

/// Sets up the solver (resolves config, parses chain, builds solver).
/// Returns setup errors if any step fails.
async fn setup_solver(args: &cli::ServeArgs) -> Result<fynd_rpc::builder::FyndRPC, SolverError> {
    let chain = args.chain;

    let config = resolve_solver_config(args).await?;
    info!(?config, "solver config resolved");

    let tycho_url = resolve_tycho_url(&chain.to_string(), args.tycho_url.as_deref())?;
    let rpc_url = resolve_rpc_url(&chain.to_string(), args.rpc_url.as_deref())?;

    let protocols = resolve_protocols(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &config.protocols,
    )
    .await
    .map_err(|e| SolverError::SetupError(format!("failed to resolve protocols: {e}")))?;

    info!(?protocols, "starting with {} protocol(s)", protocols.len());

    let mut builder =
        FyndRPCBuilder::new(chain, config.pools.clone(), tycho_url, rpc_url, protocols)
            .map_err(|e| SolverError::SetupError(format!("invalid pool configuration: {e}")))?
            .http_host(args.http_host.clone())
            .http_port(args.http_port)
            .min_tvl(config.min_tvl_or_chain_default(chain))
            .min_token_quality(config.min_token_quality)
            .traded_n_days_ago(config.traded_n_days_ago)
            .tvl_buffer_ratio(config.tvl_buffer_ratio)
            .gas_refresh_interval(Duration::from_secs(config.gas_refresh_interval_secs))
            .reconnect_delay(Duration::from_secs(config.reconnect_delay_secs))
            .worker_router_timeout(Duration::from_millis(config.worker_router_timeout_ms))
            .worker_router_min_responses(config.worker_router_min_responses)
            .gas_price_stale_threshold(
                args.gas_price_stale_threshold_secs
                    .map(Duration::from_secs),
            );

    if args.disable_tls {
        builder = builder.disable_tls();
    }
    if let Some(api_key) = &args.tycho_api_key {
        builder = builder.tycho_api_key(api_key.clone());
    }
    let blocklist = match &args.blocklist_config {
        Some(path) => BlocklistConfig::load_from_file(path)
            .map_err(|e| {
                SolverError::SetupError(format!("failed to load blocklist config: {}", e))
            })?
            .into_components(),
        None => default_blocklist(),
    };

    builder = builder.blocklist(blocklist);
    builder = builder.partial_blocks(config.partial_blocks);
    builder = builder.price_guard_enabled(args.enable_price_guard);

    // Build and start solver
    let solver = builder
        .build()
        .map_err(|e| SolverError::SetupError(format!("failed to start solver: {}", e)))?;

    Ok(solver)
}

#[tokio::main]
async fn run_solver(args: cli::ServeArgs) -> Result<(), SolverError> {
    let provider = create_tracing_subscriber();
    info!("Starting Fynd");

    #[cfg(feature = "metrics")]
    let _metrics_task = create_metrics_exporter(args.metrics_port);

    // Setup solver, but allow SIGINT to cancel it for fast exit during startup
    let solver = tokio::select! {
        result = setup_solver(&args) => result?,
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT received during setup. Exiting.");
            return Ok(());
        }
    };

    // Run with graceful shutdown
    // The shutdown signal stops the server, which causes solver.run() to complete
    // and automatically clean up workers and feed (see Fynd::run() in builder.rs)
    let server_handle = solver.server_handle();
    let shutdown_signal = tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(e) => {
                error!("Failed to register SIGTERM handler: {}", e);
                return Err(SolverError::SetupError(format!(
                    "failed to register signal handler: {}",
                    e
                )));
            }
        };

        select! {
            _ = ctrl_c => {
                info!("SIGINT (Ctrl+C) received. Stopping server...");
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received. Stopping server...");
            }
        }

        server_handle.stop(true).await;
        Ok::<(), SolverError>(())
    });

    select! {
        result = solver.run() => {
            if let Err(e) = result {
                return Err(SolverError::SolverRuntimeError(e.to_string()));
            }
        }
        result = shutdown_signal => {
            // Shutdown signal received and server stopped
            if let Err(e) = result {
                return Err(SolverError::ShutdownError(e.to_string()));
            }
        }
    }

    if let Some(provider) = provider {
        let _ = provider.shutdown();
    }
    Ok(())
}
