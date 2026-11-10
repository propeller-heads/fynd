mod common;

use std::{io::Read as _, net::SocketAddr, sync::Arc};

use common::{new_latest_prices, RUN_ID};
use fynd_core::nexus::{
    derived::{DerivedKind, DerivedSnapshot},
    price_levels::{prices_path, PriceLevelVenue},
    MessageBody, NexusMessage,
};
use fynd_nexus::{
    api::{self, Streams},
    derived::snapshot::DerivedSnapshots,
    latest_prices::LatestPrices,
};
use reqwest::{header, Response};
use rstest::rstest;
use serde_json::{json, Value};
use tycho_simulation::evm::tycho_models::Chain;

/// The API on a local port, serving `streams`.
fn serve(streams: &[Arc<LatestPrices>]) -> SocketAddr {
    serve_with(streams, None)
}

/// The API on a local port, serving `streams` and `derived`.
fn serve_with(streams: &[Arc<LatestPrices>], derived: Option<Arc<DerivedSnapshots>>) -> SocketAddr {
    let streams = Arc::new(Streams::new(streams.iter().cloned()));
    let (server, address) = api::bind(streams, derived, "127.0.0.1:0".parse().unwrap()).unwrap();
    tokio::spawn(server);
    address
}

fn bebop() -> Arc<LatestPrices> {
    new_latest_prices(PriceLevelVenue::Bebop)
}

async fn get(address: SocketAddr, path: &str, if_none_match: Option<&str>) -> Response {
    get_with(address, path, header::IF_NONE_MATCH, if_none_match).await
}

async fn get_with(
    address: SocketAddr,
    path: &str,
    name: header::HeaderName,
    value: Option<&str>,
) -> Response {
    let mut request = reqwest::Client::new().get(format!("http://{address}{path}"));
    if let Some(value) = value {
        request = request.header(name, value);
    }
    request.send().await.unwrap()
}

fn etag_of(response: &Response) -> String {
    response
        .headers()
        .get(header::ETAG)
        .expect("the response has an ETag")
        .to_str()
        .unwrap()
        .to_string()
}

async fn get_json(address: SocketAddr, path: &str) -> (u16, Value) {
    let response = get(address, path, None).await;
    (response.status().as_u16(), response.json().await.unwrap())
}

