//! Integration tests for `fynd-client`.
//!
//! Each test spins up an in-process mock HTTP server (wiremock) and verifies the full
//! request/response round-trip of the [`FyndClient`] public API.
//!
//! The alloy mock transport is used for Ethereum RPC calls so that tests run without
//! any real network access.

use std::time::Duration;

use alloy::{
    consensus::TypedTransaction,
    network::Ethereum,
    primitives::Address,
    providers::{ProviderBuilder, RootProvider},
};
use fynd_client::{
    ErrorCode, FyndClient, FyndError, Order, OrderSide, QuoteOptions, QuoteParams, RetryConfig,
    SignablePayload, SigningHints,
};
use num_bigint::BigUint;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

// ============================================================================
// Shared helpers
// ============================================================================

/// Build a [`FyndClient<RootProvider<Ethereum>>`] pointing at `base_url` backed by an
/// alloy mock transport, so that Ethereum RPC calls are handled in-process.
fn make_client(
    base_url: String,
    retry: RetryConfig,
    default_sender: Option<Address>,
) -> (FyndClient<RootProvider<Ethereum>>, alloy::providers::mock::Asserter) {
    use alloy::providers::mock::Asserter;

    let asserter = Asserter::new();
    let provider = ProviderBuilder::default().connect_mocked_client(asserter.clone());
    let submit_provider = ProviderBuilder::default().connect_mocked_client(asserter.clone());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");

    let client = FyndClient::new_with_providers(
        http,
        base_url,
        retry,
        Address::ZERO, // router address
        1,             // chain_id
        default_sender,
        provider,
        submit_provider,
    );
    (client, asserter)
}

/// Construct a minimal `QuoteParams` with one sell order.
fn make_quote_params() -> QuoteParams {
    let token_in = bytes::Bytes::copy_from_slice(&[0xaa; 20]);
    let token_out = bytes::Bytes::copy_from_slice(&[0xbb; 20]);
    let sender = bytes::Bytes::copy_from_slice(&[0xcc; 20]);

    let order =
        Order::new(token_in, token_out, BigUint::from(1_000_000u64), OrderSide::Sell, sender, None);
    QuoteParams::new(order, QuoteOptions::default())
}

/// Return a minimal valid wire `Solution` JSON with one order.
fn minimal_solution_json(order_id: &str) -> serde_json::Value {
    serde_json::json!({
        "orders": [{
            "order_id": order_id,
            "status": "success",
            "amount_in": "1000000",
            "amount_out": "990000",
            "gas_estimate": "50000",
            "amount_out_net_gas": "940000",
            "price_impact_bps": 10,
            "block": {
                "number": 1234567,
                "hash": "0xabcdef",
                "timestamp": 1700000000
            }
        }],
        "total_gas_estimate": "50000",
        "solve_time_ms": 42
    })
}

// ============================================================================
// Integration tests
// ============================================================================

/// Happy path: single-order quote round-trip.
#[tokio::test]
async fn full_quote_roundtrip() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(minimal_solution_json("order-1")))
        .expect(1)
        .mount(&server)
        .await;

    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);
    let quote = client
        .quote(make_quote_params())
        .await
        .expect("quote should succeed");

    assert_eq!(quote.order_id(), "order-1");
    assert_eq!(quote.amount_out(), &BigUint::from(990_000u64));
    assert_eq!(quote.amount_in(), &BigUint::from(1_000_000u64));
    assert_eq!(quote.gas_estimate(), &BigUint::from(50_000u64));
    assert_eq!(quote.price_impact_bps(), Some(10));

    // token_out and receiver should be populated from the request Order.
    assert_eq!(quote.token_out(), &bytes::Bytes::copy_from_slice(&[0xbb; 20]));
    // receiver defaults to sender when not explicitly set.
    assert_eq!(quote.receiver(), &bytes::Bytes::copy_from_slice(&[0xcc; 20]));

    server.verify().await;
}

/// Health endpoint happy path.
#[tokio::test]
async fn health_roundtrip() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "healthy": true,
            "last_update_ms": 250,
            "num_solver_pools": 3
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);
    let health = client
        .health()
        .await
        .expect("health should succeed");

    assert!(health.healthy());
    assert_eq!(health.last_update_ms(), 250);
    assert_eq!(health.num_solver_pools(), 3);

    server.verify().await;
}

