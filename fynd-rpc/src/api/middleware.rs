//! HTTP metrics middleware.
//!
//! Records `http_request_duration_seconds{endpoint, method, status}` (histogram; no user
//! labels so bucket cardinality stays bounded) and
//! `http_requests_total{endpoint, status, user_identity, user_plan, client_version}`
//! (counter carrying the per-client dimensions).
//!
//! `User-Identity` and `X-User-Plan` are injected by the ph-nginx-auth proxy
//! (`fynd-api-auth` release) in hosted deployments; direct traffic (kube probes, staging
//! without the proxy) falls back to bounded sentinel values.

use std::time::Instant;

use actix_web::{
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    http::header::HeaderMap,
    middleware::Next,
};
use metrics::{counter, histogram};

/// Per-client label values extracted from proxy-injected headers.
pub(crate) struct ClientLabels {
    pub(crate) user_identity: String,
    pub(crate) user_plan: String,
    pub(crate) client_version: String,
}

impl ClientLabels {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let header_or = |name: &str, default: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or(default)
                .to_string()
        };
        Self {
            user_identity: header_or("user-identity", "unknown"),
            user_plan: header_or("x-user-plan", "none"),
            client_version: headers
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                .map(sanitize_client_version)
                .unwrap_or("unknown")
                .to_string(),
        }
    }
}

/// Accepts only `product/version` tokens (e.g. `fynd-client/0.9.0`) as label values;
/// anything else collapses to `other`. Raw User-Agents are client-controlled and would
/// mint unbounded Prometheus series.
fn sanitize_client_version(user_agent: &str) -> &str {
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let mut parts = user_agent.splitn(2, '/');
    let (Some(product), Some(version)) = (parts.next(), parts.next()) else { return "other" };
    let well_formed = user_agent.len() <= 64 &&
        !product.is_empty() &&
        !version.is_empty() &&
        product.chars().all(is_token_char) &&
        version.chars().all(is_token_char);
    if well_formed {
        user_agent
    } else {
        "other"
    }
}

/// Emits both HTTP metrics for one completed request.
pub(crate) fn record_request(
    endpoint: &str,
    method: &str,
    status: u16,
    elapsed: std::time::Duration,
    client: &ClientLabels,
) {
    histogram!(
        "http_request_duration_seconds",
        "endpoint" => endpoint.to_string(),
        "method" => method.to_string(),
        "status" => status.to_string(),
    )
    .record(elapsed.as_secs_f64());
    counter!(
        "http_requests_total",
        "endpoint" => endpoint.to_string(),
        "status" => status.to_string(),
        "user_identity" => client.user_identity.clone(),
        "user_plan" => client.user_plan.clone(),
        "client_version" => client.client_version.clone(),
    )
    .increment(1);
}

/// Actix middleware: times every request and records the metrics above.
pub(crate) async fn http_metrics_middleware(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    let start = Instant::now();
    // Matched route pattern, not the raw path: unmatched requests (scanners, typos) must
    // not mint unbounded label values.
    let endpoint = req
        .match_pattern()
        .unwrap_or_else(|| "other".to_string());
    let method = req.method().to_string();
    let client = ClientLabels::from_headers(req.headers());

    let result = next.call(req).await;

    let status = match &result {
        Ok(response) => response.status().as_u16(),
        Err(error) => error
            .as_response_error()
            .status_code()
            .as_u16(),
    };
    record_request(&endpoint, &method, status, start.elapsed(), &client);
    result
}

#[cfg(test)]
mod tests {
    use actix_web::http::header::{HeaderMap, HeaderName, HeaderValue};

    use super::*;

