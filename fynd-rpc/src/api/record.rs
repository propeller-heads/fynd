//! The record a Fynd pod emits for every answered `POST /v1/quote`: who asked, what they asked,
//! and what Fynd answered, on success and failure alike.
//!
//! The JSON shape is the wire format the collector accepts on `POST /v1/records`
//! (fynd-hosted-service, `crates/collector`); the two field lists must match. Like the
//! [`ReplayRequest`](crate::api::request_capture::ReplayRequest) it embeds, the record is an
//! allowlist by construction: every field is copied by name, so `sender`, `receiver`, signatures,
//! calldata and fee breakdowns cannot leak into it.
//!
//! Only requests that reached the solver produce a record. A request rejected by
//! `validate_quote_request` returns 400 without one, so the collector sees solver outcomes and
//! not client-side rejections.

use chrono::{DateTime, SecondsFormat, Utc};
use fynd_core::{
    types::BlockInfo, ClientFeeParams, EncodingOptions, ExclusiveAccess, OrderQuote, Quote,
    QuoteRequest, SolveError, Swap, UserTransferType,
};
use serde::{Serialize, Serializer};
use serde_with::{serde_as, DisplayFromStr};
use tycho_simulation::tycho_common::models::{Address, Chain};

pub use crate::api::middleware::ClientInfo;
use crate::api::{
    error::solve_error_code,
    request_capture::{
        failure_reason_slug, quote_status_code, request_failure_slug, ReplayRequest,
        RequestOutcome, SLOW_SOLVE_THRESHOLD_MS,
    },
};

/// Version of the record layout. Bump it when a field is added, removed or changes meaning, so a
/// reader can tell records of different layouts apart.
pub const SCHEMA_VERSION: u16 = 1;

/// One answered quote request, as the collector receives it.
#[must_use]
#[serde_as]
#[derive(Debug, Serialize)]
pub struct QuoteRecord {
    /// The chain this pod serves, by its Tycho name (`ethereum`).
    #[serde_as(as = "DisplayFromStr")]
    chain: Chain,
    /// The pod's clock when the answer was ready, distinct from the collector's `received_at`.
    #[serde(serialize_with = "rfc3339_micros")]
    served_at: DateTime<Utc>,
    schema_version: u16,
    client: ClientInfo,
    request: RequestRecord,
    outcome: OutcomeRecord,
}

impl QuoteRecord {
    /// Builds the record for a finished quote. `served_at` is read from the clock here, so build
    /// the record as soon as the solve returns.
    ///
    /// `head` is the pod's latest known block, used only when no order priced against one — see
    /// [`priced_block`]. Pass `None` to leave such a record without a block.
    pub fn build(
        request: RequestRecord,
        result: &Result<Quote, SolveError>,
        chain: Chain,
        client: ClientInfo,
        head: Option<BlockInfo>,
    ) -> Self {
        let outcome = match result {
            Ok(quote) => OutcomeRecord::solved(quote, head),
            Err(error) => OutcomeRecord::failed(error, request.replay.num_orders(), head),
        };
        Self {
            chain,
            served_at: Utc::now(),
            schema_version: SCHEMA_VERSION,
            client,
            request,
            outcome,
        }
    }

    /// The routing-essential capture the replay log serializes.
    pub(crate) fn replay(&self) -> &ReplayRequest {
        &self.request.replay
    }

    /// Whether the replay log records this quote: the solve failed, or at least one order did.
    pub(crate) fn is_failure(&self) -> bool {
        self.outcome.request_error.is_some() ||
            self.outcome
                .orders
                .iter()
                .any(|order| order.status != "success")
    }

    /// The solve time when it crossed the slow-solve threshold, which is what gets logged.
    pub(crate) fn slow_solve_time_ms(&self) -> Option<u64> {
        self.outcome
            .solve_time_ms
            .filter(|solve_time_ms| *solve_time_ms > SLOW_SOLVE_THRESHOLD_MS)
    }

    /// The log line's view of the outcome, built from the record so the two cannot disagree.
    ///
    /// Allocates the per-order status and reason strings, so call it only once the guard above
    /// has decided the line will be emitted.
    pub(crate) fn log_outcome(&self) -> RequestOutcome {
        match self.outcome.request_error {
            Some(code) => RequestOutcome::Failed { code },
            None => RequestOutcome::Solved {
                solve_time_ms: self
                    .outcome
                    .solve_time_ms
                    .unwrap_or_default(),
                order_statuses: self
                    .outcome
                    .orders
                    .iter()
                    .map(|order| order.status.clone())
                    .collect(),
                failure_reasons: self
                    .outcome
                    .orders
                    .iter()
                    .map(|order| order.failure_reason.clone())
                    .collect(),
            },
        }
    }
}

