//! Builds the routing-essential representation of a quote request for the
//! replay-capture log emitted by the `/v1/quote` handler.

use fynd_core::{NoPathReason, QuoteStatus};
use fynd_rpc_types::{Address, OrderSide, QuoteRequest};
use serde::Serialize;
use tracing::info;

/// Routing-essential view of a single order, serialized into the replay log.
///
/// An allowlist by construction: only fields that affect route-finding are
/// present. The server-generated `id`, the routing-irrelevant `sender` /
/// `receiver` (also PII we keep out of logs), and all encoding data are
/// omitted — nothing is copied implicitly, so no DTO field can leak.
#[derive(Serialize)]
struct ReplayOrder {
    token_in: Address,
    token_out: Address,
    amount: String,
    side: OrderSide,
}

/// Routing-essential view of the request-level solve options.
#[derive(Serialize)]
struct ReplayOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_responses: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_gas: Option<String>,
}

/// Owned, routing-essential view of a whole quote request.
///
/// Captured (the fields that determine the route): per order `token_in`,
/// `token_out`, `amount`, `side`; plus the solve options `timeout_ms`,
/// `min_responses`, `max_gas`. Everything else is dropped: `encoding_options`
/// (slippage / transfer type / price guard, and every Permit2 / client-fee
/// **signature**), the server-generated order `id`, and `sender` / `receiver`
/// (routing-irrelevant and PII). Built from an explicit allowlist, so no
/// request field can leak into the logs.
///
/// This is NOT a full [`QuoteRequest`] — it omits the required `sender`, so a
/// replay harness supplies a placeholder sender before re-issuing. An outcome
/// that depended on `price_guard` may not reproduce on replay.
#[derive(Serialize)]
pub(crate) struct ReplayRequest {
    orders: Vec<ReplayOrder>,
    options: ReplayOptions,
}

impl ReplayRequest {
    /// Extracts the owned routing-essential capture from `request`. Cheap — a
    /// few address clones and no JSON — so it is safe to run on the quote hot
    /// path; the serialization cost is deferred to [`ReplayRequest::to_json`],
    /// which the handler only calls off the response path.
    pub(crate) fn capture(request: &QuoteRequest) -> Self {
        let orders = request
            .orders()
            .iter()
            .map(|order| ReplayOrder {
                token_in: order.token_in().clone(),
                token_out: order.token_out().clone(),
                amount: order.amount().to_string(),
                side: order.side(),
            })
            .collect();
        let options = request.options();
        Self {
            orders,
            options: ReplayOptions {
                timeout_ms: options.timeout_ms(),
                min_responses: options.min_responses(),
                max_gas: options
                    .max_gas()
                    .map(ToString::to_string),
            },
        }
    }

    /// Serializes the capture to the replay-log JSON string.
    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "<unserializable>".to_string())
    }
}

/// Serializes `request` down to the routing-essential fields, as a JSON string.
/// Convenience wrapper over [`ReplayRequest::capture`] + [`ReplayRequest::to_json`].
pub(crate) fn replay_json(request: &QuoteRequest) -> String {
    ReplayRequest::capture(request).to_json()
}

/// Stable snake_case code for a per-order [`QuoteStatus`], matching the wire
/// serialization. The wildcard guards the `#[non_exhaustive]` enum against new
/// variants added upstream.
pub(crate) fn quote_status_code(status: QuoteStatus) -> &'static str {
    match status {
        QuoteStatus::Success => "success",
        QuoteStatus::NoRouteFound => "no_route_found",
        QuoteStatus::InsufficientLiquidity => "insufficient_liquidity",
        QuoteStatus::Timeout => "timeout",
        QuoteStatus::NotReady => "not_ready",
        QuoteStatus::PriceCheckFailed => "price_check_failed",
        _ => "unknown",
    }
}

/// Stable snake_case code for a no-route [`NoPathReason`]. `""` when absent;
/// wildcard guards the `#[non_exhaustive]` enum.
pub(crate) fn no_route_reason_code(reason: Option<NoPathReason>) -> &'static str {
    match reason {
        None => "",
        Some(NoPathReason::SourceTokenNotInGraph) => "source_token_not_in_graph",
        Some(NoPathReason::DestinationTokenNotInGraph) => "destination_token_not_in_graph",
        Some(NoPathReason::NoGraphPath) => "no_graph_path",
        Some(NoPathReason::NoScorablePaths) => "no_scorable_paths",
        Some(_) => "unknown",
    }
}

/// Recorded outcome of a solved request, for the replay-capture log.
pub(crate) enum RequestOutcome {
    /// Solve returned a quote; per-order status codes in request order.
    Solved {
        /// Total orchestration time reported by the router.
        solve_time_ms: u64,
        /// One [`quote_status_code`] per order, in request order.
        order_statuses: Vec<&'static str>,
        /// One [`no_route_reason_code`] per order, aligned with `order_statuses`
        /// (`""` for non-`no_route_found` orders).
        no_route_reasons: Vec<&'static str>,
    },
    /// Solve failed; carries the [`crate::api::error::solve_error_code`].
    Failed {
        /// Stable error code (e.g. `TIMEOUT`, `QUEUE_FULL`).
        code: &'static str,
    },
}

