use std::time::Duration;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use clap::Parser;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use thiserror::Error;
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tycho_solver::{builder::parse_chain, cli::Cli, config::WorkerPoolsConfig, TychoSolverBuilder};

fn main() -> Result<(), anyhow::Error> {
    create_tracing_subscriber();
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

fn create_tracing_subscriber() {
    // RUST_LOG environment variable controls log level
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_target(true) // Show module path where log originated
        .compact();
    tracing_subscriber::fmt()
        .event_format(format)
        .with_env_filter(EnvFilter::from_default_env())
        .init();
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
async fn setup_solver(cli: &Cli) -> Result<tycho_solver::builder::TychoSolver, SolverError> {
    // Load worker pools config
    let pools_config =
        WorkerPoolsConfig::load_from_file(&cli.worker_pools_config).map_err(|e| {
            SolverError::SetupError(format!("failed to load worker pools config: {}", e))
        })?;

    // Parse chain
    let chain = parse_chain(&cli.chain)
        .map_err(|e| SolverError::SetupError(format!("failed to parse chain: {}", e)))?;

    // Build solver with all fields from CLI
    let mut builder = TychoSolverBuilder::new(
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
    if let Some(ref api_key) = cli.tycho_api_key {
        builder = builder.tycho_api_key(api_key.clone());
    }

    // Build and start solver
    let solver = builder
        .build()
        .map_err(|e| SolverError::SetupError(format!("failed to start solver: {}", e)))?;

    Ok(solver)
}

#[tokio::main]
async fn run_solver(cli: Cli) -> Result<(), SolverError> {
    info!("Starting Tycho Solver");

    let _metrics_task = create_metrics_exporter();

    // Setup solver (handles setup errors)
    let solver = setup_solver(&cli).await?;

    // Run with graceful shutdown
    // The shutdown signal stops the server, which causes solver.run() to complete
    // and automatically clean up workers and feed (see TychoSolver::run() in builder.rs)
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

    Ok(())
}
