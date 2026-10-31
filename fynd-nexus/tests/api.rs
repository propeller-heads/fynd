mod common;

use std::{net::SocketAddr, sync::Arc};

use common::{new_latest_stream, EPOCH};
use fynd_core::nexus::{
    rfq::{prices_path, RfqVenue},
    MessageBody, NexusMessage,
};
use fynd_nexus::{
    api::{self, Streams},
    latest_stream::LatestStream,
};
use reqwest::{header, Response};
use rstest::rstest;
use serde_json::{json, Value};

/// The API on a local port, serving `streams`.
fn serve(streams: &[Arc<LatestStream>]) -> SocketAddr {
    let streams = Arc::new(Streams::new(streams.iter().cloned()));
    let (server, address) = api::bind(streams, None, "127.0.0.1:0".parse().unwrap()).unwrap();
    tokio::spawn(server);
    address
}

fn bebop() -> Arc<LatestStream> {
    new_latest_stream(RfqVenue::Bebop)
}

async fn get(address: SocketAddr, path: &str, if_none_match: Option<&str>) -> Response {
    let mut request = reqwest::Client::new().get(format!("http://{address}{path}"));
    if let Some(etag) = if_none_match {
        request = request.header(header::IF_NONE_MATCH, etag);
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

    let response = get(address, "/v1/rfq/bebop", None).await;

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

    let response = get(address, "/v1/rfq/bebop", None).await;

    assert_eq!(response.status(), 200);
    assert_eq!(etag_of(&response), format!("\"{EPOCH}-1\""));
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let NexusMessage { epoch, seq, body: MessageBody::Snapshot { data, .. }, .. } = response
        .json::<NexusMessage<Value>>()
        .await
        .unwrap()
    else {
        panic!("expected a snapshot");
    };
    assert_eq!((epoch, seq, data), (EPOCH, 1, json!({"pairs": [1]})));
}

#[tokio::test]
async fn test_etag_round_trip() {
    let state = bebop();
    state.publish(&json!({"n": 1}), chrono::Utc::now());
    let address = serve(std::slice::from_ref(&state));
    let path = "/v1/rfq/bebop";
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
    assert_eq!(second, format!("\"{EPOCH}-2\""));
    assert_eq!(withdrawn.status(), 200);
    assert_eq!(
        withdrawn
            .json::<NexusMessage<Value>>()
            .await
            .unwrap(),
        NexusMessage::new(EPOCH, 3, MessageBody::Withdrawn)
    );
}

#[rstest]
#[case::unknown_venue("/v1/rfq/liquorice")]
#[case::venue_not_served("/v1/rfq/metric")]
#[tokio::test]
async fn test_request_unknown_stream(#[case] path: &str) {
    let address = serve(&[bebop()]);

    let response = get(address, path, None).await;

    assert_eq!(response.status(), 404);
}

#[rstest]
#[case::bebop(RfqVenue::Bebop)]
#[case::metric(RfqVenue::Metric)]
#[tokio::test]
async fn test_prices_path_routing(#[case] venue: RfqVenue) {
    let state = new_latest_stream(venue);
    state.publish(&json!({}), chrono::Utc::now());
    let address = serve(&[state]);

    let response = get(address, &prices_path(venue), None).await;

    assert_eq!(response.status(), 200);
}

#[rstest]
#[case::none_is_serving(false, false, 503, json!([]))]
#[case::one_is_serving(true, false, 200, json!(["metric"]))]
#[case::withdrawn_again(true, true, 503, json!([]))]
#[tokio::test]
async fn test_health_follows_whether_any_stream_is_serving(
    #[case] publish: bool,
    #[case] withdraw: bool,
    #[case] status: u16,
    #[case] serving: Value,
) {
    let bebop = bebop();
    let metric = new_latest_stream(RfqVenue::Metric);
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
