//! The record a Fynd pod emits for every answered `POST /v1/quote`: who asked, what they asked,
//! and what Fynd answered, on success and failure alike.
//!
//! The JSON shape is the wire format the collector accepts on `POST /v1/records`
//! (fynd-hosted-service, `crates/collector`); the two field lists must match. Like the
//! [`ReplayRequest`](crate::api::request_capture::ReplayRequest) it embeds, the record is an
//! allowlist by construction: every field is copied by name, so `sender`, `receiver`, signatures,
//! calldata and fee breakdowns cannot leak into it.

use actix_web::http::header::HeaderMap;
use chrono::{DateTime, SecondsFormat, Utc};
use fynd_core::{
    types::BlockInfo, EncodingOptions, ExclusiveAccess, OrderQuote, Quote, QuoteRequest,
    SolveError, Swap, UserTransferType,
};
use serde::{Serialize, Serializer};
use serde_with::{serde_as, DisplayFromStr};
use tycho_simulation::tycho_common::models::{Address, Chain};

use crate::api::{
    error::solve_error_code,
    middleware::ClientLabels,
    request_capture::{failure_reason_slug, quote_status_code, ReplayRequest},
};

/// Version of the record layout. Bump it when a field is added, removed or changes meaning, so a
/// reader can tell records of different layouts apart.
pub const SCHEMA_VERSION: u16 = 1;

/// One answered quote request, as the collector receives it.
#[must_use]
#[serde_as]
#[derive(Debug, Clone, Serialize)]
pub struct QuoteRecord {
    /// The chain this pod serves, by its Tycho name (`ethereum`).
    #[serde_as(as = "DisplayFromStr")]
    chain: Chain,
    /// The pod's clock when the answer was ready, distinct from the collector's `received_at`.
    #[serde(serialize_with = "rfc3339_micros")]
    served_at: DateTime<Utc>,
    schema_version: u16,
    client: ClientLabels,
    request: RequestRecord,
    outcome: OutcomeRecord,
}

impl QuoteRecord {
    /// Builds the record for a finished quote. `served_at` is read from the clock here, so build
    /// the record as soon as the solve returns.
    pub fn build(
        request: RequestRecord,
        result: &Result<Quote, SolveError>,
        chain: Chain,
        headers: &HeaderMap,
    ) -> Self {
        let outcome = OutcomeRecord::from_result(result, request.replay.num_orders());
        Self {
            chain,
            served_at: Utc::now(),
            schema_version: SCHEMA_VERSION,
            client: ClientLabels::from_headers(headers),
            request,
            outcome,
        }
    }

    /// The routing-essential capture the replay log serializes.
    pub(crate) fn replay(&self) -> &ReplayRequest {
        &self.request.replay
    }
}

fn rfc3339_micros<S: Serializer>(time: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&time.to_rfc3339_opts(SecondsFormat::Micros, true))
}

/// The request half of a record: the replay capture plus the non-secret encoding options the
/// capture leaves out because they do not shape the route.
#[must_use]
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecord {
    #[serde(flatten)]
    replay: ReplayRequest,
    encoding: EncodingRecord,
}

impl RequestRecord {
    /// Captures `request` before the solve consumes it. Cheap, like [`ReplayRequest::capture`].
    pub fn capture(request: &QuoteRequest, access: ExclusiveAccess) -> Self {
        Self {
            replay: ReplayRequest::capture(request, access),
            encoding: EncodingRecord::capture(request.options().encoding_options()),
        }
    }
}

/// The encoding options that shape the transaction but not the route. Both absent when the
/// request asked for no encoding. Permits, signatures and client fee params stay out.
#[derive(Debug, Clone, Serialize)]
struct EncodingRecord {
    /// Decimal fraction, for example `0.005`.
    #[serde(skip_serializing_if = "Option::is_none")]
    slippage: Option<String>,
    /// Wire name, for example `transfer_from`.
    #[serde(skip_serializing_if = "Option::is_none")]
    transfer_type: Option<&'static str>,
}

impl EncodingRecord {
    fn capture(options: Option<&EncodingOptions>) -> Self {
        Self {
            slippage: options.map(|options| options.slippage().to_string()),
            transfer_type: options.map(|options| transfer_type_name(options.transfer_type())),
        }
    }
}

/// The wire name `/v1/quote` accepts for a transfer type, which the core enum does not carry.
fn transfer_type_name(transfer_type: &UserTransferType) -> &'static str {
    match transfer_type {
        UserTransferType::TransferFromPermit2 => "transfer_from_permit2",
        UserTransferType::TransferFrom => "transfer_from",
        UserTransferType::UseVaultsFunds => "use_vaults_funds",
    }
}