/// Quote retries once on a transient error, then succeeds.
#[tokio::test]
async fn quote_retries_once_then_succeeds() {
    let server = MockServer::start().await;

    // First request fails with a retryable error.
    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "service overloaded",
            "code": "SERVICE_OVERLOADED"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Second request succeeds.
    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(minimal_solution_json("retry-ok")))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let retry = RetryConfig::new(3, Duration::from_millis(1), Duration::from_millis(5));
    let (client, _asserter) = make_client(server.uri(), retry, None);

    let quote = client
        .quote(make_quote_params())
        .await
        .expect("quote should succeed after one retry");

    assert_eq!(quote.order_id(), "retry-ok");
}

/// Quote with a multi-hop route: verify all swaps are deserialized correctly.
/// Also verifies that `component_id` is correctly mapped to `pool_id` by the `Swap` mapping.
#[tokio::test]
async fn quote_with_multi_hop_route_deserializes_all_swaps() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "orders": [{
            "order_id": "multihop-1",
            "status": "success",
            "route": {
                "swaps": [
                    {
                        "component_id": "0xpool1111111111111111111111111111111111111111",
                        "protocol": "uniswap_v3",
                        "token_in": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "token_out": "0xcccccccccccccccccccccccccccccccccccccccc",
                        "amount_in": "1000000",
                        "amount_out": "500000",
                        "gas_estimate": "30000"
                    },
                    {
                        "component_id": "0xpool2222222222222222222222222222222222222222",
                        "protocol": "uniswap_v2",
                        "token_in": "0xcccccccccccccccccccccccccccccccccccccccc",
                        "token_out": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "amount_in": "500000",
                        "amount_out": "990000",
                        "gas_estimate": "20000"
                    }
                ]
            },
            "amount_in": "1000000",
            "amount_out": "990000",
            "gas_estimate": "50000",
            "amount_out_net_gas": "940000",
            "price_impact_bps": 15,
            "block": {
                "number": 9999999,
                "hash": "0x1234",
                "timestamp": 1700001000
            }
        }],
        "total_gas_estimate": "50000",
        "solve_time_ms": 15
    });

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;

    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);
    let quote = client
        .quote(make_quote_params())
        .await
        .expect("multi-hop quote should succeed");

    let route = quote
        .route()
        .expect("route should be present");
    assert_eq!(route.swaps().len(), 2);

    let first = &route.swaps()[0];
    // component_id in wire format maps to pool_id in the client type.
    assert_eq!(first.component_id(), "0xpool1111111111111111111111111111111111111111");
    assert_eq!(first.protocol(), "uniswap_v3");
    assert_eq!(first.amount_in(), &BigUint::from(1_000_000u64));
    assert_eq!(first.amount_out(), &BigUint::from(500_000u64));

    let second = &route.swaps()[1];
    assert_eq!(second.component_id(), "0xpool2222222222222222222222222222222222222222");
    assert_eq!(second.protocol(), "uniswap_v2");
    assert_eq!(second.amount_in(), &BigUint::from(500_000u64));
    assert_eq!(second.amount_out(), &BigUint::from(990_000u64));

    server.verify().await;
}

/// Verify that a non-retryable API error (400 BAD_REQUEST) surfaces immediately as
/// `FyndError::Api { code: ErrorCode::BadRequest }` without any retry.
#[tokio::test]
async fn quote_bad_request_not_retried() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "no orders provided",
            "code": "BAD_REQUEST"
        })))
        .expect(1) // Must be called exactly once — no retries.
        .mount(&server)
        .await;

    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);
    let err = client
        .quote(make_quote_params())
        .await
        .unwrap_err();

    assert!(
        matches!(err, FyndError::Api { code: ErrorCode::BadRequest, .. }),
        "expected BadRequest, got {err:?}"
    );

    server.verify().await;
}

/// Verify the client correctly populates `token_out` and `receiver` on each `OrderSolution`
/// from the original order list (by index), even when `receiver` is explicit (not sender).
#[tokio::test]
async fn quote_populates_token_out_and_receiver_from_order() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(minimal_solution_json("populated-order")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let token_in = bytes::Bytes::copy_from_slice(&[0x11; 20]);
    let token_out = bytes::Bytes::copy_from_slice(&[0x22; 20]);
    let sender = bytes::Bytes::copy_from_slice(&[0x33; 20]);
    let receiver = bytes::Bytes::copy_from_slice(&[0x44; 20]);

    let order = Order::new(
        token_in,
        token_out.clone(),
        BigUint::from(1_000u64),
        OrderSide::Sell,
        sender,
        Some(receiver.clone()),
    );

    let params = QuoteParams::new(order, QuoteOptions::default());
    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);
    let quote = client
        .quote(params)
        .await
        .expect("quote should succeed");

    assert_eq!(quote.token_out(), &token_out);
    assert_eq!(quote.receiver(), &receiver);

    server.verify().await;
}

