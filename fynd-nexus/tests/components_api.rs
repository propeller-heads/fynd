mod common;

use std::{net::SocketAddr, sync::Arc};

use common::{TestMonitor, EPOCH};
use fynd_core::nexus::{
    components::{excluded_path, ExcludedComponents, ExclusionCause},
    MessageBody, NexusMessage,
};
use fynd_nexus::api::{self, Streams};
use reqwest::{header, Method, Response};
use rstest::rstest;
use serde_json::{json, Value};

fn serve(test_monitor: &TestMonitor) -> SocketAddr {
    let streams = Arc::new(Streams::new([Arc::clone(&test_monitor.stream)]));
    let (server, address) =
        api::bind(streams, Some(Arc::clone(&test_monitor.monitor)), "127.0.0.1:0".parse().unwrap())
            .unwrap();
    tokio::spawn(server);
    address
}

async fn request(
    address: SocketAddr,
    method: Method,
    path: &str,
    if_none_match: Option<&str>,
) -> Response {
    let mut request = reqwest::Client::new().request(method, format!("http://{address}{path}"));
    if let Some(etag) = if_none_match {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    request.send().await.unwrap()
}

async fn get_json(address: SocketAddr, path: &str) -> (u16, Value) {
    let response = request(address, Method::GET, path, None).await;
    (response.status().as_u16(), response.json().await.unwrap())
}

fn etag_of(response: &Response) -> String {
    response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_string()
}

async fn excluded_ids(response: Response) -> Vec<(String, ExclusionCause)> {
    let message: NexusMessage<ExcludedComponents> = response.json().await.unwrap();
    let MessageBody::Snapshot { data, .. } = message.body else {
        panic!("expected a snapshot");
    };
    data.components
        .into_iter()
        .map(|component| (component.id, component.cause))
        .collect()
}

#[tokio::test]
async fn test_excluded_stream_has_no_data_before_the_first_verdict() {
    let address = serve(&TestMonitor::new(&["0xa"]));

    let response = request(address, Method::GET, excluded_path(), None).await;

    assert_eq!(response.status(), 204);
}

#[tokio::test]
async fn test_excluded_stream_serves_the_broken_components_with_an_etag() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb"]);
    test_monitor.break_component("0xb");
    let address = serve(&test_monitor);

    let response = request(address, Method::GET, excluded_path(), None).await;

    assert_eq!(response.status(), 200);
    assert_eq!(etag_of(&response), format!("\"{EPOCH}-1\""));
    assert_eq!(excluded_ids(response).await, [("0xb".to_string(), ExclusionCause::Broken)]);
}

#[tokio::test]
async fn test_excluded_stream_answers_304_until_the_set_changes() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb"]);
    test_monitor.break_component("0xa");
    let address = serve(&test_monitor);
    let first = etag_of(&request(address, Method::GET, excluded_path(), None).await);

    let unchanged = request(address, Method::GET, excluded_path(), Some(&first)).await;
    test_monitor.break_component("0xb");
    let changed = request(address, Method::GET, excluded_path(), Some(&first)).await;

    assert_eq!(unchanged.status(), 304);
    assert_eq!(changed.status(), 200);
    assert_ne!(etag_of(&changed), first);
    assert_eq!(excluded_ids(changed).await.len(), 2);
}