/// The block a route was priced against, if any order got one.
///
/// An order no worker answered carries a zero placeholder, so a zero block number means the
/// order went unpriced rather than that the chain is at block zero.
#[must_use]
pub fn priced_block(quote: &Quote) -> Option<&BlockInfo> {
    quote
        .orders()
        .iter()
        .map(OrderQuote::block)
        .find(|block| block.number() != 0)
}

fn rfc3339_micros<S: Serializer>(time: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&time.to_rfc3339_opts(SecondsFormat::Micros, true))
}

/// The request half of a record: the replay capture plus the non-secret encoding options the
/// capture leaves out because they do not shape the route.
#[must_use]
#[derive(Debug, Serialize)]
pub struct RequestRecord {
    #[serde(flatten)]
    replay: ReplayRequest,
    /// Absent when the request asked for no encoding.
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<EncodingRecord>,
}

impl RequestRecord {
    /// Captures `request` before the solve consumes it. Cheap, like [`ReplayRequest::capture`].
    pub fn capture(request: &QuoteRequest, access: ExclusiveAccess) -> Self {
        Self {
            replay: ReplayRequest::capture(request, access),
            encoding: request
                .options()
                .encoding_options()
                .map(EncodingRecord::capture),
        }
    }
}

/// The encoding options that shape the transaction but not the route. Permits, signatures and
/// the fee receiver stay out.
#[derive(Debug, Serialize)]
struct EncodingRecord {
    /// Decimal fraction, for example `0.005`.
    slippage: String,
    /// Wire name, for example `transfer_from`.
    transfer_type: &'static str,
    /// The fee the client asked the router to take from the output, in basis points. A fee the
    /// solve did not account for shows up as a route that reverts, so a reader needs the number.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_fee_bps: Option<u16>,
}

impl EncodingRecord {
    fn capture(options: &EncodingOptions) -> Self {
        Self {
            slippage: options.slippage().to_string(),
            transfer_type: transfer_type_name(options.transfer_type()),
            client_fee_bps: options
                .client_fee_params()
                .map(ClientFeeParams::bps),
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

/// Where the record's `block` came from.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum BlockSource {
    /// A routed order priced against it.
    Order,
    /// The pod's latest known block. Nothing priced against it, so it dates the answer rather
    /// than the routing.
    Head,
}

/// What Fynd answered, at request level. A solve that failed before producing a quote has no
/// time or gas; every order then carries the request-level error.
#[derive(Debug, Serialize)]
struct OutcomeRecord {
    /// The code of a failure that hit the whole request, `None` when the solve returned a
    /// quote. Not serialized: the collector reads the same code as every order's `status`.
    #[serde(skip)]
    request_error: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    solve_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_gas_estimate: Option<String>,
    /// In wei.
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block: Option<BlockInfo>,
    /// Always present alongside `block`, so a reader knows whether anything priced against it.
    #[serde(skip_serializing_if = "Option::is_none")]
    block_source: Option<BlockSource>,
    /// Aligned with the request's orders.
    orders: Vec<OrderRecord>,
}

impl OutcomeRecord {
    fn solved(quote: &Quote, head: Option<BlockInfo>) -> Self {
        // One solve answers every order at one block and one gas price.
        let orders = quote.orders();
        let (block, block_source) = match priced_block(quote) {
            Some(block) => (Some(block.clone()), Some(BlockSource::Order)),
            None => dated_by_head(head),
        };
        Self {
            request_error: None,
            solve_time_ms: Some(quote.solve_time_ms()),
            total_gas_estimate: Some(quote.total_gas_estimate().to_string()),
            gas_price: orders
                .iter()
                .find_map(OrderQuote::gas_price)
                .map(ToString::to_string),
            block,
            block_source,
            orders: orders
                .iter()
                .map(OrderRecord::from_quote)
                .collect(),
        }
    }

    /// The request-level error, repeated for every order: its code as the status, and the same
    /// `http/<code>` reason the replay log emits.
    fn failed(error: &SolveError, num_orders: usize, head: Option<BlockInfo>) -> Self {
        let code = solve_error_code(error);
        let order =
            OrderRecord::without_route(code.to_ascii_lowercase(), request_failure_slug(code));
        let (block, block_source) = dated_by_head(head);
        Self {
            request_error: Some(code),
            solve_time_ms: None,
            total_gas_estimate: None,
            gas_price: None,
            block,
            block_source,
            orders: vec![order; num_orders],
        }
    }
}

/// Pairs the pod's head with the tag that says nothing priced against it.
fn dated_by_head(head: Option<BlockInfo>) -> (Option<BlockInfo>, Option<BlockSource>) {
    match head {
        Some(block) => (Some(block), Some(BlockSource::Head)),
        None => (None, None),
    }
}

/// What Fynd answered for one order. Amounts and the route are present whenever a route was
/// found, whatever the status: an order that failed encoding still shows the route it found.
#[derive(Debug, Clone, Serialize)]
struct OrderRecord {
    /// A [`quote_status_code`], or the lowercased error code of a request-level failure.
    status: String,
    /// A [`failure_reason_slug`] (`graph/no_liquidity`, `infra/timeout`, …), or
    /// [`request_failure_slug`] for a request-level failure. Empty on success.
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

impl OrderRecord {
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
        finalize_quote,
        types::test_utils::OrderQuoteBuilder,
        ExclusiveAccess, Quote, QuoteRequest, QuoteStatus, Route, SolveError, Swap,
    };
    use num_bigint::BigUint;
    use rstest::rstest;
    use serde_json::{json, Value};
    use tycho_simulation::tycho_common::models::{Address, Chain};

    use super::*;
    use crate::api::request_capture::test_utils::{order, request_with_signatures};

    fn client() -> ClientInfo {
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("user-identity"), HeaderValue::from_static("relay"));
        headers
            .insert(HeaderName::from_static("x-user-plan"), HeaderValue::from_static("enterprise"));
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("fynd-client/1.4.2"),
        );
        ClientInfo::from_headers(&headers)
    }