/// When `receiver` is `None` on the order, `OrderSolution::receiver()` defaults to `sender`.
#[tokio::test]
async fn quote_receiver_defaults_to_sender_when_none() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(minimal_solution_json("recv-default")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let sender = bytes::Bytes::copy_from_slice(&[0x77; 20]);
    let order = Order::new(
        bytes::Bytes::copy_from_slice(&[0xaa; 20]),
        bytes::Bytes::copy_from_slice(&[0xbb; 20]),
        BigUint::from(1u64),
        OrderSide::Sell,
        sender.clone(),
        None, // no receiver
    );

    let params = QuoteParams::new(order, QuoteOptions::default());
    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);
    let quote = client
        .quote(params)
        .await
        .expect("quote should succeed");

    assert_eq!(quote.receiver(), &sender, "receiver should default to sender");

    server.verify().await;
}

/// Full round-trip: quote, then signable_payload with all hints — no RPC calls needed.
///
/// This test exercises the complete flow that a real user would follow:
/// 1. Call quote() to get an OrderSolution.
/// 2. Call signable_payload() on the solution with explicit hints.
/// 3. Verify the returned EIP-1559 tx has the expected fields.
#[tokio::test]
async fn full_quote_then_signable_payload_with_all_hints() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(minimal_solution_json("flow-order")))
        .expect(1)
        .mount(&server)
        .await;

    let sender = Address::with_last_byte(0xab);
    let (client, _asserter) = make_client(server.uri(), RetryConfig::default(), None);

    let quote = client
        .quote(make_quote_params())
        .await
        .expect("quote should succeed");

    let hints = SigningHints::default()
        .with_sender(sender)
        .with_nonce(7)
        .with_max_fee_per_gas(3_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000)
        .with_gas_limit(80_000);

    let payload = client
        .signable_payload(quote, &hints)
        .await
        .expect("signable_payload should succeed");

    let SignablePayload::Fynd(fynd) = payload else {
        panic!("expected Fynd payload");
    };

    let TypedTransaction::Eip1559(tx) = fynd.tx() else {
        panic!("expected EIP-1559 tx");
    };

    assert_eq!(tx.nonce, 7);
    assert_eq!(tx.max_fee_per_gas, 3_000_000_000);
    assert_eq!(tx.max_priority_fee_per_gas, 1_000_000);
    assert_eq!(tx.gas_limit, 80_000);
    assert_eq!(tx.chain_id, 1);

    server.verify().await;
}

/// Full round-trip: quote, signable_payload resolving nonce/fees from mock provider.
#[tokio::test]
async fn full_quote_then_signable_payload_resolves_from_provider() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/solve"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(minimal_solution_json("provider-flow")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let sender = Address::with_last_byte(0xde);
    let (client, asserter) = make_client(server.uri(), RetryConfig::default(), Some(sender));

    let quote = client
        .quote(make_quote_params())
        .await
        .expect("quote should succeed");

    // Pre-load RPC responses for nonce and fee estimation.
    asserter.push_success(&99u64); // eth_getTransactionCount → nonce
    let fee_history = serde_json::json!({
        "oldestBlock": "0x1",
        "baseFeePerGas": ["0x3b9aca00", "0x3b9aca00"],
        "gasUsedRatio": [0.5],
        "reward": [["0xf4240", "0x1e8480"]]
    });
    asserter.push_success(&fee_history); // eth_feeHistory

    let hints = SigningHints::default(); // no overrides — resolved from provider

    let payload = client
        .signable_payload(quote, &hints)
        .await
        .expect("signable_payload should succeed");

    let SignablePayload::Fynd(fynd) = payload else {
        panic!("expected Fynd payload");
    };

    let TypedTransaction::Eip1559(tx) = fynd.tx() else {
        panic!("expected EIP-1559 tx");
    };

    assert_eq!(tx.nonce, 99, "nonce should come from mock provider");

    server.verify().await;
}
