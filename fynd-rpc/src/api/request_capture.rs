//! Builds the routing-essential representation of a quote request for the
//! replay-capture log emitted by the `/v1/quote` handler.

use fynd_core::{ExclusiveAccess, NoPathReason, QuoteStatus, SolveError};
use fynd_rpc_types::{Address, OrderSide, QuoteRequest};
use serde::Serialize;
use tracing::{info, warn};

/// Routing-essential view of a single order, serialized into the replay log.
///
/// An allowlist by construction: only fields that affect route-finding are
/// present. The server-generated `id`, the routing-irrelevant `sender` /
/// `receiver` (also PII we keep out of logs), and all encoding data are
/// omitted — nothing is copied implicitly, so no DTO field can leak.
#[derive(Debug, Serialize)]
struct ReplayOrder {
    token_in: Address,
    token_out: Address,
    amount: String,
    side: OrderSide,
}

/// Routing-essential view of the request-level solve options.
#[derive(Debug, Serialize)]
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
/// `min_responses`, `max_gas`; plus `exclusive_access` — the access the
/// authenticating proxy granted, which decides the worker pool allocation.
/// Everything else is dropped: `encoding_options` (slippage / transfer type /
/// price guard, and every Permit2 / client-fee **signature**), the
/// server-generated order `id`, and `sender` / `receiver`
/// (routing-irrelevant and PII). Built from an explicit allowlist, so no
/// request field can leak into the logs.
///
/// This is NOT a full [`QuoteRequest`] — it omits the required `sender`, so a
/// replay harness supplies a placeholder sender before re-issuing.
/// `exclusive_access` is not a body field either: a harness re-sends it as the
/// `x-exclusive-access` header. An outcome that depended on `price_guard` may
/// not reproduce on replay.
#[derive(Debug, Serialize)]
pub struct ReplayRequest {
    orders: Vec<ReplayOrder>,
    options: ReplayOptions,
    exclusive_access: bool,
}

impl ReplayRequest {
    /// Extracts the owned routing-essential capture from `request`. Cheap — a
    /// few address clones and no JSON — so it is safe to run on the quote hot
    /// path; the serialization cost is deferred to [`ReplayRequest::to_json`],
    /// which the handler only calls off the response path.
    pub fn capture(request: &QuoteRequest, access: ExclusiveAccess) -> Self {
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
            exclusive_access: access == ExclusiveAccess::Granted,
        }
    }

    /// Serializes the capture to the replay-log JSON string.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "<unserializable>".to_string())
    }
}

/// Serializes `request` down to the routing-essential fields, as a JSON string.
/// Convenience wrapper over [`ReplayRequest::capture`] + [`ReplayRequest::to_json`].
#[cfg(test)]
pub(crate) fn replay_json(request: &QuoteRequest, access: ExclusiveAccess) -> String {
    ReplayRequest::capture(request, access).to_json()
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

/// Namespaced `group/reason` slug for a failed order slot, `""` for success.
///
/// Derived from the recorded [`SolveError`] cause when present, else from the
/// order status (price-guard rejections and legacy paths carry no cause).
/// Wildcard arms guard the foreign `#[non_exhaustive]` enums.
pub(crate) fn failure_reason_slug(status: QuoteStatus, cause: Option<&SolveError>) -> &'static str {
    if status == QuoteStatus::Success {
        return "";
    }
    let Some(cause) = cause else {
        return match status {
            QuoteStatus::NoRouteFound => "graph/other",
            QuoteStatus::InsufficientLiquidity => "graph/insufficient_liquidity",
            QuoteStatus::Timeout => "infra/timeout",
            QuoteStatus::NotReady => "data/not_ready",
            QuoteStatus::PriceCheckFailed => "guard/price_check_failed",
            _ => "unknown",
        };
    };
    match cause {
        SolveError::NoRouteFound { reason, .. } => match reason {
            Some(NoPathReason::SourceTokenNotInGraph) => "graph/source_token_not_in_graph",
            Some(NoPathReason::DestinationTokenNotInGraph) => {
                "graph/destination_token_not_in_graph"
            }
            Some(NoPathReason::NoGraphPath) => "graph/no_graph_path",
            Some(NoPathReason::NoScorablePaths) => "graph/no_scorable_paths",
            Some(NoPathReason::AmountTooSmall) => "graph/amount_too_small",
            Some(_) | None => "graph/other",
        },
        SolveError::InsufficientLiquidity { .. } => "graph/insufficient_liquidity",
        SolveError::MaxGasExceeded => "request/max_gas_exceeded",
        SolveError::MissingData(_) => "data/missing_data",
        SolveError::MarketDataStale { .. } => "data/market_data_stale",
        SolveError::ComputationFailed(_) => "data/computation_failed",
        SolveError::NotReady(_) => "data/not_ready",
        SolveError::SimulationFailed(_) => "algorithm/simulation_failed",
        SolveError::AlgorithmError(_) => "algorithm/algorithm_error",
        SolveError::QueueFull => "infra/queue_full",
        SolveError::Timeout { .. } => "infra/timeout",
        SolveError::Internal(_) => "infra/internal_error",
        _ => "unknown",
    }
}

