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
//! fynd serve --tycho-url tycho-beta.propellerheads.xyz
//!
//! # Combine all on-chain protocols with specific RFQ protocols:
//! fynd serve --tycho-url tycho-beta.propellerheads.xyz \
//!            --protocols all_onchain,rfq:bebop
//!
//! # Or specify protocols explicitly:
//! fynd serve --tycho-url tycho-beta.propellerheads.xyz \
//!            --protocols uniswap_v2,uniswap_v3
//! ```
//!
//! `--rpc-url` defaults to `https://eth.llamarpc.com`. For production, provide a dedicated endpoint:
//!
//! ```bash
//! fynd serve --tycho-url tycho-beta.propellerheads.xyz \
//!            --rpc-url https://your-rpc-provider.com/v1/your_key
//! ```
//!
//! See `fynd --help` for all available options.

use std::time::Duration;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use clap::Parser;
use fynd_rpc::{
    builder::{parse_chain, FyndBuilder},
    config::{defaults, BlacklistConfig, WorkerPoolsConfig},
};
use tycho_simulation::{
    tycho_client::rpc::{HttpRPCClient, HttpRPCClientOptions, RPCClient},
    tycho_common::models::Chain,
};

mod cli;
use cli::{Cli, Commands};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use thiserror::Error;
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
async fn setup_solver(args: &cli::ServeArgs) -> Result<fynd_rpc::builder::Fynd, SolverError> {
    // Load worker pools config
    let pools_config =
        WorkerPoolsConfig::load_from_file(&args.worker_pools_config).map_err(|e| {
            SolverError::SetupError(format!("failed to load worker pools config: {}", e))
        })?;

    // Parse chain
    let chain = parse_chain(&args.chain)
        .map_err(|e| SolverError::SetupError(format!("failed to parse chain: {}", e)))?;

    // Resolve RPC URL, falling back to public endpoint with a warning
    let rpc_url = match &args.rpc_url {
        Some(url) => url.clone(),
        None => {
            warn!(
                "No --rpc-url provided. Using public endpoint: {}. \
                For production use, provide a dedicated RPC endpoint.",
                defaults::DEFAULT_RPC_URL
            );
            defaults::DEFAULT_RPC_URL.to_string()
        }
    };

    // Resolve protocols: fetch from Tycho RPC if omitted or if "all_onchain" is used
    let needs_fetch = args.protocols.is_empty() ||
        args.protocols
            .iter()
            .any(|p| p == "all_onchain");
    let protocols = if needs_fetch {
        let mut fetched = fetch_protocol_systems(
            &args.tycho_url,
            args.tycho_api_key.as_deref(),
            !args.disable_tls,
            chain,
        )
        .await
        .map_err(|e| SolverError::SetupError(format!("failed to fetch protocol systems: {}", e)))?;
        // Append any explicit protocols (e.g. rfq:bebop) alongside all_onchain
        for p in &args.protocols {
            if p != "all_onchain" && !fetched.contains(p) {
                fetched.push(p.clone());
            }
        }
        fetched
    } else {
        args.protocols.clone()
    };

    if protocols.is_empty() {
        return Err(SolverError::SetupError(
            "no supported protocols found. Provide --protocols or check Tycho connectivity."
                .to_string(),
        ));
    }

    info!(?protocols, "starting with {} protocol(s)", protocols.len());

    // Build solver with all fields from CLI
    let mut builder =
        FyndBuilder::new(chain, pools_config.pools, args.tycho_url.clone(), rpc_url, protocols)
            .http_host(args.http_host.clone())
            .http_port(args.http_port)
            .min_tvl(args.min_tvl)
            .min_token_quality(args.min_token_quality)
            .tvl_buffer_multiplier(args.tvl_buffer_multiplier)
            .gas_refresh_interval(Duration::from_secs(args.gas_refresh_interval_secs))
            .reconnect_delay(Duration::from_secs(args.reconnect_delay_secs))
            .order_manager_timeout(Duration::from_millis(args.order_manager_timeout_ms))
            .order_manager_min_responses(args.order_manager_min_responses);

    if args.disable_tls {
        builder = builder.disable_tls();
    }
    if let Some(api_key) = &args.tycho_api_key {
        builder = builder.tycho_api_key(api_key.clone());
    }
    if let Some(blacklist_path) = &args.blacklist_config {
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
async fn run_solver(args: cli::ServeArgs) -> Result<(), SolverError> {
    let provider = create_tracing_subscriber();
    info!("Starting Fynd");

    let _metrics_task = create_metrics_exporter();

    // Setup solver (handles setup errors)
    let solver = setup_solver(&args).await?;

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

/// Fetches all available protocol systems from Tycho RPC.
async fn fetch_protocol_systems(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
) -> Result<Vec<String>, anyhow::Error> {
    use tycho_simulation::tycho_common::dto::{PaginationParams, ProtocolSystemsRequestBody};

    info!("Fetching available protocol systems from Tycho RPC...");

    let rpc_url =
        if use_tls { format!("https://{tycho_url}") } else { format!("http://{tycho_url}") };
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options)?;

    const PAGE_SIZE: i64 = 100;
    let mut all_protocols = Vec::new();
    let mut page = 0;

    loop {
        let request = ProtocolSystemsRequestBody {
            chain: chain.into(),
            pagination: PaginationParams { page, page_size: PAGE_SIZE },
        };
        let response = rpc_client
            .get_protocol_systems(&request)
            .await?;
        let count = response.protocol_systems.len();
        all_protocols.extend(response.protocol_systems);
        if (count as i64) < PAGE_SIZE {
            break;
        }
        page += 1;
    }

    debug!("Fetched {} protocol system(s) from Tycho RPC", all_protocols.len());
    Ok(all_protocols)
}