/// What Fynd answered, at request level. A solve that failed before producing a quote has no
/// time, gas or block; every order then carries the request-level error.
#[derive(Debug, Clone, Serialize)]
struct OutcomeRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    solve_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_gas_estimate: Option<String>,
    /// In wei.
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block: Option<BlockInfo>,
    /// Aligned with the request's orders.
    orders: Vec<OrderOutcome>,
}

impl OutcomeRecord {
    fn from_result(result: &Result<Quote, SolveError>, num_orders: usize) -> Self {
        match result {
            Ok(quote) => Self::solved(quote),
            Err(error) => Self::failed(error, num_orders),
        }
    }

    fn solved(quote: &Quote) -> Self {
        // One solve answers every order at one block and one gas price.
        let orders = quote.orders();
        Self {
            solve_time_ms: Some(quote.solve_time_ms()),
            total_gas_estimate: Some(quote.total_gas_estimate().to_string()),
            gas_price: orders
                .iter()
                .find_map(OrderQuote::gas_price)
                .map(ToString::to_string),
            block: orders
                .first()
                .map(|order| order.block().clone()),
            orders: orders
                .iter()
                .map(OrderOutcome::from_quote)
                .collect(),
        }
    }

    /// The request-level error, repeated for every order: its code as the status, and the same
    /// `http/<code>` reason the replay log emits.
    fn failed(error: &SolveError, num_orders: usize) -> Self {
        let code = solve_error_code(error).to_ascii_lowercase();
        let order = OrderOutcome::without_route(code.clone(), format!("http/{code}"));
        Self {
            solve_time_ms: None,
            total_gas_estimate: None,
            gas_price: None,
            block: None,
            orders: vec![order; num_orders],
        }
    }
}

/// What Fynd answered for one order. Amounts and the route are present whenever a route was
/// found, whatever the status: an order that failed encoding still shows the route it found.
#[derive(Debug, Clone, Serialize)]
struct OrderOutcome {
    /// A [`quote_status_code`], or the lowercased error code of a request-level failure.
    status: String,
    /// A `group/reason` slug, empty on success.
    failure_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount_in: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount_out: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_estimate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount_out_net_gas: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    price_impact_bps: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    algorithm: Option<String>,
    route: Vec<SwapRecord>,
}

impl OrderOutcome {
    fn from_quote(order: &OrderQuote) -> Self {
        let status = quote_status_code(order.status()).to_string();
        let failure_reason =
            failure_reason_slug(order.status(), order.no_route_cause()).to_string();
        let Some(route) = order.route() else {
            return Self::without_route(status, failure_reason);
        };
        Self {
            status,
            failure_reason,
            amount_in: Some(order.amount_in().to_string()),
            amount_out: Some(order.amount_out().to_string()),
            gas_estimate: Some(order.gas_estimate().to_string()),
            amount_out_net_gas: Some(order.amount_out_net_gas().to_string()),
            price_impact_bps: order.price_impact_bps(),
            algorithm: Some(order.algorithm().to_string()),
            route: route
                .swaps()
                .iter()
                .map(SwapRecord::from_swap)
                .collect(),
        }
    }

    fn without_route(status: String, failure_reason: String) -> Self {
        Self {
            status,
            failure_reason,
            amount_in: None,
            amount_out: None,
            gas_estimate: None,
            amount_out_net_gas: None,
            price_impact_bps: None,
            algorithm: None,
            route: Vec::new(),
        }
    }
}

/// One swap of a served route.
#[derive(Debug, Clone, Serialize)]
struct SwapRecord {
    component_id: String,
    protocol: String,
    token_in: Address,
    token_out: Address,
    amount_in: String,
    amount_out: String,
    gas_estimate: String,
    /// Share of the order through this swap, `0.5` meaning half.
    split: String,
}

