//! The pod's connection to a `fynd-nexus` service.

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use metrics::{counter, gauge};
use reqwest::{header::IF_NONE_MATCH, StatusCode, Url};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::{
    derived::DerivedSnapshot,
    price_levels::{PriceLevelsAssembler, PriceLevelsFeed},
};
use crate::{
    derived::ComputationId,
    feed::market_data::{MarketData, MarketState},
};

/// How long a derived snapshot request may take. A snapshot is a few megabytes.
const DERIVED_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How many blocks in a row a snapshot must be within `max_block_lag` before a computation that
/// stopped using the service uses it again.
const IN_SYNC_BLOCKS_TO_RESUME: u32 = 3;

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
/// It caches the latest snapshot it accepted and counts the blocks it has been in sync with the
/// market.
///
/// Exports `nexus_derived_block_lag`, `nexus_derived_snapshot_age_seconds` and
/// `nexus_derived_missing_components` per computation, and counts failed fetches in
/// `nexus_client_failed_polls_total`.
#[derive(Debug)]
pub(crate) struct NexusDerivedClient<Output> {
    computation: ComputationId,
    client: NexusClient,
    warn_when_missing_ids_change: bool,
    max_block_lag: u64,
    state: Mutex<DerivedClientState<Output>>,
    reported_missing: AtomicUsize,
}

/// What a [`NexusDerivedClient`] keeps between blocks.
#[derive(Debug)]
struct DerivedClientState<Output> {
    cached: Option<CachedSnapshot<Output>>,
    /// Blocks in a row the held snapshot was within `max_block_lag`; 0 when it is not now.
    in_sync_blocks: u32,
    last_answer: LastAnswer,
}

/// What the client's last call answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAnswer {
    Nothing,
    InSync,
    NotInSync,
}

/// The latest snapshot from `fynd-nexus`, by how it follows this pod's market.
#[derive(Debug, PartialEq, Eq)]
#[expect(clippy::enum_variant_names, reason = "the variants name the three sync outcomes")]
pub(crate) enum NexusSnapshot<Output> {
    /// The snapshot is in sync: use it.
    InSync(Output),
    /// The last block's snapshot was in sync and this one is not.
    FellOutOfSync,
    /// The snapshot is out of sync, or none is held.
    OutOfSync,
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
    /// `warn_when_missing_ids_change` is set. It is always exported as a gauge. A snapshot is
    /// within the lag while it is at most `max_block_lag` blocks behind the market.
    pub(crate) fn new(
        client: NexusClient,
        computation: ComputationId,
        warn_when_missing_ids_change: bool,
        max_block_lag: u64,
    ) -> Self {
        Self {
            computation,
            client,
            warn_when_missing_ids_change,
            max_block_lag,
            state: Mutex::new(DerivedClientState {
                cached: None,
                in_sync_blocks: 0,
                last_answer: LastAnswer::Nothing,
            }),
            reported_missing: AtomicUsize::new(0),
        }
    }