#[tokio::test]
async fn test_request_before_first_snapshot() {
    let address = serve(&[bebop()]);

    let response = get(address, "/v1/price-levels/bebop", None).await;

    assert_eq!(response.status(), 204);
    assert!(response
        .bytes()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_snapshot_response() {
    let state = bebop();
    state.publish(&json!({"pairs": [1]}), chrono::Utc::now());
    let address = serve(&[state]);

    let response = get(address, "/v1/price-levels/bebop", None).await;

    assert_eq!(response.status(), 200);
    assert_eq!(etag_of(&response), format!("\"{RUN_ID}-1\""));
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let NexusMessage { run_id, seq, body: MessageBody::Snapshot { data, .. }, .. } = response
        .json::<NexusMessage<Value>>()
        .await
        .unwrap()
    else {
        panic!("expected a snapshot");
    };
    assert_eq!((run_id, seq, data), (RUN_ID, 1, json!({"pairs": [1]})));
}

#[tokio::test]
async fn test_etag_round_trip() {
    let state = bebop();
    state.publish(&json!({"n": 1}), chrono::Utc::now());
    let address = serve(std::slice::from_ref(&state));
    let path = "/v1/price-levels/bebop";
    let first = etag_of(&get(address, path, None).await);

    let unchanged = get(address, path, Some(&first)).await;
    let other = get(address, path, Some("\"1-1\"")).await;
    state.publish(&json!({"n": 2}), chrono::Utc::now());
    let changed = get(address, path, Some(&first)).await;
    let second = etag_of(&changed);
    state.withdraw();
    let withdrawn = get(address, path, Some(&second)).await;

    assert_eq!(unchanged.status(), 304);
    assert_eq!(etag_of(&unchanged), first);
    assert!(unchanged
        .bytes()
        .await
        .unwrap()
        .is_empty());
    assert_eq!(other.status(), 200, "a different ETag gets the message");
    assert_eq!(changed.status(), 200);
    assert_eq!(second, format!("\"{RUN_ID}-2\""));
    assert_eq!(withdrawn.status(), 200);
    assert_eq!(
        withdrawn
            .json::<NexusMessage<Value>>()
            .await
            .unwrap(),
        NexusMessage::new(RUN_ID, 3, MessageBody::Withdrawn)
    );
}

#[rstest]
#[case::unknown_venue("/v1/price-levels/liquorice")]
#[case::venue_not_served("/v1/price-levels/metric")]
#[tokio::test]
async fn test_request_unknown_stream(#[case] path: &str) {
    let address = serve(&[bebop()]);

    let response = get(address, path, None).await;

    assert_eq!(response.status(), 404);
}

#[rstest]
#[case::bebop(PriceLevelVenue::Bebop)]
#[case::metric(PriceLevelVenue::Metric)]
#[tokio::test]
async fn test_prices_path_routing(#[case] venue: PriceLevelVenue) {
    let state = new_latest_prices(venue);
    state.publish(&json!({}), chrono::Utc::now());
    let address = serve(&[state]);

    let response = get(address, &prices_path(venue), None).await;

    assert_eq!(response.status(), 200);
}

#[rstest]
#[case::none_has_prices(false, false, 503, json!([]))]
#[case::one_has_prices(true, false, 200, json!(["metric"]))]
#[case::withdrawn_again(true, true, 503, json!([]))]
#[tokio::test]
async fn test_health_follows_whether_any_stream_has_prices(
    #[case] publish: bool,
    #[case] withdraw: bool,
    #[case] status: u16,
    #[case] serving: Value,
) {
    let bebop = bebop();
    let metric = new_latest_prices(PriceLevelVenue::Metric);
    let address = serve(&[bebop, metric.clone()]);
    if publish {
        metric.publish(&json!({}), chrono::Utc::now());
    }
    if withdraw {
        metric.withdraw();
    }

    let (actual_status, body) = get_json(address, "/v1/health").await;

    assert_eq!(actual_status, status);
    assert_eq!(body, json!({ "serving": serving, "streams": ["bebop", "metric"] }));
}

/// A store with a snapshot at block 5 of each of `kinds`.
fn derived_with(kinds: &[DerivedKind]) -> Arc<DerivedSnapshots> {
    let derived = Arc::new(DerivedSnapshots::new(Chain::Ethereum));
    for kind in kinds {
        derived.replace(*kind, &DerivedSnapshot { block: 5, entries: vec![json!({"n": 1})] });
    }
    derived
}

#[rstest]
#[case::unknown_kind("/v1/derived/nope", true, true, None, 404)]
#[case::market_off("/v1/derived/pool_depths", false, true, None, 404)]
#[case::no_snapshot_yet("/v1/derived/pool_depths", true, false, None, 204)]
#[case::snapshot("/v1/derived/pool_depths", true, true, None, 200)]
#[case::same_block("/v1/derived/pool_depths", true, true, Some("\"5\""), 304)]
#[case::other_block("/v1/derived/pool_depths", true, true, Some("\"4\""), 200)]
#[tokio::test]
async fn test_derived_status_follows_market_snapshot_and_etag(
    #[case] path: &str,
    #[case] market: bool,
    #[case] written: bool,
    #[case] if_none_match: Option<&str>,
    #[case] status: u16,
) {
    let kinds: &[DerivedKind] = if written { &[DerivedKind::PoolDepths] } else { &[] };
    let address = serve_with(&[], market.then(|| derived_with(kinds)));

    let response = get(address, path, if_none_match).await;

    assert_eq!(response.status(), status);
    if status == 200 || status == 304 {
        assert_eq!(etag_of(&response), "\"5\"");
        assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
    }
    let body = response.bytes().await.unwrap();
    if status == 200 {
        let snapshot: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(snapshot, json!({"block": 5, "entries": [{"n": 1}]}));
    } else if status != 404 {
        assert!(body.is_empty());
    }
}

#[rstest]
#[case::no_header(None, false)]
#[case::gzip(Some("gzip"), true)]
#[case::gzip_among_others(Some("br, gzip;q=0.5"), true)]
#[case::gzip_refused(Some("gzip;q=0"), false)]
#[case::identity_only(Some("identity"), false)]
#[tokio::test]
async fn test_derived_body_is_gzip_only_when_accepted(
    #[case] accept_encoding: Option<&str>,
    #[case] gzipped: bool,
) {
    let address = serve_with(&[], Some(derived_with(&[DerivedKind::SpotPrices])));

    let response =
        get_with(address, "/v1/derived/spot_prices", header::ACCEPT_ENCODING, accept_encoding)
            .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
    assert_eq!(
        response
            .headers()
            .contains_key(header::CONTENT_ENCODING),
        gzipped
    );
    let body = response.bytes().await.unwrap();
    let mut plain = String::new();
    if gzipped {
        flate2::read::GzDecoder::new(&body[..])
            .read_to_string(&mut plain)
            .unwrap();
    } else {
        plain = String::from_utf8(body.to_vec()).unwrap();
    }
    assert_eq!(serde_json::from_str::<Value>(&plain).unwrap()["block"], 5);
}

#[rstest]
#[case::nothing_computed(&[], 503, &[])]
#[case::some_computed(&[DerivedKind::SpotPrices], 503, &["spot_prices"])]
#[case::all_computed(&DerivedKind::ALL, 200, &["spot_prices", "pool_depths", "token_prices"])]
#[tokio::test]
async fn test_market_only_health_follows_every_derived_kind(
    #[case] written: &[DerivedKind],
    #[case] status: u16,
    #[case] serving: &[&str],
) {
    let address = serve_with(&[], Some(derived_with(written)));

    let (actual_status, body) = get_json(address, "/v1/health").await;

    assert_eq!(actual_status, status);
    assert_eq!(
        body,
        json!({
            "serving": [],
            "streams": [],
            "derived": {
                "serving": serving,
                "computations": ["spot_prices", "pool_depths", "token_prices"],
            },
        })
    );
}

#[tokio::test]
async fn test_health_with_price_levels_ignores_derived_snapshots() {
    let address = serve_with(&[bebop()], Some(derived_with(&DerivedKind::ALL)));

    let (status, body) = get_json(address, "/v1/health").await;

    assert_eq!(status, 503);
    assert_eq!(
        body["derived"]["serving"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}
