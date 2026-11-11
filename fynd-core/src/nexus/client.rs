//! The pod's connection to a `fynd-nexus` service.

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use metrics::{counter, gauge};
use reqwest::{header::IF_NONE_MATCH, StatusCode, Url};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tracing::warn;

use super::{
    derived::DerivedSnapshot,
    price_levels::{PriceLevelsAssembler, PriceLevelsFeed},
};
use crate::{
    derived::{ComputationError, ComputationId},
    feed::market_data::{MarketData, MarketState},
};

/// How long a derived snapshot request may take. A snapshot is a few megabytes.
const DERIVED_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How many missing ids a warning lists.
const MISSING_SAMPLE_SIZE: usize = 10;

/// Errors when creating a [`NexusClient`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NexusError {
    /// The service URL is not an HTTP URL.
    #[error(
        "nexus URL '{url}' must be an http:// or https:// URL, e.g. \
         http://fynd-nexus:3000"
    )]
    InvalidUrl {
        /// The service URL supplied by the caller.
        url: String,
    },
    /// The HTTP client could not be built.
    #[error("cannot build the nexus HTTP client: {0}")]
    HttpClient(String),
}

/// Why fetching a derived snapshot failed.
#[derive(Debug, thiserror::Error)]
enum DerivedFetchError {
    /// The request failed or its body could not be read.
    #[error("request for {computation} at {url} failed: {source}")]
    Request { computation: ComputationId, url: String, source: reqwest::Error },
    /// The service answered with a status other than 200, 204 or 304.
    #[error("request for {computation} at {url} got HTTP status {status}")]
    Status { computation: ComputationId, url: String, status: StatusCode },
    /// The body is not a snapshot of the expected entries.
    #[error("snapshot of {computation} at {url} is malformed: {source}")]
    Malformed { computation: ComputationId, url: String, source: serde_json::Error },
}

/// Connects a pod to a `fynd-nexus` service.
#[derive(Debug, Clone)]
pub struct NexusClient {
    connection_url: String,
    http: reqwest::Client,
}

/// A client for one derived computation's latest snapshot on a `fynd-nexus` service.
///
/// It caches the latest snapshot it accepted.
///
/// Exports `nexus_derived_block_lag`, `nexus_derived_snapshot_age_seconds` and
/// `nexus_derived_missing_components` per computation, and counts failed fetches in
/// `nexus_client_failed_polls_total`.
#[derive(Debug)]
pub(crate) struct NexusDerivedClient<Output> {
    computation: ComputationId,
    client: NexusClient,
    warn_when_missing_ids_change: bool,
    cached: Mutex<Option<CachedSnapshot<Output>>>,
    reported_missing: AtomicUsize,
}

/// The latest snapshot a client accepted, converted into the computation's output.
#[derive(Debug)]
struct CachedSnapshot<Output> {
    block: u64,
    received_at: Instant,
    output: Output,
}

impl NexusClient {
    /// A client for the service at `connection_url`, such as `http://fynd-nexus:3000`.
    ///
    /// The service serves one chain; the caller points `connection_url` at the one of the pod's
    /// chain.
    ///
    /// # Errors
    ///
    /// [`NexusError::InvalidUrl`] when `connection_url` is not an http:// or https:// URL, and
    /// [`NexusError::HttpClient`] when the HTTP client cannot be built.
    pub fn new(connection_url: &str) -> Result<Self, NexusError> {
        let valid = Url::parse(connection_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host());
        if !valid {
            return Err(NexusError::InvalidUrl { url: connection_url.to_string() });
        }
        let connection_url = connection_url.trim_end_matches('/');
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| NexusError::HttpClient(e.to_string()))?;
        Ok(Self { connection_url: connection_url.to_string(), http })
    }

    /// A feed of one venue's price levels, built by `assembler` from the prices the service serves.
    pub fn price_levels_feed(&self, assembler: PriceLevelsAssembler) -> PriceLevelsFeed {
        PriceLevelsFeed::new(&self.connection_url, self.http.clone(), assembler)
    }
}

impl<Output: Clone> NexusDerivedClient<Output> {
    /// A client for `computation`'s snapshots from the service behind `client`.
    ///
    /// A change in the number of missing ids is logged as a warning when
    /// `warn_when_missing_ids_change` is set. It is always exported as a gauge.
    pub(crate) fn new(
        client: NexusClient,
        computation: ComputationId,
        warn_when_missing_ids_change: bool,
    ) -> Self {
        Self {
            computation,
            client,
            warn_when_missing_ids_change,
            cached: Mutex::new(None),
            reported_missing: AtomicUsize::new(0),
        }
    }