    /// The latest snapshot from the service, when it is in sync with the market.
    ///
    /// Call it once per block. A fetched snapshot replaces the held one only if its block is
    /// newer. When the fetch fails, the held snapshot stays and the failure is logged and counted.
    /// `convert` turns a snapshot into the output and `missing` lists the ids of `market` that
    /// the output has no entry for. See [`classify`] for when a snapshot is in sync.
    ///
    /// Logs a warning when the snapshot falls out of sync and a message when it is in sync again.
    pub(crate) async fn get_in_sync_snapshot<Entry: DeserializeOwned>(
        &self,
        market: &MarketData,
        convert: fn(DerivedSnapshot<Entry>) -> Output,
        missing: fn(&MarketState, &Output) -> Vec<String>,
    ) -> NexusSnapshot<Output> {
        let mut state_guard = self.state.lock().await;
        let DerivedClientState { cached, in_sync_blocks, last_answer } = &mut *state_guard;
        let known_block = cached
            .as_ref()
            .map(|cached| cached.block);
        match fetch_derived_snapshot::<Entry>(&self.client, self.computation, known_block).await {
            Ok(Some(snapshot)) if known_block.is_none_or(|known| snapshot.block > known) => {
                *cached = Some(CachedSnapshot {
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
        let mut block_lag = None;
        if let Some(cached) = cached.as_ref() {
            let view = market.read().await;
            let market_block = view
                .last_updated()
                .map(|block| block.number());
            let lag = market_block.map_or(0, |block| block.saturating_sub(cached.block));
            let missing = missing(view.base_market_state(), &cached.output);
            drop(view);
            self.report_held_snapshot(cached, market_block.map(|_| lag), &missing);
            block_lag = Some(lag);
        }
        let within_lag = cached
            .as_ref()
            .filter(|_| block_lag.is_some_and(|lag| lag <= self.max_block_lag))
            .map(|cached| &cached.output);
        *in_sync_blocks = if within_lag.is_some() { in_sync_blocks.saturating_add(1) } else { 0 };

        let answer = classify(*last_answer, *in_sync_blocks, within_lag);
        self.log_answer(&answer, *last_answer, block_lag);
        match answer {
            NexusSnapshot::InSync(output) => {
                *last_answer = LastAnswer::InSync;
                NexusSnapshot::InSync(output.clone())
            }
            NexusSnapshot::FellOutOfSync => {
                *last_answer = LastAnswer::NotInSync;
                NexusSnapshot::FellOutOfSync
            }
            NexusSnapshot::OutOfSync => {
                *last_answer = LastAnswer::NotInSync;
                NexusSnapshot::OutOfSync
            }
        }
    }

    fn log_answer(
        &self,
        answer: &NexusSnapshot<&Output>,
        last_answer: LastAnswer,
        block_lag: Option<u64>,
    ) {
        match (answer, last_answer) {
            (NexusSnapshot::FellOutOfSync, _) => match block_lag {
                Some(lag) => warn!(
                    computation = self.computation,
                    lag,
                    max_block_lag = self.max_block_lag,
                    "fynd-nexus snapshot is out of sync, computing locally"
                ),
                None => warn!(
                    computation = self.computation,
                    "no snapshot from fynd-nexus, computing locally"
                ),
            },
            (NexusSnapshot::InSync(_), LastAnswer::NotInSync) => {
                info!(computation = self.computation, "fynd-nexus is in sync again, using it");
            }
            (NexusSnapshot::InSync(_), LastAnswer::Nothing | LastAnswer::InSync) |
            (NexusSnapshot::OutOfSync, _) => {}
        }
    }

    /// Exports the gauges of the held snapshot and warns when the missing ids change. The lag is
    /// `None` while the market has no block.
    fn report_held_snapshot(
        &self,
        cached: &CachedSnapshot<Output>,
        lag: Option<u64>,
        missing: &[String],
    ) {
        if let Some(lag) = lag {
            gauge!("nexus_derived_block_lag", "computation" => self.computation).set(lag as f64);
        }
        gauge!("nexus_derived_snapshot_age_seconds", "computation" => self.computation).set(
            cached
                .received_at
                .elapsed()
                .as_secs_f64(),
        );
        gauge!("nexus_derived_missing_components", "computation" => self.computation)
            .set(missing.len() as f64);
        self.warn_if_missing_changed(missing);
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

/// Whether the snapshot is in sync, given what the last call answered, the blocks in a row the
/// snapshot was within the lag and `within_lag`, the snapshot when it is within the lag now.
///
/// The first call and a call after an in-sync one use a snapshot that is within the lag. After
/// an out-of-sync call, the snapshot must be within the lag for [`IN_SYNC_BLOCKS_TO_RESUME`]
/// blocks in a row. The first call without such a snapshot counts as falling out of sync, since
/// the store holds no local data yet.
fn classify<Output>(
    last_answer: LastAnswer,
    in_sync_blocks: u32,
    within_lag: Option<Output>,
) -> NexusSnapshot<Output> {
    match (within_lag, last_answer) {
        (Some(output), LastAnswer::Nothing | LastAnswer::InSync) => NexusSnapshot::InSync(output),
        (Some(output), LastAnswer::NotInSync) if in_sync_blocks >= IN_SYNC_BLOCKS_TO_RESUME => {
            NexusSnapshot::InSync(output)
        }
        (Some(_) | None, LastAnswer::NotInSync) => NexusSnapshot::OutOfSync,
        (None, LastAnswer::Nothing | LastAnswer::InSync) => NexusSnapshot::FellOutOfSync,
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
    use std::collections::HashMap;

    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use rstest::rstest;
    use serde_json::json;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{setup_market_weighted, token, MockProtocolSim},
        derived::{computations::components_without_entries, types::SpotPrices},
        nexus::derived::{SpotPriceEntry, SpotPriceSnapshot},
    };

    const TOKEN: &str = "0x0101010101010101010101010101010101010101";

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

    fn snapshot_body(block: u64) -> serde_json::Value {
        json!({"block": block, "entries": [{
            "component_id": "pool", "token_in": TOKEN, "token_out": TOKEN, "price": 1.5,
        }]})
    }

    async fn fetch_from(
        response: ResponseTemplate,
        known_block: Option<u64>,
    ) -> Result<Option<DerivedSnapshot<SpotPriceEntry>>, DerivedFetchError> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/derived/spot_prices"))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = NexusClient::new(&server.uri()).unwrap();

        fetch_derived_snapshot(&client, "spot_prices", known_block).await
    }

    #[tokio::test]
    async fn test_fetch_snapshot_ok() {
        let response = ResponseTemplate::new(200).set_body_json(snapshot_body(7));

        let snapshot = fetch_from(response, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(snapshot.block, 7);
        assert_eq!(snapshot.entries.len(), 1);
    }

    #[rstest]
    #[case::not_modified(304)]
    #[case::no_snapshot_yet(204)]
    #[tokio::test]
    async fn test_fetch_snapshot_none(#[case] status: u16) {
        let snapshot = fetch_from(ResponseTemplate::new(status), Some(7))
            .await
            .unwrap();

        assert!(snapshot.is_none());
    }

    #[tokio::test]
    async fn test_fetch_snapshot_sends_the_known_block_as_etag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/derived/spot_prices"))
            .and(header("if-none-match", "\"7\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let client = NexusClient::new(&server.uri()).unwrap();

        let snapshot = fetch_derived_snapshot::<SpotPriceEntry>(&client, "spot_prices", Some(7))
            .await
            .unwrap();

        assert!(snapshot.is_none());
    }

    #[rstest]
    #[case::not_found(ResponseTemplate::new(404), "HTTP status 404")]
    #[case::server_error(ResponseTemplate::new(503), "HTTP status 503")]
    #[case::not_json(ResponseTemplate::new(200).set_body_string("nope"), "malformed")]
    #[case::wrong_shape(
        ResponseTemplate::new(200).set_body_json(json!({"block": 9, "entries": [{"x": 1}]})),
        "malformed"
    )]
    #[tokio::test]
    async fn test_fetch_snapshot_error(#[case] response: ResponseTemplate, #[case] reason: &str) {
        let error = fetch_from(response, None)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains(reason), "got {error}");
        assert!(error.contains("spot_prices"), "got {error}");
        assert!(error.contains("/v1/derived/spot_prices"), "got {error}");
    }

    fn market() -> MarketData {
        let (eth, usdc, dai) = (token(1, "ETH"), token(2, "USDC"), token(3, "DAI"));
        let (market, _) = setup_market_weighted(vec![
            ("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("eth_dai", &eth, &dai, MockProtocolSim::new(2000.0)),
        ]);
        market
    }

    fn served(block: u64, price: f64, components: &[&str]) -> ResponseTemplate {
        let (eth, usdc) = (token(1, "ETH").address, token(2, "USDC").address);
        let entries: Vec<_> = components
            .iter()
            .map(|component| {
                json!({"component_id": component, "token_in": eth, "token_out": usdc,
                       "price": price})
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({"block": block, "entries": entries}))
    }

    async fn serve(server: &MockServer, response: ResponseTemplate) {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/v1/derived/spot_prices"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn derived_client(server: &MockServer) -> NexusDerivedClient<SpotPrices> {
        NexusDerivedClient::new(NexusClient::new(&server.uri()).unwrap(), "spot_prices", true)
    }

    async fn latest_price(
        client: &NexusDerivedClient<SpotPrices>,
    ) -> Result<Option<f64>, ComputationError> {
        let prices = client
            .get_latest_snapshot::<SpotPriceEntry>(
                &market(),
                SpotPriceSnapshot::into_spot_prices,
                |market, prices| {
                    components_without_entries(market, prices.keys().map(|key| key.0.as_str()))
                },
            )
            .await?;
        Ok(prices.values().next().copied())
    }

    /// The value of every metric by name. Reading drains the recorder.
    fn metrics(recorder: &DebuggingRecorder) -> HashMap<String, f64> {
        let mut values = HashMap::new();
        for (key, _, _, value) in recorder
            .snapshotter()
            .snapshot()
            .into_vec()
        {
            match value {
                DebugValue::Gauge(gauge) => {
                    values.insert(key.key().name().to_string(), gauge.into_inner());
                }
                DebugValue::Counter(count) => {
                    values.insert(key.key().name().to_string(), count as f64);
                }
                DebugValue::Histogram(_) => {}
            }
        }
        values
    }

    #[tokio::test]
    async fn test_first_fetch_failing_is_no_valid_result() {
        let recorder = DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let server = MockServer::start().await;
        serve(&server, ResponseTemplate::new(503)).await;

        let result = latest_price(&derived_client(&server)).await;

        assert!(matches!(result, Err(ComputationError::NoValidResult { .. })), "{result:?}");
        assert_eq!(metrics(&recorder)["nexus_client_failed_polls_total"], 1.0);
    }

    #[rstest]
    #[case::newer_block(7, 8, 2.0)]
    #[case::equal_block(7, 7, 1.0)]
    #[case::older_block(7, 5, 1.0)]
    #[tokio::test]
    async fn test_second_snapshot_replaces_only_a_newer_block(
        #[case] first_block: u64,
        #[case] second_block: u64,
        #[case] expected_price: f64,
    ) {
        let server = MockServer::start().await;
        let client = derived_client(&server);
        serve(&server, served(first_block, 1.0, &["eth_usdc"])).await;
        latest_price(&client).await.unwrap();
        serve(&server, served(second_block, 2.0, &["eth_usdc"])).await;

        let price = latest_price(&client).await.unwrap();

        assert_eq!(price, Some(expected_price));
    }

    #[rstest]
    #[case::unchanged(ResponseTemplate::new(304), false)]
    #[case::error_status(ResponseTemplate::new(500), true)]
    #[tokio::test]
    async fn test_cached_output_survives_a_fetch_without_a_snapshot(
        #[case] response: ResponseTemplate,
        #[case] counted: bool,
    ) {
        let recorder = DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let server = MockServer::start().await;
        let client = derived_client(&server);
        serve(&server, served(7, 1.0, &["eth_usdc"])).await;
        latest_price(&client).await.unwrap();
        serve(&server, response).await;

        let price = latest_price(&client).await.unwrap();

        assert_eq!(price, Some(1.0));
        let failed = metrics(&recorder).contains_key("nexus_client_failed_polls_total");
        assert_eq!(failed, counted);
    }

    #[rstest]
    #[case::complete(&["eth_usdc", "eth_dai"], 0.0)]
    #[case::one_component_missing(&["eth_usdc"], 1.0)]
    #[case::none_listed(&[], 2.0)]
    #[tokio::test]
    async fn test_missing_components_and_block_lag_metrics(
        #[case] listed: &[&str],
        #[case] missing: f64,
    ) {
        let recorder = DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let market_block = market()
            .read()
            .await
            .last_updated()
            .unwrap()
            .number();
        let server = MockServer::start().await;
        serve(&server, served(market_block + 3, 1.0, listed)).await;

        latest_price(&derived_client(&server))
            .await
            .unwrap();

        let metrics = metrics(&recorder);
        assert_eq!(metrics["nexus_derived_missing_components"], missing);
        assert_eq!(metrics["nexus_derived_block_lag"], -3.0);
        assert!(metrics.contains_key("nexus_derived_snapshot_age_seconds"));
    }
}