    fn request_record(request: fynd_rpc_types::QuoteRequest) -> RequestRecord {
        let request: QuoteRequest = request.into();
        RequestRecord::capture(&request, ExclusiveAccess::Granted)
    }

    fn route() -> Route {
        let swap = Swap::new(
            "0xpool".to_string(),
            "uniswap_v2".to_string(),
            Address::from([0xAAu8; 20]),
            Address::from([0xBBu8; 20]),
            BigUint::from(1_000_000_000_000_000_000u64),
            BigUint::from(3_500_000_000u64),
            BigUint::from(180_000u64),
            component("pool", &[token(0xAA, "TIN"), token(0xBB, "TOUT")]),
            Box::new(MockProtocolSim::default()),
        )
        .with_split(0.5);
        Route::new(vec![swap], Vec::new()).unwrap()
    }

    /// An order as the router returns it: priced, routed, and solved by a named algorithm.
    fn routed_order(status: QuoteStatus) -> OrderQuoteBuilder {
        OrderQuoteBuilder::new("order-1", status, "greedy")
            .amounts([1_000_000_000_000_000_000, 3_500_000_000, 180_000, 3_498_000_000])
            .block(21_000_000, "0xabc", 1_730_000_000)
            .gas_price(20_000_000_000)
            .price_impact_bps(12)
    }

    fn record_json(
        request: RequestRecord,
        result: &Result<Quote, SolveError>,
        head: Option<BlockInfo>,
    ) -> Value {
        serde_json::to_value(QuoteRecord::build(request, result, Chain::Ethereum, client(), head))
            .unwrap()
    }

    fn solved_json(orders: Vec<fynd_core::OrderQuote>, head: Option<BlockInfo>) -> Value {
        let quote = finalize_quote(orders, 42);
        record_json(request_record(request_with_signatures()), &Ok(quote), head)
    }

