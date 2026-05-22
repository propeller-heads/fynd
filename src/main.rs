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
//! `--rpc-url` defaults to `https://eth.llamarpc.com`. For production, provide a dedicated endpoint:
//!
//! ```bash
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --rpc-url https://your-rpc-provider.com/v1/your_key
//! ```
//!
//! See `fynd --help` for all available options.

use std::time::Duration;

#[cfg(feature = "metrics")]
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use clap::Parser;
use fynd_rpc::{
    builder::{parse_chain, FyndRPCBuilder},
    config::{defaults, BlocklistConfig, WorkerPoolsConfig},
    protocols::fetch_protocol_systems,
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
use tycho_simulation::{tycho_common::models::Chain, utils::default_blocklist};

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

/// Resolves the Ethereum RPC URL: uses the override if provided, otherwise falls back to the
/// public endpoint with a warning.
fn resolve_rpc_url(override_url: Option<&str>) -> String {
    match override_url {
        Some(url) => url.to_string(),
        None => {
            warn!(
                "No --rpc-url provided. Using public endpoint: {}. \
                For production use, provide a dedicated RPC endpoint.",
                defaults::DEFAULT_RPC_URL
            );
            defaults::DEFAULT_RPC_URL.to_string()
        }
    }
}

/// Resolves the protocol list from `--protocols`.
///
/// - Empty → fetch all on-chain protocols from Tycho RPC.
/// - Contains `"all_onchain"` → fetch all on-chain, then append any other explicit entries.
/// - Otherwise → use as given (no network call).
///
/// Returns an error if the resolved list is empty.
async fn resolve_protocols(
    tycho_url: &str,
    api_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
    requested: &[String],
) -> Result<Vec<String>, SolverError> {
    let needs_fetch = requested.is_empty() ||
        requested
            .iter()
            .any(|p| p == "all_onchain");
    let protocols = if needs_fetch {
        let mut fetched = fetch_protocol_systems(tycho_url, api_key, use_tls, chain)
            .await
            .map_err(|e| {
                SolverError::SetupError(format!("failed to fetch protocol systems: {e}"))
            })?;
        for p in requested {
            if p != "all_onchain" && !fetched.contains(p) {
                fetched.push(p.clone());
            }
        }
        fetched
    } else {
        requested.to_vec()
    };
    if protocols.is_empty() {
        return Err(SolverError::SetupError(
            "no supported protocols found. Provide --protocols or check Tycho connectivity."
                .to_string(),
        ));
    }
    Ok(protocols)
}

/// Result of solver setup, including optional slippage feature components.
struct SetupResult {
    solver: fynd_rpc::builder::FyndRPC,
    #[cfg(feature = "slippage-features")]
    resim_rx: Option<tokio::sync::mpsc::Receiver<fynd_core::observer::QuoteProducedEvent>>,
    #[cfg(feature = "slippage-features")]
    slippage_observer: Option<std::sync::Arc<slippage_features::quote_log::QuoteLogObserver>>,
}

/// Sets up the solver (loads config, parses chain, builds solver).
/// Returns setup errors if any step fails.
async fn setup_solver(args: &cli::ServeArgs) -> Result<SetupResult, SolverError> {
    // Load worker pools config, falling back to the built-in defaults when the default path is
    // absent (e.g. `cargo install`, Docker). Custom paths that don't exist still fail fast.
    let default_path = std::path::Path::new("worker_pools.toml");
    let pools_config =
        if args.worker_pools_config == default_path && !args.worker_pools_config.exists() {
            warn!(
                "worker_pools.toml not found; using built-in defaults. \
             Set --worker-pools-config or WORKER_POOLS_CONFIG to use a custom config."
            );
            WorkerPoolsConfig::builtin_default()
        } else {
            WorkerPoolsConfig::load_from_file(&args.worker_pools_config).map_err(|e| {
                SolverError::SetupError(format!("failed to load worker pools config: {}", e))
            })?
        };

    // Parse chain
    let chain = parse_chain(&args.chain)
        .map_err(|e| SolverError::SetupError(format!("failed to parse chain: {}", e)))?;

    let tycho_url = resolve_tycho_url(&args.chain, args.tycho_url.as_deref())?;
    let rpc_url = resolve_rpc_url(args.rpc_url.as_deref());

    let protocols = resolve_protocols(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &args.protocols,
    )
    .await?;

    info!(?protocols, "starting with {} protocol(s)", protocols.len());

    // Build solver with all fields from CLI
    let mut builder =
        FyndRPCBuilder::new(chain, pools_config.into_pools(), tycho_url, rpc_url, protocols)
            .map_err(|e| SolverError::SetupError(format!("invalid pool configuration: {e}")))?
            .http_host(args.http_host.clone())
            .http_port(args.http_port)
            .min_tvl(args.min_tvl)
            .min_token_quality(args.min_token_quality)
            .traded_n_days_ago(args.traded_n_days_ago)
            .tvl_buffer_ratio(args.tvl_buffer_ratio)
            .gas_refresh_interval(Duration::from_secs(args.gas_refresh_interval_secs))
            .reconnect_delay(Duration::from_secs(args.reconnect_delay_secs))
            .worker_router_timeout(Duration::from_millis(args.worker_router_timeout_ms))
            .worker_router_min_responses(args.worker_router_min_responses)
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
    builder = builder.price_guard_enabled(args.enable_price_guard);

    // Wire slippage-features observer if enabled.
    #[cfg(feature = "slippage-features")]
    let (resim_rx, slippage_observer) = {
        let output_dir = std::path::PathBuf::from("./slippage-data");
        std::fs::create_dir_all(&output_dir).map_err(|e| {
            SolverError::SetupError(format!("failed to create slippage data dir: {e}"))
        })?;

        let flush_threshold: usize = std::env::var("SLIPPAGE_FLUSH_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);

        let (tx, rx) = tokio::sync::mpsc::channel(1000);
        let observer = std::sync::Arc::new(slippage_features::quote_log::QuoteLogObserver::new(
            output_dir.clone(),
            tx,
            flush_threshold,
        ));

        builder = builder.with_observer(observer.clone());
        info!(flush_threshold, "slippage feature collection enabled");
        (Some(rx), Some(observer))
    };

    // Build and start solver
    let solver = builder
        .build()
        .map_err(|e| SolverError::SetupError(format!("failed to start solver: {}", e)))?;

    Ok(SetupResult {
        solver,
        #[cfg(feature = "slippage-features")]
        resim_rx,
        #[cfg(feature = "slippage-features")]
        slippage_observer,
    })
}

#[tokio::main]
async fn run_solver(args: cli::ServeArgs) -> Result<(), SolverError> {
    let provider = create_tracing_subscriber();
    info!("Starting Fynd");

    #[cfg(feature = "metrics")]
    let _metrics_task = create_metrics_exporter(args.metrics_port);

    // Setup solver, but allow SIGINT to cancel it for fast exit during startup
    let setup = tokio::select! {
        result = setup_solver(&args) => result?,
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT received during setup. Exiting.");
            return Ok(());
        }
    };

    let solver = setup.solver;

    // Spawn the resim background task when slippage-features is enabled.
    #[cfg(feature = "slippage-features")]
    let _resim_handle = {
        if let Some(rx) = setup.resim_rx {
            let market_data = solver.market_data();
            let derived_data = solver.derived_data();
            let output_dir = std::path::PathBuf::from("./slippage-data");
            let requote_url = std::env::var("SLIPPAGE_REQUOTE_URL")
                .ok()
                .or_else(|| Some("http://localhost:3000".to_string()));
            Some(tokio::spawn(slippage_features::tycho_resim::run_tycho_resim(
                rx,
                market_data,
                derived_data,
                output_dir,
                requote_url,
            )))
        } else {
            None
        }
    };

    // Spawn periodic flush timer for quote log when slippage-features is enabled.
    #[cfg(feature = "slippage-features")]
    let _flush_handle = {
        if let Some(obs) = setup.slippage_observer {
            let flush_interval_secs: u64 = std::env::var("SLIPPAGE_FLUSH_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            Some(tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(flush_interval_secs));
                loop {
                    interval.tick().await;
                    obs.flush_if_stale(std::time::Duration::from_secs(flush_interval_secs));
                }
            }))
        } else {
            None
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
