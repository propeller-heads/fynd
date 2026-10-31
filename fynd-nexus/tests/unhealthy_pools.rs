//! Fynd Nexus's monitor and a pod-side poller of the excluded components, reading over a local
//! socket.

mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use common::TestMonitor;
use fynd_core::{feed::market_data::MarketData, nexus::client::NexusClient};
use fynd_nexus::api::{self, Streams};
use tokio::time::{sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(15);

fn serve(test_monitor: &TestMonitor) -> SocketAddr {
    let streams = Arc::new(Streams::new([Arc::clone(&test_monitor.stream)]));
    let (server, address) =
        api::bind(streams, Some(Arc::clone(&test_monitor.monitor)), "127.0.0.1:0".parse().unwrap())
            .unwrap();
    tokio::spawn(server);
    address
}

fn start_pod(service: SocketAddr) -> MarketData {
    let market = MarketData::new_shared();
    let client = NexusClient::new(&format!("http://{service}")).unwrap();
    let poller = client.unhealthy_pools_poller(market.clone(), Duration::from_secs(600));
    tokio::spawn(poller.run());
    market
}

/// Waits until the pod's view of `component_id` being unhealthy is `expected`.
async fn wait_for_pod(market: &MarketData, component_id: &str, expected: bool) {
    timeout(DEADLINE, async {
        loop {
            let unhealthy = market
                .read()
                .await
                .base_market_state()
                .is_unhealthy_pool(component_id);
            if unhealthy == expected {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("the pod did not see {component_id} as unhealthy={expected} within the deadline")
    });
}

#[tokio::test]
async fn test_pod_excludes_a_broken_component_and_routes_through_it_after_a_reset() {
    let test_monitor = TestMonitor::new(&["0xa", "0xb"]);
    let address = serve(&test_monitor);
    let market = start_pod(address);
    test_monitor.break_component("0xb");

    wait_for_pod(&market, "0xb", true).await;
    assert!(!market
        .read()
        .await
        .base_market_state()
        .is_unhealthy_pool("0xa"));
    let reset = reqwest::Client::new()
        .delete(format!("http://{address}/v1/components/0xb/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);

    wait_for_pod(&market, "0xb", false).await;
}

#[tokio::test]
async fn test_pod_stops_excluding_when_the_service_withdraws_the_stream() {
    let test_monitor = TestMonitor::new(&["0xa"]);
    let address = serve(&test_monitor);
    let market = start_pod(address);
    test_monitor.break_component("0xa");
    wait_for_pod(&market, "0xa", true).await;

    test_monitor
        .monitor
        .publisher
        .withdraw();

    wait_for_pod(&market, "0xa", false).await;
}