#[tokio::test]
async fn test_report_lists_every_component_and_filters() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb", "0xc"]);
    test_monitor.break_component("0xb");
    let address = serve(&test_monitor);
    let ids = |body: &Value| -> Vec<String> {
        body["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|component| {
                component["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };

    let (status, all) = get_json(address, "/v1/components").await;
    let (_, broken) = get_json(address, "/v1/components?status=broken").await;
    let (_, none) = get_json(address, "/v1/components?protocol=uniswap_v2").await;
    let (_, both) = get_json(address, "/v1/components?status=unknown&protocol=uniswap_v3").await;

    assert_eq!(status, 200);
    assert_eq!(ids(&all), ["0xa", "0xb", "0xc"]);
    assert_eq!(ids(&broken), ["0xb"]);
    assert!(ids(&none).is_empty());
    assert_eq!(ids(&both), ["0xa", "0xc"]);
    let record = &broken["components"][0];
    assert_eq!(record["status"], "broken");
    assert_eq!(record["protocol_system"], "uniswap_v3");
    assert_eq!(record["last_outcome"], "pool_reverted");
    assert_eq!(record["reason"], "PoolEmpty");
    assert_eq!(record["block"], 2);
    assert_eq!(record["consecutive_pool_reverts"], 2);
    assert!(record["next_check_at"].is_string());
    assert!(record["checked_at"].is_string());
}

#[tokio::test]
async fn test_report_rejects_an_unknown_status() {
    let address = serve(&TestMonitor::new(&["0xa"]));

    let response = request(address, Method::GET, "/v1/components?status=sleepy", None).await;

    assert_eq!(response.status(), 400);
}

#[rstest]
#[case::known("/v1/components/0xa", 200)]
#[case::upper_case("/v1/components/0xA", 200)]
#[case::unknown("/v1/components/0xz", 404)]
#[tokio::test]
async fn test_one_component(#[case] path: &str, #[case] status: u16) {
    let address = serve(&TestMonitor::new(&["0xa"]));

    let response = request(address, Method::GET, path, None).await;

    assert_eq!(response.status(), status);
    if status == 200 {
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["id"], "0xa");
        assert_eq!(body["status"], "unknown");
    }
}

#[tokio::test]
async fn test_reset_one_clears_it_and_publishes_the_new_set() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb"]);
    test_monitor.break_component("0xa");
    test_monitor.break_component("0xb");
    let address = serve(&test_monitor);

    let reset = request(address, Method::DELETE, "/v1/components/0xa/health", None).await;
    let unknown = request(address, Method::DELETE, "/v1/components/0xz/health", None).await;
    let excluded = request(address, Method::GET, excluded_path(), None).await;

    assert_eq!(reset.status(), 200);
    assert_eq!(reset.json::<Value>().await.unwrap(), json!({"reset": 1}));
    assert_eq!(unknown.status(), 404);
    assert_eq!(excluded_ids(excluded).await, [("0xb".to_string(), ExclusionCause::Broken)]);
    let (_, record) = get_json(address, "/v1/components/0xa").await;
    assert_eq!(record["status"], "unknown");
    assert!(record["checked_at"].is_null());
}

#[tokio::test]
async fn test_reset_all_clears_every_component_and_publishes_an_empty_set() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb"]);
    test_monitor.break_component("0xa");
    test_monitor.break_component("0xb");
    let address = serve(&test_monitor);

    let reset = request(address, Method::DELETE, "/v1/components/health", None).await;
    let excluded = request(address, Method::GET, excluded_path(), None).await;

    assert_eq!(reset.status(), 200);
    assert_eq!(reset.json::<Value>().await.unwrap(), json!({"reset": 2}));
    assert!(excluded_ids(excluded).await.is_empty());
}

#[tokio::test]
async fn test_component_routes_are_absent_without_a_monitor() {
    let test_monitor = TestMonitor::new(&["0xa"]);
    let streams = Arc::new(Streams::new([Arc::clone(&test_monitor.stream)]));
    let (server, address) = api::bind(streams, None, "127.0.0.1:0".parse().unwrap()).unwrap();
    tokio::spawn(server);

    let report = request(address, Method::GET, "/v1/components", None).await;
    let reset = request(address, Method::DELETE, "/v1/components/health", None).await;

    assert_eq!((report.status().as_u16(), reset.status().as_u16()), (404, 404));
}

#[tokio::test]
async fn test_health_reports_the_monitor() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb"]);
    test_monitor.break_component("0xa");
    let address = serve(&test_monitor);

    let (status, body) = get_json(address, "/v1/health").await;

    assert_eq!(status, 200);
    assert_eq!(body["serving"], json!(["excluded_components"]));
    assert_eq!(body["monitor"]["components"], 2);
    assert_eq!(body["monitor"]["by_status"]["broken"], 1);
    assert_eq!(body["monitor"]["by_status"]["unknown"], 1);
    assert_eq!(body["monitor"]["stalled"], false);
}