    /// Finds the debug value recorded for `name` carrying every label in `labels`.
    fn find_metric<'a>(
        recorded: &'a [(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
    ) -> &'a metrics_util::debugging::DebugValue {
        recorded
            .iter()
            .find(|(key, _, _, _)| {
                key.key().name() == name &&
                    labels
                        .iter()
                        .all(|(label_key, label_value)| {
                            key.key()
                                .labels()
                                .any(|l| l.key() == *label_key && l.value() == *label_value)
                        })
            })
            .map(|(_, _, _, value)| value)
            .unwrap_or_else(|| panic!("missing {name}{labels:?}, got {recorded:?}"))
    }

    #[test]
    fn client_labels_read_proxy_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("user-identity"), HeaderValue::from_static("alice"));
        headers.insert(HeaderName::from_static("x-user-plan"), HeaderValue::from_static("scale"));
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("fynd-client/0.9.0"),
        );

        let labels = ClientLabels::from_headers(&headers);
        assert_eq!(labels.user_identity, "alice");
        assert_eq!(labels.user_plan, "scale");
        assert_eq!(labels.client_version, "fynd-client/0.9.0");
    }

    #[test]
    fn client_labels_default_to_sentinels() {
        let labels = ClientLabels::from_headers(&HeaderMap::new());
        assert_eq!(labels.user_identity, "unknown");
        assert_eq!(labels.user_plan, "none");
        assert_eq!(labels.client_version, "unknown");
    }

    #[test]
    fn sanitize_client_version_accepts_product_token() {
        assert_eq!(sanitize_client_version("fynd-client/0.9.0"), "fynd-client/0.9.0");
    }

    #[test]
    fn sanitize_client_version_rejects_browser_user_agent() {
        // Contains spaces and parens, which are not valid token characters.
        assert_eq!(sanitize_client_version("Mozilla/5.0 (X11; Linux) AppleWebKit/537.36"), "other");
    }

    #[test]
    fn sanitize_client_version_rejects_over_length_cap() {
        let long_version = "a".repeat(64);
        let user_agent = format!("fynd-client/{long_version}");
        assert!(user_agent.len() > 64);
        assert_eq!(sanitize_client_version(&user_agent), "other");
    }

    #[test]
    fn client_labels_sanitizes_garbage_user_agent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("Mozilla/5.0 (X11; Linux) AppleWebKit/537.36"),
        );
        let labels = ClientLabels::from_headers(&headers);
        assert_eq!(labels.client_version, "other");
    }

    #[test]
    fn record_request_emits_both_metrics() {
        use metrics_util::debugging::DebugValue;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_request(
                "/v1/quote",
                "POST",
                200,
                std::time::Duration::from_millis(42),
                &ClientLabels {
                    user_identity: "alice".to_string(),
                    user_plan: "scale".to_string(),
                    client_version: "fynd-client/0.9.0".to_string(),
                },
            );
        });

        let recorded = snapshotter.snapshot().into_vec();

        match find_metric(
            &recorded,
            "http_request_duration_seconds",
            &[("endpoint", "/v1/quote"), ("method", "POST"), ("status", "200")],
        ) {
            DebugValue::Histogram(samples) => {
                assert_eq!(samples.len(), 1, "expected exactly one recorded duration sample");
                let sample = samples[0].into_inner();
                assert!(
                    (sample - 0.042).abs() < 1e-9,
                    "expected duration sample ~0.042, got {sample}"
                );
            }
            other => panic!("http_request_duration_seconds is not a histogram: {other:?}"),
        }

        match find_metric(
            &recorded,
            "http_requests_total",
            &[
                ("endpoint", "/v1/quote"),
                ("status", "200"),
                ("user_identity", "alice"),
                ("user_plan", "scale"),
                ("client_version", "fynd-client/0.9.0"),
            ],
        ) {
            DebugValue::Counter(value) => {
                assert_eq!(*value, 1, "expected counter == 1, got {value}");
            }
            other => panic!("http_requests_total is not a counter: {other:?}"),
        }
    }

    #[test]
    fn http_metrics_middleware_records_matched_and_unmatched_requests() {
        use actix_web::{web, App, HttpResponse};
        use metrics_util::debugging::DebugValue;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        // A plain current-thread runtime, not #[actix_web::test]: the local recorder is a
        // thread-local, and actix's own test executor does not guarantee the middleware runs
        // on the same OS thread that installed the recorder.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let app = actix_web::test::init_service(
                    App::new()
                        .wrap(actix_web::middleware::from_fn(http_metrics_middleware))
                        .route("/v1/thing", web::get().to(HttpResponse::Ok)),
                )
                .await;

                let matched = actix_web::test::TestRequest::get()
                    .uri("/v1/thing")
                    .to_request();
                let matched_resp = actix_web::test::call_service(&app, matched).await;
                assert_eq!(matched_resp.status().as_u16(), 200);

                let unmatched = actix_web::test::TestRequest::get()
                    .uri("/does-not-exist")
                    .to_request();
                let unmatched_resp = actix_web::test::call_service(&app, unmatched).await;
                assert_eq!(unmatched_resp.status().as_u16(), 404);
            });
        });

        let recorded = snapshotter.snapshot().into_vec();

        match find_metric(
            &recorded,
            "http_requests_total",
            &[("endpoint", "/v1/thing"), ("status", "200")],
        ) {
            DebugValue::Counter(value) => {
                assert_eq!(*value, 1, "expected matched-route counter == 1, got {value}");
            }
            other => panic!("http_requests_total (matched) is not a counter: {other:?}"),
        }

        match find_metric(
            &recorded,
            "http_requests_total",
            &[("endpoint", "other"), ("status", "404")],
        ) {
            DebugValue::Counter(value) => {
                assert_eq!(*value, 1, "expected unmatched-route counter == 1, got {value}");
            }
            other => panic!("http_requests_total (unmatched) is not a counter: {other:?}"),
        }
    }
}