/// Recorded outcome of a solved request, for the replay-capture log.
///
/// Both variants log one `failure_reasons` entry per order, so a breakdown by
/// reason counts order slots without branching on the outcome.
#[non_exhaustive]
pub enum RequestOutcome {
    /// Solve returned a quote; per-order status codes in request order.
    #[non_exhaustive]
    Solved {
        /// Total orchestration time reported by the router.
        solve_time_ms: u64,
        /// One `quote_status_code` per order, in request order.
        order_statuses: Vec<&'static str>,
        /// One `failure_reason_slug` per order, aligned with `order_statuses`
        /// (`""` for successful orders).
        failure_reasons: Vec<&'static str>,
    },
    /// Solve failed; carries the crate-internal solve-error code.
    ///
    /// The request-level error applies to every order, so the derived
    /// `http/<code>` reason is repeated `num_orders` times in the log line. No
    /// `order_statuses` are logged: the solver produced no per-order results.
    #[non_exhaustive]
    Failed {
        /// Stable error code (e.g. `TIMEOUT`, `QUEUE_FULL`).
        code: &'static str,
    },
}

impl RequestOutcome {
    /// Whether this outcome represents a failed quote: a solver error, or a
    /// solve in which at least one order did not succeed. Successful quotes
    /// (every order `success`) are not logged.
    pub fn is_failure(&self) -> bool {
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
/// `request_json` is the sanitized, re-issuable request from
/// [`ReplayRequest::to_json`].
/// Only failed quotes are logged; filter these in Loki with
/// `event="quote_failure"`.
///
/// `failure_reasons` always holds `num_orders` entries, whichever variant is
/// logged — see [`RequestOutcome`].
pub(crate) fn log_request_capture(num_orders: usize, request_json: &str, outcome: &RequestOutcome) {
    match outcome {
        RequestOutcome::Solved { solve_time_ms, order_statuses, failure_reasons } => info!(
            event = "quote_failure",
            num_orders,
            solve_time_ms = *solve_time_ms,
            outcome = "ok",
            order_statuses = ?order_statuses,
            failure_reasons = ?failure_reasons,
            request = %request_json,
            "quote failure captured"
        ),
        RequestOutcome::Failed { code } => {
            let reason = format!("http/{}", code.to_lowercase());
            let failure_reasons = vec![reason.as_str(); num_orders];
            info!(
                event = "quote_failure",
                num_orders,
                outcome = *code,
                failure_reasons = ?failure_reasons,
                request = %request_json,
                "quote failure captured"
            )
        }
    }
}

/// Threshold in milliseconds above which a successful solve is logged as slow.
pub const SLOW_SOLVE_THRESHOLD_MS: u64 = 200;

/// Emits a `slow_solve` warning when a successful solve exceeds the threshold.
///
/// Only successful quotes are logged here — failures are handled by
/// [`log_request_capture`]. Filter in Loki with `event="slow_solve"`.
///
/// When `threshold_ms` is `0`, logging is bypassed entirely.
/// `request_json` is the sanitized, re-issuable request from [`replay_json`],
/// included so slow solves can be reproduced.
pub(crate) fn log_slow_solve(
    solve_time_ms: u64,
    num_orders: usize,
    threshold_ms: u64,
    request_json: &str,
) {
    if threshold_ms == 0 || solve_time_ms <= threshold_ms {
        return;
    }
    warn!(
        event = "slow_solve",
        solve_time_ms,
        num_orders,
        threshold_ms,
        request = %request_json,
        "solve exceeded slow threshold"
    );
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
    use rstest::rstest;
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
        let json = replay_json(&request_with_signatures(), ExclusiveAccess::Denied);
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
        let json = replay_json(&request_with_signatures(), ExclusiveAccess::Denied);
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
        let json = replay_json(&req, ExclusiveAccess::Denied);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        let top_level: std::collections::BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top_level,
            ["orders", "options", "exclusive_access"]
                .into_iter()
                .collect(),
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

    #[rstest]
    #[case::granted(ExclusiveAccess::Granted, true)]
    #[case::denied(ExclusiveAccess::Denied, false)]
    fn test_replay_json_captures_exclusive_access(
        #[case] access: ExclusiveAccess,
        #[case] expected: bool,
    ) {
        let json = replay_json(&request_with_signatures(), access);
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["exclusive_access"], expected, "json was: {json}");
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
                    failure_reasons: vec!["", "graph/no_graph_path"],
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
    fn test_failure_reason_slug_maps_cause_classes() {
        use fynd_core::SolveError;
        let s = QuoteStatus::NoRouteFound;
        assert_eq!(
            failure_reason_slug(
                s,
                Some(&SolveError::no_route_found_with_reason("o", NoPathReason::NoGraphPath))
            ),
            "graph/no_graph_path"
        );
        assert_eq!(
            failure_reason_slug(
                s,
                Some(&SolveError::no_route_found_with_reason("o", NoPathReason::AmountTooSmall))
            ),
            "graph/amount_too_small"
        );
        assert_eq!(failure_reason_slug(s, Some(&SolveError::no_route_found("o"))), "graph/other");
        assert_eq!(
            failure_reason_slug(
                s,
                Some(&SolveError::insufficient_liquidity(1u32.into(), 0u32.into()))
            ),
            "graph/insufficient_liquidity"
        );
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::MaxGasExceeded)),
            "request/max_gas_exceeded"
        );
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::MissingData("gas price".to_string()))),
            "data/missing_data"
        );
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::SimulationFailed("p: e".to_string()))),
            "algorithm/simulation_failed"
        );
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::AlgorithmError("x".to_string()))),
            "algorithm/algorithm_error"
        );
        assert_eq!(failure_reason_slug(s, Some(&SolveError::QueueFull)), "infra/queue_full");
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::Internal("x".to_string()))),
            "infra/internal_error"
        );
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::NotReady("x".to_string()))),
            "data/not_ready"
        );
        assert_eq!(
            failure_reason_slug(s, Some(&SolveError::ComputationFailed("x".to_string()))),
            "data/computation_failed"
        );
    }

    #[test]
    fn test_failure_reason_slug_falls_back_to_status() {
        assert_eq!(failure_reason_slug(QuoteStatus::Success, None), "");
        assert_eq!(failure_reason_slug(QuoteStatus::Timeout, None), "infra/timeout");
        assert_eq!(failure_reason_slug(QuoteStatus::NotReady, None), "data/not_ready");
        assert_eq!(
            failure_reason_slug(QuoteStatus::PriceCheckFailed, None),
            "guard/price_check_failed"
        );
        assert_eq!(failure_reason_slug(QuoteStatus::NoRouteFound, None), "graph/other");
        assert_eq!(
            failure_reason_slug(QuoteStatus::InsufficientLiquidity, None),
            "graph/insufficient_liquidity"
        );
    }

    #[test]
    fn test_logs_ok_outcome_with_failure_reasons() {
        let logs = capture_logs(|| {
            log_request_capture(
                2,
                r#"{"orders":[]}"#,
                &RequestOutcome::Solved {
                    solve_time_ms: 12,
                    order_statuses: vec!["success", "no_route_found"],
                    failure_reasons: vec!["", "graph/no_graph_path"],
                },
            );
        });
        assert!(logs.contains("failure_reasons"), "logs were: {logs}");
        assert!(logs.contains("graph/no_graph_path"), "logs were: {logs}");
    }

    #[test]
    fn test_logs_failed_outcome_with_http_slug() {
        let logs = capture_logs(|| {
            log_request_capture(1, r#"{"orders":[]}"#, &RequestOutcome::Failed { code: "TIMEOUT" });
        });
        assert!(logs.contains("http/timeout"), "logs were: {logs}");
        assert!(logs.contains("TIMEOUT"), "logs were: {logs}");
    }

    #[test]
    fn test_logs_failed_outcome_reason_per_order() {
        let logs = capture_logs(|| {
            log_request_capture(3, r#"{"orders":[]}"#, &RequestOutcome::Failed { code: "TIMEOUT" });
        });
        assert!(
            logs.contains(r#"["http/timeout", "http/timeout", "http/timeout"]"#),
            "one reason per order expected; logs were: {logs}"
        );
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
            failure_reasons: vec!["", ""],
        };
        assert!(!outcome.is_failure());
    }

    #[test]
    fn is_failure_true_when_any_order_not_success() {
        let outcome = RequestOutcome::Solved {
            solve_time_ms: 5,
            order_statuses: vec!["success", "no_route_found"],
            failure_reasons: vec!["", "graph/no_graph_path"],
        };
        assert!(outcome.is_failure());
    }

    const TEST_REQUEST: &str = r#"{"orders":[]}"#;

    #[test]
    fn log_slow_solve_emits_when_above_threshold() {
        let logs = capture_logs(|| {
            log_slow_solve(250, 1, SLOW_SOLVE_THRESHOLD_MS, TEST_REQUEST);
        });
        assert!(logs.contains("slow_solve"), "logs were: {logs}");
        assert!(logs.contains("solve_time_ms"), "logs were: {logs}");
        assert!(logs.contains("250"), "logs were: {logs}");
        assert!(logs.contains("request"), "logs were: {logs}");
    }

    #[test]
    fn log_slow_solve_silent_when_at_threshold() {
        let logs = capture_logs(|| {
            log_slow_solve(SLOW_SOLVE_THRESHOLD_MS, 1, SLOW_SOLVE_THRESHOLD_MS, TEST_REQUEST);
        });
        assert!(
            !logs.contains("slow_solve"),
            "should not log at exactly threshold; logs were: {logs}"
        );
    }

    #[test]
    fn log_slow_solve_silent_when_below_threshold() {
        let logs = capture_logs(|| {
            log_slow_solve(50, 1, SLOW_SOLVE_THRESHOLD_MS, TEST_REQUEST);
        });
        assert!(!logs.contains("slow_solve"), "should not log below threshold; logs were: {logs}");
    }

    #[test]
    fn log_slow_solve_bypassed_when_threshold_zero() {
        let logs = capture_logs(|| {
            log_slow_solve(10_000, 1, 0, TEST_REQUEST);
        });
        assert!(
            !logs.contains("slow_solve"),
            "threshold=0 should bypass logging entirely; logs were: {logs}"
        );
    }

    #[test]
    fn log_slow_solve_includes_request_json() {
        let logs = capture_logs(|| {
            log_slow_solve(300, 2, SLOW_SOLVE_THRESHOLD_MS, r#"{"orders":[{"token_in":"0xaa"}]}"#);
        });
        assert!(logs.contains("0xaa"), "request JSON should appear in logs; logs were: {logs}");
    }
}
