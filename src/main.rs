//! Fynd CLI - DeFi routing service
//!
//! A command-line application that runs an HTTP RPC server for finding optimal
//! swap routes across multiple DeFi protocols. Uses [`fynd-rpc`] for the HTTP server
//! and [`fynd-core`] for the routing algorithms.
//!
//! # Usage
//!
//! ```bash
//! fynd --rpc-url $RPC_URL \
//!      --tycho-url tycho-beta.propellerheads.xyz \
//!      --protocols uniswap_v2,uniswap_v3
//! ```
//!
//! See `fynd --help` for all available options.

use std::time::Duration;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use clap::Parser;
use fynd_rpc::{
    builder::{parse_chain, FyndBuilder},
    config::{BlacklistConfig, WorkerPoolsConfig},
};

mod cli;
use cli::Cli;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use thiserror::Error;
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    run_solver(cli).map_err(|e| anyhow!("{}", e))?;
    Ok(())
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
                        opentelemetry::KeyValue::new("service.name", "tycho-solver"),
                    ]))
                    .build();

                let otel_layer =
                    tracing_opentelemetry::layer().with_tracer(provider.tracer("tycho-solver"));

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
/// This exposes the metrics on the '/metrics' endpoint on a separate HTTP server on port 9898.
fn create_metrics_exporter() -> tokio::task::JoinHandle<()> {
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
        .bind(("0.0.0.0", 9898))
        .expect("Failed to bind metrics server")
        .run()
        .await
        {
            error!("Metrics server failed: {}", e);
        }
    })
}

/// Sets up the solver (loads config, parses chain, builds solver).
/// Returns setup errors if any step fails.
async fn setup_solver(cli: &Cli) -> Result<fynd_rpc::builder::Fynd, SolverError> {
    // Load worker pools config
    let pools_config =
        WorkerPoolsConfig::load_from_file(&cli.worker_pools_config).map_err(|e| {
            SolverError::SetupError(format!("failed to load worker pools config: {}", e))
        })?;

    // Parse chain
    let chain = parse_chain(&cli.chain)
        .map_err(|e| SolverError::SetupError(format!("failed to parse chain: {}", e)))?;

    // Build solver with all fields from CLI
    let mut builder = FyndBuilder::new(
        chain,
        pools_config.pools,
        cli.tycho_url.clone(),
        cli.rpc_url.clone(),
        cli.protocols.clone(),
    )
    .http_host(cli.http_host.clone())
    .http_port(cli.http_port)
    .min_tvl(cli.min_tvl)
    .tvl_buffer_multiplier(cli.tvl_buffer_multiplier)
    .gas_refresh_interval(Duration::from_secs(cli.gas_refresh_interval_secs))
    .reconnect_delay(Duration::from_secs(cli.reconnect_delay_secs))
    .order_manager_timeout(Duration::from_millis(cli.order_manager_timeout_ms))
    .order_manager_min_responses(cli.order_manager_min_responses);

    if cli.disable_tls {
        builder = builder.disable_tls();
    }
    if let Some(api_key) = &cli.tycho_api_key {
        builder = builder.tycho_api_key(api_key.clone());
    }
    if let Some(blacklist_path) = &cli.blacklist_config {
        let blacklist = BlacklistConfig::load_from_file(blacklist_path).map_err(|e| {
            SolverError::SetupError(format!("failed to load blacklist config: {}", e))
        })?;
        builder = builder.blacklist(blacklist);
    }

    // Build and start solver
    let solver = builder
        .build()
        .map_err(|e| SolverError::SetupError(format!("failed to start solver: {}", e)))?;

    Ok(solver)
}

#[tokio::main]
async fn run_solver(cli: Cli) -> Result<(), SolverError> {
    let provider = create_tracing_subscriber();
    info!("Starting Fynd");

    let _metrics_task = if cli.enable_metrics {
        info!("Starting metrics server on port 9898");
        Some(create_metrics_exporter())
    } else {
        None
    };

    // Setup solver (handles setup errors)
    let solver = setup_solver(&cli).await?;

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