    /// The output of the latest snapshot the service has served, converted by `convert`.
    ///
    /// A fetched snapshot replaces the cached one only if its block is newer. When the fetch
    /// fails, the cached snapshot is returned and the failure is logged and counted. `missing`
    /// lists the ids of `market` that the output has no entry for.
    ///
    /// # Errors
    ///
    /// [`ComputationError::NoValidResult`] when the service has not served a snapshot yet.
    pub(crate) async fn get_latest_snapshot<Entry: DeserializeOwned>(
        &self,
        market: &MarketData,
        convert: fn(DerivedSnapshot<Entry>) -> Output,
        missing: fn(&MarketState, &Output) -> Vec<String>,
    ) -> Result<Output, ComputationError> {
        let mut cached_guard = self.cached.lock().await;
        let known_block = cached_guard
            .as_ref()
            .map(|cached| cached.block);
        match fetch_derived_snapshot::<Entry>(&self.client, self.computation, known_block).await {
            Ok(Some(snapshot)) if known_block.is_none_or(|known| snapshot.block > known) => {
                *cached_guard = Some(CachedSnapshot {
                    block: snapshot.block,
                    received_at: Instant::now(),
                    output: convert(snapshot),
                });
            }
            Ok(_) => {}
            Err(error) => {
                counter!("nexus_client_failed_polls_total", "computation" => self.computation)
                    .increment(1);
                warn!(computation = self.computation, "fetching from Fynd Nexus failed: {error}");
            }
        }
        let Some(cached) = cached_guard.as_ref() else {
            return Err(ComputationError::NoValidResult {
                reason: format!("{} has no snapshot from fynd-nexus yet", self.computation),
            });
        };
        let view = market.read().await;
        if let Some(block) = view.last_updated() {
            let lag = i128::from(block.number()) - i128::from(cached.block);
            gauge!("nexus_derived_block_lag", "computation" => self.computation).set(lag as f64);
        }
        let missing = missing(view.base_market_state(), &cached.output);
        drop(view);
        gauge!("nexus_derived_snapshot_age_seconds", "computation" => self.computation).set(
            cached
                .received_at
                .elapsed()
                .as_secs_f64(),
        );
        gauge!("nexus_derived_missing_components", "computation" => self.computation)
            .set(missing.len() as f64);
        let output = cached.output.clone();
        self.warn_if_missing_changed(&missing);
        Ok(output)
    }

    fn warn_if_missing_changed(&self, missing: &[String]) {
        let previous = self
            .reported_missing
            .swap(missing.len(), Ordering::Relaxed);
        if self.warn_when_missing_ids_change && previous != missing.len() && !missing.is_empty() {
            warn!(
                computation = self.computation,
                missing = missing.len(),
                sample = ?&missing[..missing.len().min(MISSING_SAMPLE_SIZE)],
                "the fynd-nexus snapshot lacks entries for part of this pod's market"
            );
        }
    }
}

/// The latest snapshot of the computation `computation`, or `None` when the service has none
/// newer than `known_block` (HTTP 304) or none yet (HTTP 204).
async fn fetch_derived_snapshot<Entry: DeserializeOwned>(
    client: &NexusClient,
    computation: ComputationId,
    known_block: Option<u64>,
) -> Result<Option<DerivedSnapshot<Entry>>, DerivedFetchError> {
    let url = format!("{}/v1/derived/{computation}", client.connection_url);
    let mut request = client
        .http
        .get(&url)
        .timeout(DERIVED_REQUEST_TIMEOUT);
    if let Some(block) = known_block {
        request = request.header(IF_NONE_MATCH, format!("\"{block}\""));
    }
    let request_failed =
        |source| DerivedFetchError::Request { computation, url: url.clone(), source };
    let response = request
        .send()
        .await
        .map_err(request_failed)?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_MODIFIED | StatusCode::NO_CONTENT => return Ok(None),
        status => return Err(DerivedFetchError::Status { computation, url, status }),
    }
    let body = response
        .bytes()
        .await
        .map_err(request_failed)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|source| DerivedFetchError::Malformed { computation, url, source })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::http("http://fynd-nexus:3000")]
    #[case::https("https://shared.example.com")]
    #[case::trailing_slash("http://fynd-nexus:3000/")]
    fn test_new_http_url(#[case] url: &str) {
        assert!(NexusClient::new(url).is_ok());
    }

    #[rstest]
    #[case::websocket("ws://shared:3000")]
    #[case::no_scheme("shared:3000")]
    #[case::empty("")]
    #[case::no_host("http://")]
    fn test_new_invalid_url(#[case] url: &str) {
        let error = NexusClient::new(url).unwrap_err();

        assert!(matches!(error, NexusError::InvalidUrl { .. }), "got {error}");
        assert!(
            error
                .to_string()
                .contains("http:// or https://"),
            "got {error}"
        );
    }
}
