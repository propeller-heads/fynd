//! Builds the routing-essential representation of a quote request for the
//! replay-capture log emitted by the `/v1/quote` handler.

use fynd_core::QuoteStatus;
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
struct ReplayOrder<'a> {
    token_in: &'a Address,
    token_out: &'a Address,
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

/// Routing-essential view of a whole quote request.
#[derive(Serialize)]
struct ReplayRequest<'a> {
    orders: Vec<ReplayOrder<'a>>,
    options: ReplayOptions,
}

/// Serializes `request` down to the routing-essential fields, as a JSON string,
/// for the replay log.
///
/// Captured (the fields that determine the route): per order `token_in`,
/// `token_out`, `amount`, `side`; plus the solve options `timeout_ms`,
/// `min_responses`, `max_gas`. Everything else is dropped: `encoding_options`
/// (slippage / transfer type / price guard, and every Permit2 / client-fee
/// **signature**), the server-generated order `id`, and `sender` / `receiver`
/// (routing-irrelevant and PII). Built from an explicit allowlist, so no
/// request field can leak into the logs.
///
/// The result is NOT a full [`QuoteRequest`] — it omits the required `sender`,
/// so a replay harness supplies a placeholder sender before re-issuing. An
/// outcome that depended on `price_guard` may not reproduce on replay.
pub(crate) fn replay_json(request: &QuoteRequest) -> String {
    let orders = request
        .orders()
        .iter()
        .map(|order| ReplayOrder {
            token_in: order.token_in(),
            token_out: order.token_out(),
            amount: order.amount().to_string(),
            side: order.side(),
        })
        .collect();
    let options = request.options();
    let capture = ReplayRequest {
        orders,
        options: ReplayOptions {
            timeout_ms: options.timeout_ms(),
            min_responses: options.min_responses(),
            max_gas: options
                .max_gas()
                .map(ToString::to_string),
        },
    };
    serde_json::to_string(&capture).unwrap_or_else(|_| "<unserializable>".to_string())
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

/// Recorded outcome of a solved request, for the replay-capture log.
pub(crate) enum RequestOutcome {
    /// Solve returned a quote; per-order status codes in request order.
    Solved {
        /// Total orchestration time reported by the router.
        solve_time_ms: u64,
        /// One [`quote_status_code`] per order, in request order.
        order_statuses: Vec<&'static str>,
    },
    /// Solve failed; carries the [`crate::api::error::solve_error_code`].
    Failed {
        /// Stable error code (e.g. `TIMEOUT`, `QUEUE_FULL`).
        code: &'static str,
    },
}

/// Emits the single `quote_request` replay-capture log line.
///
/// `request_json` is the sanitized, re-issuable request from
/// [`replay_json`]. Filter these in Loki with `event="quote_request"`.
pub(crate) fn log_request_capture(num_orders: usize, request_json: &str, outcome: &RequestOutcome) {
    match outcome {
        RequestOutcome::Solved { solve_time_ms, order_statuses } => info!(
            event = "quote_request",
            num_orders,
            solve_time_ms = *solve_time_ms,
            outcome = "ok",
            order_statuses = ?order_statuses,
            request = %request_json,
            "quote request captured"
        ),
        RequestOutcome::Failed { code } => info!(
            event = "quote_request",
            num_orders,
            outcome = *code,
            request = %request_json,
            "quote request captured"
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
                },
            );
        });
        assert!(logs.contains("event"), "logs were: {logs}");
        assert!(logs.contains("quote_request"), "logs were: {logs}");
        assert!(logs.contains("outcome"), "logs were: {logs}");
        assert!(logs.contains("ok"), "logs were: {logs}");
        assert!(logs.contains("no_route_found"), "logs were: {logs}");
        assert!(logs.contains("num_orders"), "logs were: {logs}");
    }

    #[test]
    fn logs_failed_outcome_with_code() {
        let logs = capture_logs(|| {
            log_request_capture(1, r#"{"orders":[]}"#, &RequestOutcome::Failed { code: "TIMEOUT" });
        });
        assert!(logs.contains("quote_request"), "logs were: {logs}");
        assert!(logs.contains("TIMEOUT"), "logs were: {logs}");
    }
}
