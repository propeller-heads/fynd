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
//!
//! # Opt a protocol into streaming its exclusive pools:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols all_onchain,exclusive:ekubo_v3
//!
//! # Serve FermiSwap from the Titan pAMM price level stream instead of simulating it in the EVM:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols all_onchain,exclude:vm:fermiswap,pricelevelstream:fermiswap
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

use std::time::Duration;

#[cfg(feature = "metrics")]
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use clap::Parser;
use fynd_rpc::{
    builder::FyndRPCBuilder,
    config::{defaults, BlocklistConfig, WorkerPoolsConfig},
    parse_chain,
    protocols::resolve_protocols,
};
mod cli;
mod commands;
use cli::{Cli, Commands};
#[cfg(feature = "metrics")]
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
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
use tycho_simulation::{
    tycho_common::models::chain_config::TvlThresholdTier, utils::default_blocklist,
};

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Openapi => {
            let spec = fynd_rpc::api::openapi_spec();
            // Safety: OpenAPI spec serialization only fails on non-string map keys,
            // which utoipa never produces.
            let json = serde_json::to_string_pretty(&spec).expect("spec serialization cannot fail");
            println!("{json}");
            Ok(())
        }
        Commands::Serve(serve_args) => {
            // Log the failure before returning it: the fmt layer writes to stdout, whereas a
            // `main` that returns `Err` prints only to stderr, so log pipelines that follow stdout
            // show a run that stops mid-startup with no reason. Returning the error too keeps the
            // exit code.
            run_solver(*serve_args).map_err(|e| {
                error!(error = %e, "Fynd exited with an error");
                anyhow!("{}", e)
            })?;
            Ok(())
        }
        Commands::DeriveConnectorTokens(args) => tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime")
            .block_on(commands::derive_connector_tokens::run(*args)),
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
/// Exposes `/metrics` on a dedicated HTTP server bound to `port`. Every metric carries a
/// global `chain` label so multi-chain fleets can aggregate without pod-name regexes.
/// All `*_seconds` histograms render as bucketed Prometheus histograms (aggregatable
/// across pods, unlike summary quantiles); `worker_router_solver_responses` is a count
/// distribution and gets its own 0..=6 buckets.
/// Compiled only when the `metrics` feature is enabled.
#[cfg(feature = "metrics")]
fn create_metrics_exporter(host: &str, port: u16, chain: &str) -> tokio::task::JoinHandle<()> {
    const LATENCY_BUCKETS_SECONDS: &[f64] =
        &[0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
    const SOLVER_RESPONSE_BUCKETS: &[f64] = &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];

    let handle = PrometheusBuilder::new()
        // Normalize: `--chain` is free-form ("Ethereum" == "ethereum" after parse_chain),
        // but differing label case would fragment the per-chain series.
        .add_global_label("chain", chain.to_lowercase())
        .set_buckets_for_metric(Matcher::Suffix("_seconds".to_string()), LATENCY_BUCKETS_SECONDS)
        .expect("static bucket list is non-empty")
        .set_buckets_for_metric(
            Matcher::Full("worker_router_solver_responses".to_string()),
            SOLVER_RESPONSE_BUCKETS,
        )
        .expect("static bucket list is non-empty")
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    record_build_info();

    let bind_host = host.to_string();
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
        .bind((bind_host.as_str(), port))
        .expect("Failed to bind metrics server")
        .run()
        .await
        {
            error!("Metrics server failed: {}", e);
        }
    })
}

/// Records a constant-1 gauge labelled with the Fynd binary version, so operators can see which
/// build a running instance is on. The value carries no meaning; the version lives in the label.
#[cfg(feature = "metrics")]
fn record_build_info() {
    metrics::describe_gauge!(
        "fynd_build_info",
        "Fynd build information exposed as a constant-1 gauge labelled by version."
    );
    metrics::gauge!("fynd_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
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

/// Sets up the solver (loads config, parses chain, builds solver).
/// Returns setup errors if any step fails.
async fn setup_solver(args: &cli::ServeArgs) -> Result<fynd_rpc::builder::FyndRPC, SolverError> {
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

    if let Some(path) = &args.chains_config {
        fynd_rpc::init_chain_registry_from_file(path)
            .map_err(|e| SolverError::SetupError(format!("failed to load chains config: {e}")))?;
        info!(?path, "installed custom chain registry");
    }

    // Parse chain
    let chain = parse_chain(&args.chain)
        .map_err(|e| SolverError::SetupError(format!("failed to parse chain: {}", e)))?;

    let tycho_url = resolve_tycho_url(&args.chain, args.tycho_url.as_deref())?;
    let rpc_url = resolve_rpc_url(&args.chain, args.rpc_url.as_deref())?;
    let min_tvl = args
        .min_tvl
        .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Low));

    let protocols = resolve_protocols(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &args.protocols,
    )
    .await
    .map_err(|e| SolverError::SetupError(format!("failed to resolve protocols: {e}")))?;

    info!(?protocols, "starting with {} protocol(s)", protocols.len());

    // Build solver with all fields from CLI
    let mut builder =
        FyndRPCBuilder::new(chain, pools_config.into_pools(), tycho_url, rpc_url, protocols)
            .map_err(|e| SolverError::SetupError(format!("invalid pool configuration: {e}")))?
            .http_host(args.http_host.clone())
            .http_port(args.http_port)
            .min_tvl(min_tvl)
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
            )
            .hosted_swagger_url(args.hosted_swagger_url.clone());

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
    builder = builder.partial_blocks(args.partial_blocks);
    builder = builder.stream_timeout_secs(args.stream_timeout_secs);
    builder = builder.price_guard_enabled(args.enable_price_guard);
    if let Some(watermark) = &args.calldata_watermark {
        builder = builder.calldata_watermark(watermark.as_bytes());
    }

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
    let _metrics_task = create_metrics_exporter(&args.metrics_host, args.metrics_port, &args.chain);

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

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    #[test]
    fn record_build_info_emits_versioned_gauge() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, super::record_build_info);
        let rendered = handle.render();
        assert!(
            rendered.contains(&format!(
                "fynd_build_info{{version=\"{}\"}} 1",
                env!("CARGO_PKG_VERSION")
            )),
            "unexpected metrics output:\n{rendered}"
        );
    }
}
