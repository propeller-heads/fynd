//! Starts the prices feeds and the HTTP and metrics servers.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tracing::info;

use crate::{
    api::{self, Streams},
    cli::Cli,
    latest_prices::{spawn_snapshot_age_reports, LatestPrices},
    rfq,
};

/// Byte sizes for `nexus_message_bytes`: from a withdrawal up to a full Bebop book.
const MESSAGE_BYTES_BUCKETS: &[f64] =
    &[256.0, 1e3, 1e4, 1e5, 2.5e5, 5e5, 1e6, 2.5e6, 5e6, 1e7, 2.5e7];

/// The epoch of this service run: its start time in Unix milliseconds.
fn start_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX))
}

/// Serves `/metrics` on `port`.
fn bind_metrics(port: u16) -> anyhow::Result<actix_web::dev::Server> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("nexus_message_bytes".to_string()),
            MESSAGE_BYTES_BUCKETS,
        )
        .context("invalid message size buckets")?
        .install_recorder()
        .context("cannot install the Prometheus recorder")?;
    let server = actix_web::HttpServer::new(move || {
        let handle = handle.clone();
        actix_web::App::new().route(
            "/metrics",
            actix_web::web::get().to(move || {
                let body = handle.render();
                async move {
                    actix_web::HttpResponse::Ok()
                        .content_type("text/plain; version=0.0.4; charset=utf-8")
                        .body(body)
                }
            }),
        )
    })
    .bind((Ipv4Addr::UNSPECIFIED, port))
    .with_context(|| format!("cannot bind the metrics port {port}"))?;
    Ok(server.run())
}

/// Starts the prices feeds and the HTTP and metrics servers, and returns when a server stops.
///
/// The caller sets up logging.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let credentials = rfq::require_credentials(&cli.venues, &cli.rfq)?;

    let metrics_server = bind_metrics(cli.metrics_port)?;
    let epoch = start_epoch_ms();
    let to_start = rfq::streams_to_serve(cli.chain, &credentials)?;
    info!(chain = %cli.chain, epoch, "starting the service");
    let mut streams = Vec::new();
    for venue_credentials in to_start {
        let venue = venue_credentials.venue();
        info!(%venue, "starting stream");
        let latest_prices = Arc::new(LatestPrices::new(cli.chain, venue, epoch));
        rfq::spawn_prices_feed(Arc::clone(&latest_prices), venue_credentials);
        streams.push(latest_prices);
    }
    spawn_snapshot_age_reports(streams.clone());

    let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, cli.http_port));
    let (http_server, bound) = api::bind(Arc::new(Streams::new(streams)), address)
        .with_context(|| format!("cannot bind the HTTP port {}", cli.http_port))?;
    info!(%bound, "HTTP API listening");
    tokio::select! {
        result = http_server => result.context("the HTTP server failed")?,
        result = metrics_server => result.context("the metrics server failed")?,
    }
    Ok(())
}