impl RequestOutcome {
    /// Whether this outcome represents a failed quote: a solver error, or a
    /// solve in which at least one order did not succeed. Successful quotes
    /// (every order `success`) are not logged.
    pub(crate) fn is_failure(&self) -> bool {
        match self {
            RequestOutcome::Failed { .. } => true,
            RequestOutcome::Solved { order_statuses, .. } => order_statuses
                .iter()
                .any(|status| *status != "success"),
        }
    }
}

/// Emits the single `quote_failure` capture log line.
///
/// `request_json` is the sanitized, re-issuable request from [`replay_json`].
/// Only failed quotes are logged; filter these in Loki with
/// `event="quote_failure"`.
pub(crate) fn log_request_capture(num_orders: usize, request_json: &str, outcome: &RequestOutcome) {
    match outcome {
        RequestOutcome::Solved { solve_time_ms, order_statuses, no_route_reasons } => info!(
            event = "quote_failure",
            num_orders,
            solve_time_ms = *solve_time_ms,
            outcome = "ok",
            order_statuses = ?order_statuses,
            no_route_reasons = ?no_route_reasons,
            request = %request_json,
            "quote failure captured"
        ),
        RequestOutcome::Failed { code } => info!(
            event = "quote_failure",
            num_orders,
            outcome = *code,
            request = %request_json,
            "quote failure captured"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use fynd_rpc_types::{
        Bytes, ClientFeeParams, EncodingOptions, Order, OrderSide, PermitDetails, PermitSingle,
        QuoteOptions, QuoteRequest,
    };
    use num_bigint::BigUint;
    use serde_json::Value;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    fn order() -> Order {
        Order::new(
            Bytes::from([0xAAu8; 20]),
            Bytes::from([0xBBu8; 20]),
            BigUint::from(1_000_000_000_000_000_000u64),
            OrderSide::Sell,
            Bytes::from([0xCCu8; 20]),
        )
        .with_receiver(Bytes::from([0x77u8; 20]))
    }

    fn request_with_signatures() -> QuoteRequest {
        let permit = PermitSingle::new(
            PermitDetails::new(
                Bytes::from([0xAAu8; 20]),
                BigUint::from(1u64),
                BigUint::from(2u64),
                BigUint::from(3u64),
            ),
            Bytes::from([0xDDu8; 20]),
            BigUint::from(9u64),
        );
        let fee = ClientFeeParams::new(
            100,
            Bytes::from([0xEEu8; 20]),
            BigUint::from(0u64),
            1_893_456_000,
            Bytes::from([0x11u8; 65]),
        );
        let encoding = EncodingOptions::new(0.005)
            .with_permit2(permit, Bytes::from([0x22u8; 65]))
            .with_client_fee_params(fee);
        let options = QuoteOptions::default()
            .with_timeout_ms(2000)
            .with_min_responses(1)
            .with_max_gas(BigUint::from(500_000u64))
            .with_encoding_options(encoding);
        QuoteRequest::new(vec![order()]).with_options(options)
    }

    #[test]
    fn replay_json_captures_only_routing_fields() {
        let json = replay_json(&request_with_signatures());
        // No encoding data or signatures.
        assert!(!json.contains("encoding_options"), "json was: {json}");
        assert!(!json.contains("signature"), "json was: {json}");
        assert!(!json.contains("permit"), "json was: {json}");
        assert!(!json.contains("client_fee"), "json was: {json}");
        // No id / sender / receiver (routing-irrelevant, PII).
        assert!(!json.contains("\"id\""), "json was: {json}");
        assert!(!json.contains("sender"), "json was: {json}");
        assert!(!json.contains("receiver"), "json was: {json}");
        assert!(!json.contains("cccccccc"), "sender address leaked; json was: {json}");
        assert!(!json.contains("77777777"), "receiver address leaked; json was: {json}");
        // Routing inputs preserved.
        assert!(json.contains("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "json was: {json}");
        assert!(json.contains("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), "json was: {json}");
    }

    #[test]
    fn replay_json_keeps_routing_essentials_and_options() {
        let json = replay_json(&request_with_signatures());
        let value: Value = serde_json::from_str(&json).unwrap();
        let order = &value["orders"][0];
        assert_eq!(order["token_in"], "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(order["token_out"], "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(order["amount"], "1000000000000000000");
        assert_eq!(order["side"], "sell");
        let options = &value["options"];
        assert_eq!(options["timeout_ms"], 2000);
        assert_eq!(options["min_responses"], 1);
        assert_eq!(options["max_gas"], "500000");
    }

    #[test]
    fn replay_json_output_keys_are_allowlisted() {
        let req = request_with_signatures();
        let json = replay_json(&req);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        let top_level: std::collections::BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top_level,
            ["orders", "options"].into_iter().collect(),
            "unexpected top-level keys {top_level:?} — a new field may leak into replay logs; json: {json}"
        );

        let options = value
            .get("options")
            .and_then(Value::as_object)
            .unwrap();
        assert!(
            !options.contains_key("encoding_options"),
            "encoding_options leaked into replay log; json: {json}"
        );
        let options_allowlist = ["timeout_ms", "min_responses", "max_gas"];
        for key in options.keys() {
            assert!(
                options_allowlist.contains(&key.as_str()),
                "unexpected key {key} — a new option field may leak into replay logs; json: {json}"
            );
        }

        // Only routing-essential order fields — id / sender / receiver must NOT appear.
        let orders_allowlist = ["token_in", "token_out", "amount", "side"];
        for order in value
            .get("orders")
            .and_then(Value::as_array)
            .unwrap()
        {
            for key in order.as_object().unwrap().keys() {
                assert!(
                    orders_allowlist.contains(&key.as_str()),
                    "unexpected key {key} — a new order field may leak into replay logs; json: {json}"
                );
            }
        }
    }

    #[test]
    fn status_code_maps_known_variants() {
        use fynd_core::QuoteStatus;
        assert_eq!(quote_status_code(QuoteStatus::Success), "success");
        assert_eq!(quote_status_code(QuoteStatus::NoRouteFound), "no_route_found");
        assert_eq!(quote_status_code(QuoteStatus::InsufficientLiquidity), "insufficient_liquidity");
        assert_eq!(quote_status_code(QuoteStatus::Timeout), "timeout");
        assert_eq!(quote_status_code(QuoteStatus::NotReady), "not_ready");
        assert_eq!(quote_status_code(QuoteStatus::PriceCheckFailed), "price_check_failed");
    }

    /// Shared in-memory buffer so a test can read what the subscriber wrote.
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = SharedBuffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_target(true)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        buffer.contents()
    }

    #[test]
    fn logs_ok_outcome_with_statuses() {
        let logs = capture_logs(|| {
            log_request_capture(
                2,
                r#"{"orders":[]}"#,
                &RequestOutcome::Solved {
                    solve_time_ms: 12,
                    order_statuses: vec!["success", "no_route_found"],
                    no_route_reasons: vec!["", "no_graph_path"],
                },
            );
        });
        assert!(logs.contains("event"), "logs were: {logs}");
        assert!(logs.contains("quote_failure"), "logs were: {logs}");
        assert!(logs.contains("outcome"), "logs were: {logs}");
        assert!(logs.contains("ok"), "logs were: {logs}");
        assert!(logs.contains("no_route_found"), "logs were: {logs}");
        assert!(logs.contains("num_orders"), "logs were: {logs}");
    }

    #[test]
    fn no_route_reason_code_maps_variants() {
        use fynd_core::NoPathReason;
        assert_eq!(no_route_reason_code(None), "");
        assert_eq!(
            no_route_reason_code(Some(NoPathReason::SourceTokenNotInGraph)),
            "source_token_not_in_graph"
        );
        assert_eq!(
            no_route_reason_code(Some(NoPathReason::DestinationTokenNotInGraph)),
            "destination_token_not_in_graph"
        );
        assert_eq!(no_route_reason_code(Some(NoPathReason::NoGraphPath)), "no_graph_path");
        assert_eq!(no_route_reason_code(Some(NoPathReason::NoScorablePaths)), "no_scorable_paths");
    }

    #[test]
    fn logs_ok_outcome_with_no_route_reasons() {
        let logs = capture_logs(|| {
            log_request_capture(
                1,
                r#"{"orders":[]}"#,
                &RequestOutcome::Solved {
                    solve_time_ms: 5,
                    order_statuses: vec!["no_route_found"],
                    no_route_reasons: vec!["destination_token_not_in_graph"],
                },
            );
        });
        assert!(logs.contains("no_route_reasons"), "logs were: {logs}");
        assert!(logs.contains("destination_token_not_in_graph"), "logs were: {logs}");
    }

    #[test]
    fn logs_failed_outcome_with_code() {
        let logs = capture_logs(|| {
            log_request_capture(1, r#"{"orders":[]}"#, &RequestOutcome::Failed { code: "TIMEOUT" });
        });
        assert!(logs.contains("quote_failure"), "logs were: {logs}");
        assert!(logs.contains("TIMEOUT"), "logs were: {logs}");
    }

    #[test]
    fn is_failure_true_for_failed_outcome() {
        assert!(RequestOutcome::Failed { code: "TIMEOUT" }.is_failure());
    }

    #[test]
    fn is_failure_false_when_all_orders_succeed() {
        let outcome = RequestOutcome::Solved {
            solve_time_ms: 5,
            order_statuses: vec!["success", "success"],
            no_route_reasons: vec!["", ""],
        };
        assert!(!outcome.is_failure());
    }

    #[test]
    fn is_failure_true_when_any_order_not_success() {
        let outcome = RequestOutcome::Solved {
            solve_time_ms: 5,
            order_statuses: vec!["success", "no_route_found"],
            no_route_reasons: vec!["", "no_graph_path"],
        };
        assert!(outcome.is_failure());
    }
}
