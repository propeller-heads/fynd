//! Starts the prices feeds, the Tycho market and the HTTP and metrics servers.

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
    market::market_launch::{launch_market, LaunchedMarket},
    price_levels,
};

/// Byte sizes for `nexus_message_bytes`: from a withdrawal up to a full Bebop book.
const MESSAGE_BYTES_BUCKETS: &[f64] =
    &[256.0, 1e3, 1e4, 1e5, 2.5e5, 5e5, 1e6, 2.5e6, 5e6, 1e7, 2.5e7];

/// The run id of this service run: its start time in Unix milliseconds.
fn new_run_id() -> u64 {
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

/// Waits for the market's task to end, which is an error. Never returns without a market.
async fn market_ended(launched: Option<&mut LaunchedMarket>) -> anyhow::Result<()> {
    let Some(launched) = launched else {
        return std::future::pending().await;
    };
    let joined: Result<anyhow::Result<()>, _> = (&mut launched.task).await;
    joined
        .context("the market task failed")?
        .context("the market stopped")
}

/// Starts the prices feeds, the Tycho market and the HTTP and metrics servers, and returns when
/// one stops.
///
/// The caller sets up logging.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let credentials = price_levels::require_credentials(&cli.venues, &cli.venue_args)?;
    anyhow::ensure!(
        !credentials.is_empty() || cli.market.is_enabled(),
        "nothing to run: list venues in --venues or set --tycho-url to run the market"
    );

    let metrics_server = bind_metrics(cli.metrics_port)?;
    let run_id = new_run_id();
    let to_start = price_levels::streams_to_serve(cli.chain, &credentials)?;
    info!(chain = %cli.chain, run_id, "starting the service");
    let mut streams = Vec::new();
    for venue_credentials in to_start {
        let venue = venue_credentials.venue();
        info!(%venue, "starting stream");
        let latest_prices = Arc::new(LatestPrices::new(cli.chain, venue, run_id));
        price_levels::spawn_prices_feed(Arc::clone(&latest_prices), venue_credentials);
        streams.push(latest_prices);
    }
    let mut market = None;
    if cli.market.is_enabled() {
        info!("starting the market");
        market = Some(launch_market(cli.chain, &cli.market).await?);
    }
    spawn_snapshot_age_reports(streams.clone());

    let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, cli.http_port));
    let (http_server, bound) = api::bind(Arc::new(Streams::new(streams)), None, address)
        .with_context(|| format!("cannot bind the HTTP port {}", cli.http_port))?;
    info!(%bound, "HTTP API listening");
    tokio::select! {
        result = http_server => result.context("the HTTP server failed")?,
        result = metrics_server => result.context("the metrics server failed")?,
        result = market_ended(market.as_mut()) => result?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[tokio::test]
    async fn test_run_fails_with_neither_venues_nor_market() {
        let cli = Cli::try_parse_from(["fynd-nexus", "--chain", "ethereum", "--venues"]).unwrap();

        let error = run(cli).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("nothing to run"),
            "{error}"
        );
    }
}