    fn solved_record() -> Value {
        solved_json(
            vec![routed_order(QuoteStatus::Success)
                .route(route())
                .build()],
            None,
        )
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
        expect(&request["options"], &["timeout_ms", "min_responses", "max_gas", "route_filter"]);
        expect(&request["options"]["route_filter"], &["exclude_pools"]);
        expect(&request["encoding"], &["slippage", "transfer_type", "client_fee_bps"]);
        let outcome = &value["outcome"];
        expect(
            outcome,
            &[
                "solve_time_ms",
                "total_gas_estimate",
                "gas_price",
                "block",
                "block_source",
                "orders",
            ],
        );
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
            "client_fee_params",
            "fee_breakdown",
            "\"id\"",
            "cccccccc",
            "77777777",
            "eeeeeeee",
        ] {
            assert!(!json.contains(needle), "{needle} leaked; json was: {json}");
        }
    }

    #[test]
    fn test_record_solved_quote() {
        let value = solved_record();
        assert_eq!(value["chain"], "ethereum");
        assert_eq!(value["schema_version"], 1);
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
            json!({ "slippage": "0.005", "transfer_type": "transfer_from", "client_fee_bps": 100 })
        );

        let outcome = &value["outcome"];
        assert_eq!(outcome["solve_time_ms"], 42);
        assert_eq!(outcome["total_gas_estimate"], "180000");
        assert_eq!(outcome["gas_price"], "20000000000");
        assert_eq!(outcome["block"]["number"], 21_000_000);
        assert_eq!(outcome["block"]["hash"], "0xabc");
        assert_eq!(outcome["block_source"], "order");

        let order = &outcome["orders"][0];
        assert_eq!(order["status"], "success");
        assert_eq!(order["failure_reason"], "");
        assert_eq!(order["amount_in"], "1000000000000000000");
        assert_eq!(order["amount_out"], "3500000000");
        assert_eq!(order["amount_out_net_gas"], "3498000000");
        assert_eq!(order["price_impact_bps"], 12);
        assert_eq!(order["algorithm"], "greedy", "the solver that won the order");

        let swap = &order["route"][0];
        assert_eq!(swap["component_id"], "0xpool");
        assert_eq!(swap["protocol"], "uniswap_v2");
        assert_eq!(swap["token_in"], "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(swap["token_out"], "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(swap["amount_out"], "3500000000");
        assert_eq!(swap["gas_estimate"], "180000");
        assert_eq!(swap["split"], "0.5");
    }

    /// The collector reads `served_at` as a fixed-width UTC timestamp, so the precision is part
    /// of the contract, not an accident of chrono's default.
    #[test]
    fn test_record_served_at_is_utc_microseconds() {
        let value = solved_record();
        let served_at = value["served_at"].as_str().unwrap();
        DateTime::parse_from_rfc3339(served_at).unwrap();
        assert!(served_at.ends_with('Z'), "{served_at}");
        assert_eq!(served_at.len(), 27, "microseconds expected: {served_at}");
    }

    /// One solve answers every order, so the outcomes line up with the request's orders.
    #[test]
    fn test_record_multi_order_quote() {
        let unpriced = OrderQuoteBuilder::new("order-1", QuoteStatus::Timeout, "")
            .block(0, "", 0)
            .build();
        let priced = routed_order(QuoteStatus::Success)
            .route(route())
            .build();
        let value = solved_json(vec![unpriced, priced], None);

        let outcome = &value["outcome"];
        let orders = outcome["orders"].as_array().unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0]["status"], "timeout");
        assert_eq!(orders[1]["status"], "success", "outcomes stay in request order");
        // Both are request-level values, and the first order carries neither.
        assert_eq!(outcome["gas_price"], "20000000000");
        assert_eq!(outcome["block"]["number"], 21_000_000, "the unpriced order is skipped");
        assert_eq!(outcome["block_source"], "order");
    }

    /// An order that routed and then failed keeps its route: that is what explains the failure.
    #[test]
    fn test_record_order_with_route_and_failure() {
        let failed = routed_order(QuoteStatus::EncodingFailed)
            .route(route())
            .build();
        let value = solved_json(vec![failed], None);

        let order = &value["outcome"]["orders"][0];
        assert_eq!(order["status"], "encoding_failed");
        assert_eq!(order["failure_reason"], "encoding/encoding_failed");
        assert_eq!(order["amount_out"], "3500000000");
        assert_eq!(order["route"].as_array().unwrap().len(), 1, "the route it found is kept");
    }

    /// The reason comes from the recorded cause, not the status, when the router set one.
    #[test]
    fn test_record_failure_reason_from_cause() {
        let order = OrderQuoteBuilder::new("order-1", QuoteStatus::NoRouteFound, "")
            .no_route_cause(SolveError::MaxGasExceeded)
            .build();
        let value = solved_json(vec![order], None);

        // The status alone would read `graph/other`; the cause is what names the real reason.
        assert_eq!(value["outcome"]["orders"][0]["failure_reason"], "request/max_gas_exceeded");
    }

    /// An order the solver could not route carries its status and reason and nothing else.
    #[test]
    fn test_record_order_without_route() {
        let order = routed_order(QuoteStatus::NoRouteFound).build();
        let value = solved_json(vec![order], None);

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

    /// Nothing priced, so the pod's head dates the answer and says so.
    #[test]
    fn test_record_block_falls_back_to_head() {
        let unpriced = OrderQuoteBuilder::new("order-1", QuoteStatus::Timeout, "")
            .block(0, "", 0)
            .build();
        let head = BlockInfo::new(21_000_042, "0xhead".to_string(), 1_730_000_500);
        let value = solved_json(vec![unpriced], Some(head));

        let outcome = &value["outcome"];
        assert_eq!(outcome["block"]["number"], 21_000_042);
        assert_eq!(outcome["block_source"], "head", "nothing priced against it");
    }

    /// No priced block and no head: better no block than a zero one.
    #[test]
    fn test_record_without_any_block() {
        let unpriced = OrderQuoteBuilder::new("order-1", QuoteStatus::Timeout, "")
            .block(0, "", 0)
            .build();
        let value = solved_json(vec![unpriced], None);

        let outcome = &value["outcome"];
        assert!(outcome.get("block").is_none(), "in {outcome}");
        assert!(outcome.get("block_source").is_none(), "in {outcome}");
    }

    /// A solve that failed as a whole has no quote to read: every order carries the error.
    #[test]
    fn test_record_request_failure() {
        let request = request_record(fynd_rpc_types::QuoteRequest::new(vec![order(), order()]));
        let head = BlockInfo::new(21_000_042, "0xhead".to_string(), 1_730_000_500);
        let value = record_json(request, &Err(SolveError::QueueFull), Some(head));

        let outcome = &value["outcome"];
        assert_eq!(
            keys(outcome),
            ["block", "block_source", "orders"]
                .into_iter()
                .collect()
        );
        assert_eq!(outcome["block_source"], "head");
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
        let quote = finalize_quote(vec![routed_order(QuoteStatus::Success).build()], 42);
        let value = record_json(request, &Ok(quote), None);

        assert!(
            value["request"]
                .get("encoding")
                .is_none(),
            "in {}",
            value["request"]
        );
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

    /// The log line and the record are two views of one solve, so they agree by construction.
    #[rstest]
    #[case::success(QuoteStatus::Success, false)]
    #[case::one_order_failed(QuoteStatus::NoRouteFound, true)]
    fn test_record_is_failure(#[case] status: QuoteStatus, #[case] expected: bool) {
        let quote = finalize_quote(
            vec![routed_order(status)
                .route(route())
                .build()],
            42,
        );
        let record = QuoteRecord::build(
            request_record(request_with_signatures()),
            &Ok(quote),
            Chain::Ethereum,
            client(),
            None,
        );
        assert_eq!(record.is_failure(), expected);
    }

    #[test]
    fn test_record_is_failure_for_request_error() {
        let record = QuoteRecord::build(
            request_record(request_with_signatures()),
            &Err(SolveError::QueueFull),
            Chain::Ethereum,
            client(),
            None,
        );
        assert!(record.is_failure());
        assert!(record.slow_solve_time_ms().is_none(), "a failed solve has no solve time");
    }

    #[rstest]
    #[case::under(SLOW_SOLVE_THRESHOLD_MS, None)]
    #[case::over(SLOW_SOLVE_THRESHOLD_MS + 1, Some(SLOW_SOLVE_THRESHOLD_MS + 1))]
    fn test_record_slow_solve_time_ms(#[case] solve_time_ms: u64, #[case] expected: Option<u64>) {
        let quote = finalize_quote(
            vec![routed_order(QuoteStatus::Success)
                .route(route())
                .build()],
            solve_time_ms,
        );
        let record = QuoteRecord::build(
            request_record(request_with_signatures()),
            &Ok(quote),
            Chain::Ethereum,
            client(),
            None,
        );
        assert_eq!(record.slow_solve_time_ms(), expected);
    }

    #[rstest]
    #[case(UserTransferType::TransferFrom, "transfer_from")]
    #[case(UserTransferType::TransferFromPermit2, "transfer_from_permit2")]
    #[case(UserTransferType::UseVaultsFunds, "use_vaults_funds")]
    fn test_transfer_type_name(#[case] transfer_type: UserTransferType, #[case] expected: &str) {
        assert_eq!(transfer_type_name(&transfer_type), expected);
    }
}