impl SwapRecord {
    fn from_swap(swap: &Swap) -> Self {
        Self {
            component_id: swap.component_id().to_string(),
            protocol: swap.protocol().to_string(),
            token_in: swap.token_in().clone(),
            token_out: swap.token_out().clone(),
            amount_in: swap.amount_in().to_string(),
            amount_out: swap.amount_out().to_string(),
            gas_estimate: swap.gas_estimate().to_string(),
            split: swap.split().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use actix_web::http::header::{HeaderMap, HeaderName, HeaderValue};
    use fynd_core::{
        algorithm::test_utils::{component, token, MockProtocolSim},
        ExclusiveAccess, Quote, QuoteRequest, SolveError, Swap,
    };
    use num_bigint::BigUint;
    use rstest::rstest;
    use serde_json::{json, Value};
    use tycho_simulation::tycho_common::models::{Address, Chain};

    use super::*;
    use crate::api::request_capture::test_fixtures::{order, request_with_signatures};

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("user-identity"), HeaderValue::from_static("relay"));
        headers
            .insert(HeaderName::from_static("x-user-plan"), HeaderValue::from_static("enterprise"));
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("fynd-client/1.4.2"),
        );
        headers
    }

    fn request_record(request: fynd_rpc_types::QuoteRequest) -> RequestRecord {
        let request: QuoteRequest = request.into();
        RequestRecord::capture(&request, ExclusiveAccess::Granted)
    }

    /// A swap as the solver builds it, serialized so a quote fixture can carry it.
    fn swap_json() -> Value {
        let swap = Swap::new(
            "0xpool".to_string(),
            "uniswap_v2".to_string(),
            Address::from([0xAAu8; 20]),
            Address::from([0xBBu8; 20]),
            BigUint::from(1_000_000_000_000_000_000u64),
            BigUint::from(3_500_000_000u64),
            BigUint::from(180_000u64),
            component("pool", &[token(0xAA, "TIN"), token(0xBB, "TOUT")]),
            // `u128::MAX`, the default liquidity, does not fit a JSON number.
            Box::new(MockProtocolSim::default().with_liquidity(u128::from(u64::MAX))),
        )
        .with_split(0.5);
        serde_json::to_value(&swap).unwrap()
    }

    fn order_quote_json(status: &str, route: Option<Value>) -> Value {
        let mut order = json!({
            "order_id": "order-1",
            "status": status,
            "amount_in": "1000000000000000000",
            "amount_out": "3500000000",
            "gas_estimate": "180000",
            "amount_out_net_gas": "3498000000",
            "price_impact_bps": 12,
            "gas_price": "20000000000",
            "block": { "number": 21_000_000, "hash": "0xabc", "timestamp": 1_730_000_000 },
            "sender": "0xcccccccccccccccccccccccccccccccccccccccc",
            "receiver": "0x7777777777777777777777777777777777777777",
            "solved_against": "21000000",
        });
        if let Some(swap) = route {
            order["route"] = json!({ "swaps": [swap] });
        }
        order
    }

    fn quote(orders: Vec<Value>) -> Quote {
        serde_json::from_value(json!({
            "orders": orders,
            "total_gas_estimate": "180000",
            "solve_time_ms": 42,
        }))
        .unwrap()
    }

    fn record_json(request: RequestRecord, result: &Result<Quote, SolveError>) -> Value {
        serde_json::to_value(QuoteRecord::build(request, result, Chain::Ethereum, &headers()))
            .unwrap()
    }

    fn solved_record() -> Value {
        let result = Ok(quote(vec![order_quote_json("success", Some(swap_json()))]));
        record_json(request_record(request_with_signatures()), &result)
    }

    fn keys(value: &Value) -> BTreeSet<&str> {
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    /// Every level of the record, key by key. A new field shows up here before it reaches the
    /// collector, which drops what it does not know.
    #[test]
    fn test_record_field_allowlist() {
        let value = solved_record();
        let expect = |actual: &Value, expected: &[&str]| {
            assert_eq!(keys(actual), expected.iter().copied().collect(), "in {actual}");
        };
        expect(&value, &["chain", "served_at", "schema_version", "client", "request", "outcome"]);
        expect(&value["client"], &["user_identity", "user_plan", "client_version"]);
        let request = &value["request"];
        expect(
            request,
            &["orders", "options", "exclusive_access", "disable_slippage_taking", "encoding"],
        );
        expect(&request["orders"][0], &["token_in", "token_out", "amount", "side"]);
        expect(&request["options"], &["timeout_ms", "min_responses", "max_gas"]);
        expect(&request["encoding"], &["slippage", "transfer_type"]);
        let outcome = &value["outcome"];
        expect(outcome, &["solve_time_ms", "total_gas_estimate", "gas_price", "block", "orders"]);
        expect(&outcome["block"], &["number", "hash", "timestamp"]);
        let order = &outcome["orders"][0];
        expect(
            order,
            &[
                "status",
                "failure_reason",
                "amount_in",
                "amount_out",
                "gas_estimate",
                "amount_out_net_gas",
                "price_impact_bps",
                "algorithm",
                "route",
            ],
        );
        expect(
            &order["route"][0],
            &[
                "component_id",
                "protocol",
                "token_in",
                "token_out",
                "amount_in",
                "amount_out",
                "gas_estimate",
                "split",
            ],
        );
    }

    /// Nothing that names the caller or signs for them leaves the pod.
    #[test]
    fn test_record_omits_identifying_and_signed_fields() {
        let json = solved_record().to_string();
        for needle in [
            "sender",
            "receiver",
            "signature",
            "permit",
            "calldata",
            "transaction",
            "fee",
            "\"id\"",
            "cccccccc",
            "77777777",
        ] {
            assert!(!json.contains(needle), "{needle} leaked; json was: {json}");
        }
    }

    #[test]
    fn test_record_solved_quote() {
        let value = solved_record();
        assert_eq!(value["chain"], "ethereum");
        assert_eq!(value["schema_version"], 1);
        DateTime::parse_from_rfc3339(value["served_at"].as_str().unwrap()).unwrap();
        assert_eq!(
            value["client"],
            json!({
                "user_identity": "relay",
                "user_plan": "enterprise",
                "client_version": "fynd-client/1.4.2"
            })
        );
        assert_eq!(value["request"]["exclusive_access"], true);
        assert_eq!(
            value["request"]["encoding"],
            json!({ "slippage": "0.005", "transfer_type": "transfer_from" })
        );

        let outcome = &value["outcome"];
        assert_eq!(outcome["solve_time_ms"], 42);
        assert_eq!(outcome["total_gas_estimate"], "180000");
        assert_eq!(outcome["gas_price"], "20000000000");
        assert_eq!(outcome["block"]["number"], 21_000_000);
        assert_eq!(outcome["block"]["hash"], "0xabc");

        let order = &outcome["orders"][0];
        assert_eq!(order["status"], "success");
        assert_eq!(order["failure_reason"], "");
        assert_eq!(order["amount_in"], "1000000000000000000");
        assert_eq!(order["amount_out"], "3500000000");
        assert_eq!(order["amount_out_net_gas"], "3498000000");
        assert_eq!(order["price_impact_bps"], 12);

        let swap = &order["route"][0];
        assert_eq!(swap["component_id"], "0xpool");
        assert_eq!(swap["protocol"], "uniswap_v2");
        assert_eq!(swap["token_in"], "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(swap["token_out"], "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(swap["amount_out"], "3500000000");
        assert_eq!(swap["gas_estimate"], "180000");
        assert_eq!(swap["split"], "0.5");
    }

    /// An order the solver could not route carries its status and reason and nothing else.
    #[test]
    fn test_record_order_without_route() {
        let result = Ok(quote(vec![order_quote_json("no_route_found", None)]));
        let value = record_json(request_record(request_with_signatures()), &result);

        let order = &value["outcome"]["orders"][0];
        assert_eq!(
            keys(order),
            ["status", "failure_reason", "route"]
                .into_iter()
                .collect()
        );
        assert_eq!(order["status"], "no_route_found");
        assert_eq!(order["failure_reason"], "graph/other");
        assert_eq!(order["route"], json!([]));
        assert_eq!(value["outcome"]["solve_time_ms"], 42, "the solve itself still finished");
    }

    /// A solve that failed as a whole has no quote to read: every order carries the error.
    #[test]
    fn test_record_request_failure() {
        let request = request_record(fynd_rpc_types::QuoteRequest::new(vec![order(), order()]));
        let value = record_json(request, &Err(SolveError::QueueFull));

        let outcome = &value["outcome"];
        assert_eq!(keys(outcome), ["orders"].into_iter().collect());
        let orders = outcome["orders"].as_array().unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(
            orders[1],
            json!({ "status": "queue_full", "failure_reason": "http/queue_full", "route": [] })
        );
    }

    #[test]
    fn test_record_without_encoding_options() {
        let request = request_record(fynd_rpc_types::QuoteRequest::new(vec![order()]));
        let result = Ok(quote(vec![order_quote_json("success", None)]));
        let value = record_json(request, &result);

        assert_eq!(value["request"]["encoding"], json!({}));
        assert_eq!(value["request"]["disable_slippage_taking"], false);
    }

    /// The replay log reads its request from the record, so the two must agree.
    #[test]
    fn test_record_request_matches_replay_capture() {
        let request: QuoteRequest = request_with_signatures().into();
        let record = RequestRecord::capture(&request, ExclusiveAccess::Denied);

        let replay: Value = serde_json::from_str(&record.replay.to_json()).unwrap();
        let mut request_json = serde_json::to_value(&record).unwrap();
        request_json
            .as_object_mut()
            .unwrap()
            .remove("encoding");

        assert_eq!(request_json, replay);
    }

    #[rstest]
    #[case(UserTransferType::TransferFrom, "transfer_from")]
    #[case(UserTransferType::TransferFromPermit2, "transfer_from_permit2")]
    #[case(UserTransferType::UseVaultsFunds, "use_vaults_funds")]
    fn test_transfer_type_name(#[case] transfer_type: UserTransferType, #[case] expected: &str) {
        assert_eq!(transfer_type_name(&transfer_type), expected);
    }
}
