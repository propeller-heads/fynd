//! The router's per-quote logs: the comparison line each worker pool writes for an order, and the
//! protocol line the winning quote writes.
//!
//! Both are plain `key=value` text on their own target, so a log pipeline reads each as one table.
//! The payload of each line is one preformatted string rather than tracing fields, because the
//! formatter wraps field names and their `=` separators in ANSI escapes, which defeats a logfmt
//! parser downstream.
//!
//! Each line carries `parent: None`. A quote is served inside the HTTP request span, and the
//! formatter appends the fields of every span in scope to the line: the request id, the method,
//! the route, the client address, the user agent and the OpenTelemetry keys. None of that
//! describes the quote, and it is several times the length of the record itself. Detaching the
//! event from the span leaves the line as written.

use std::collections::BTreeMap;

use num_traits::ToPrimitive;
use tracing::{trace, Level};

use super::{
    is_rankable, Order, OrderQuote, OrderResponses, OrderSide, QuoteOptions, QuoteStatus,
    SolveError, WorkerPoolQuote, BPS_DENOMINATOR,
};

/// Target for the per-quote comparison log. Emitted at TRACE so a plain `RUST_LOG=info` leaves
/// it off; a deployment that wants it sets `RUST_LOG=...,fynd::quote_comparison=trace`.
const QUOTE_COMPARISON_TARGET: &str = "fynd::quote_comparison";

/// How many worker pools covered an order.
#[derive(Clone, Copy)]
struct OrderCoverage {
    /// Pools whose quote was eligible for ranking.
    ranked_candidates: usize,
    /// Pools that answered at all, counting the ones that failed.
    responders: usize,
}

/// Emits one line per worker pool for an order: what it quoted, how long it took, and how far
/// ahead of the weakest quote it landed.
///
/// Covers every pool that answered, whatever its liquidity scope. `is_best` marks the pool whose
/// quote had the highest output net of gas. The amounts are the ones each pool solved; on
/// exclusive-liquidity deployments the amount finally quoted to the caller can be lower, because
/// `combine_with_surplus` withholds part of it.
pub(super) fn log_quote_comparison(
    order: &Order,
    responses: &OrderResponses,
    options: &QuoteOptions,
) {
    if !tracing::enabled!(target: QUOTE_COMPARISON_TARGET, Level::TRACE) {
        return;
    }

    let mut ranked: Vec<&WorkerPoolQuote> = responses
        .quotes
        .iter()
        .filter(|wq| is_rankable(&wq.quote, options))
        .collect();
    ranked.sort_by(|a, b| {
        b.quote
            .amount_out_net_gas()
            .cmp(a.quote.amount_out_net_gas())
    });

    // Improvement is measured against the weakest ranked quote, so 0 marks the floor and every
    // other pool reports what it added over it.
    let baseline_net = ranked
        .last()
        .and_then(|wq| wq.quote.amount_out_net_gas().to_f64());
    let best_pool = ranked
        .first()
        .map(|wq| wq.worker_pool.as_str());
    let coverage = OrderCoverage {
        ranked_candidates: ranked.len(),
        responders: responses.quotes.len() + responses.failed_solvers.len(),
    };

    for worker_quote in &responses.quotes {
        // Only ranked quotes have a comparable output; one that lost on status or `max_gas` is
        // not measured against a baseline it never competed for.
        let improvement = ranked
            .iter()
            .any(|wq| wq.worker_pool == worker_quote.worker_pool)
            .then(|| {
                improvement_bps(
                    baseline_net,
                    worker_quote
                        .quote
                        .amount_out_net_gas()
                        .to_f64(),
                )
            })
            .flatten();

        log_quote(
            order,
            worker_quote,
            coverage,
            improvement,
            best_pool == Some(worker_quote.worker_pool.as_str()),
        );
    }

    for (worker_pool, error) in &responses.failed_solvers {
        log_failure(order, worker_pool, error, coverage);
    }
}

/// Logs what one worker pool solved.
///
/// Named for the response rather than the outcome: a pool that answered can still carry a
/// non-success status, and it gets a line either way.
fn log_quote(
    order: &Order,
    worker_quote: &WorkerPoolQuote,
    coverage: OrderCoverage,
    improvement_bps: Option<f64>,
    is_best: bool,
) {
    let quote = &worker_quote.quote;
    trace!(
        target: QUOTE_COMPARISON_TARGET,
        parent: None,
        "quote_comparison order_id={} block={} token_in={} token_out={} side={} amount_in={} \
         pool={} algorithm={} status={} amount_out={} amount_out_net_gas={} gas_estimate={} \
         solve_time_ms={} improvement_bps={} is_best={} ranked_candidates={} responders={}",
        order.id(),
        quote.block().number(),
        order.token_in(),
        order.token_out(),
        order_side_label(order.side()),
        quote.amount_in(),
        worker_quote.worker_pool,
        quote.algorithm(),
        quote_status_label(quote.status()),
        quote.amount_out(),
        quote.amount_out_net_gas(),
        quote.gas_estimate(),
        worker_quote.solve_time_ms,
        improvement_bps
            .map(|bps| format!("{bps:.4}"))
            .unwrap_or_default(),
        is_best,
        coverage.ranked_candidates,
        coverage.responders,
    );
}

/// Logs a worker pool that produced no quote at all.
///
/// Carries the same keys as [`log_quote`], with the quote-specific ones empty, so a logfmt
/// consumer reads one table; `test_failed_pool_line_shares_the_schema` holds the two in step.
fn log_failure(order: &Order, worker_pool: &str, error: &SolveError, coverage: OrderCoverage) {
    // A pool that timed out is the slowest of the order, and `Timeout` already carries how long
    // it took — the reason these lines exist at all.
    let solve_time_ms = match error {
        SolveError::Timeout { elapsed_ms } => elapsed_ms.to_string(),
        _ => String::new(),
    };
    trace!(
        target: QUOTE_COMPARISON_TARGET,
        parent: None,
        "quote_comparison order_id={} block= token_in={} token_out={} side={} amount_in={} \
         pool={} algorithm= status={} amount_out= amount_out_net_gas= gas_estimate= \
         solve_time_ms={} improvement_bps= is_best=false ranked_candidates={} responders={}",
        order.id(),
        order.token_in(),
        order.token_out(),
        order_side_label(order.side()),
        order.amount(),
        worker_pool,
        solver_error_label(error),
        solve_time_ms,
        coverage.ranked_candidates,
        coverage.responders,
    );
}

/// Short, stable label for a quote status, sharing the comparison log's lowercase vocabulary
/// with [`solver_error_label`].
fn quote_status_label(status: QuoteStatus) -> &'static str {
    match status {
        QuoteStatus::Success => "success",
        QuoteStatus::NoRouteFound => "no_route",
        QuoteStatus::InsufficientLiquidity => "insufficient_liquidity",
        QuoteStatus::Timeout => "timeout",
        QuoteStatus::NotReady => "not_ready",
        QuoteStatus::PriceCheckFailed => "price_check_failed",
    }
}

/// Short, stable label for an order side.
///
/// Matched exhaustively so that adding `Buy` fails to compile here: an exact-out order reports
/// its requested *output* under `Order::amount`, which is what the failure lines print as
/// `amount_in`, so that arm needs revisiting at the same time.
fn order_side_label(side: OrderSide) -> &'static str {
    match side {
        OrderSide::Sell => "sell",
    }
}

/// Short, stable label for a solver failure, used as a metric label and in the comparison log.
///
/// Matched exhaustively even though [`SolveError`] is `#[non_exhaustive]`: that attribute only
/// forces a wildcard outside the defining crate, so listing every variant here means a new one
/// fails to compile rather than silently joining a catch-all.
pub(super) fn solver_error_label(error: &SolveError) -> &'static str {
    match error {
        SolveError::Timeout { .. } => "timeout",
        SolveError::NoRouteFound { .. } => "no_route",
        SolveError::RouteRejected { .. } => "route_rejected",
        SolveError::InsufficientLiquidity { .. } => "insufficient_liquidity",
        SolveError::QueueFull => "queue_full",
        SolveError::Internal(_) => "internal",
        SolveError::InvalidWorkerPools(_) => "invalid_worker_pools",
        SolveError::PriceCheckFailed { .. } => "price_check_failed",
        SolveError::AlgorithmError(_) => "algorithm_error",
        SolveError::MarketDataStale { .. } => "market_data_stale",
        SolveError::InvalidOrder(_) => "invalid_order",
        SolveError::NotReady(_) => "not_ready",
        SolveError::ComputationFailed(_) => "computation_failed",
        SolveError::FailedEncoding(_) => "failed_encoding",
        SolveError::EncodingUnavailable(_) => "encoding_unavailable",
        SolveError::MaxGasExceeded => "max_gas_exceeded",
        SolveError::MissingData(_) => "missing_data",
        SolveError::SimulationFailed(_) => "simulation_failed",
    }
}

/// Basis points by which `net` beats `baseline_net`, where 0 means this quote is the weakest.
///
/// `None` when there is no usable baseline or no comparable output, which keeps an unmeasurable
/// quote out of the comparison instead of entering it at the floor.
fn improvement_bps(baseline_net: Option<f64>, net: Option<f64>) -> Option<f64> {
    let (baseline, net) = (baseline_net?, net?);
    if baseline <= 0.0 || !baseline.is_finite() || !net.is_finite() {
        return None;
    }
    Some((net - baseline) / baseline * f64::from(BPS_DENOMINATOR))
}

/// Target for the winning-quote protocol log. Emitted at TRACE, like the comparison log, so a
/// plain `RUST_LOG=info` leaves it off; a deployment that wants it sets
/// `RUST_LOG=...,fynd::winning_protocols=trace`.
const WINNING_PROTOCOLS_TARGET: &str = "fynd::winning_protocols";

/// Logs the protocols the winning quote swaps on, and how many swaps it makes on each.
///
/// The protocol counts are a JSON object; the rest of the line is `key=value` text. The counts are
/// swaps, not distinct pools: a split route that crosses two Uniswap V2 pools reports 2, which is
/// what the solution executes.
///
/// Only a successful quote with a route gets a line, so the log holds one line per returned route.
pub(super) fn log_winning_protocols(quote: &OrderQuote) {
    if !tracing::enabled!(target: WINNING_PROTOCOLS_TARGET, Level::TRACE) {
        return;
    }
    if quote.status() != QuoteStatus::Success {
        return;
    }
    let Some(route) = quote.route() else {
        return;
    };

    // BTreeMap, so the same set of protocols always serialises in the same order and two lines
    // can be compared as text.
    let mut swaps_per_protocol: BTreeMap<&str, usize> = BTreeMap::new();
    for swap in route.swaps() {
        *swaps_per_protocol
            .entry(swap.protocol())
            .or_default() += 1;
    }

    trace!(
        target: WINNING_PROTOCOLS_TARGET,
        parent: None,
        "winning_protocols order_id={} block={} pool={} algorithm={} swaps={} protocols={}",
        quote.order_id(),
        quote.block().number(),
        quote.worker_pool(),
        quote.algorithm(),
        route.swaps().len(),
        serde_json::to_string(&swaps_per_protocol).unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_simulation::tycho_common::{models::Address, Bytes};

    use super::*;
    use crate::{BlockInfo, OrderQuote};

    fn timed_worker_quote(
        worker_pool: &str,
        quote: OrderQuote,
        solve_time_ms: u64,
    ) -> WorkerPoolQuote {
        WorkerPoolQuote { worker_pool: worker_pool.to_string(), quote, solve_time_ms }
    }

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn comparison_quote(worker_pool: &str, net: u64, solve_time_ms: u64) -> WorkerPoolQuote {
        timed_worker_quote(
            worker_pool,
            OrderQuote::new(
                "o1".to_string(),
                QuoteStatus::Success,
                BigUint::from(1_000u64),
                BigUint::from(net + 10),
                BigUint::from(10u64),
                BigUint::from(net),
                BlockInfo::new(42, "0xabc".to_string(), 0),
                format!("{worker_pool}_algo"),
                Bytes::default(),
                Bytes::default(),
                "1".to_string(),
            ),
            solve_time_ms,
        )
    }

    fn comparison_order() -> Order {
        Order::new(
            make_address(0xAA),
            make_address(0xBB),
            BigUint::from(1_000u64),
            OrderSide::Sell,
            make_address(0xCC),
        )
        .with_id("o1".to_string())
    }

    /// Renders the comparison log for an order and returns one payload per line.
    fn comparison_payloads(responses: &OrderResponses) -> Vec<String> {
        capture_comparison(&comparison_order(), responses, &QuoteOptions::default())
    }

    /// Runs `log_quote_comparison` and returns the payload of each line it wrote.
    fn capture_comparison(
        order: &Order,
        responses: &OrderResponses,
        options: &QuoteOptions,
    ) -> Vec<String> {
        crate::worker_pool_router::log_capture::capture_payloads("quote_comparison ", || {
            log_quote_comparison(order, responses, options);
        })
    }

    fn responses_with(quotes: Vec<WorkerPoolQuote>) -> OrderResponses {
        OrderResponses { order_id: "o1".to_string(), quotes, failed_solvers: vec![] }
    }

    /// A quote is served inside the HTTP request span. The formatter appends the fields of every
    /// span in scope, so without `parent: None` the request id and the http/otel keys land on the
    /// end of each line, several times the length of the record.
    #[test]
    fn test_payload_carries_no_enclosing_span_fields() {
        let responses = responses_with(vec![comparison_quote("winner", 1_000, 3)]);

        let payloads =
            crate::worker_pool_router::log_capture::capture_payloads("quote_comparison ", || {
                let request =
                    tracing::info_span!("HTTP request", request_id = "abcd", http.method = "POST");
                let _entered = request.enter();
                log_quote_comparison(&comparison_order(), &responses, &QuoteOptions::default());
            });

        assert_eq!(payloads.len(), 1);
        for leaked in ["request_id", "http.method", "abcd"] {
            assert!(!payloads[0].contains(leaked), "{leaked} leaked into: {}", payloads[0]);
        }
    }

    /// Asserts on the rendered bytes with colour explicitly on, as it runs in a deployment.
    /// See [`ComparisonLine::render`] for why the payload is one string.
    #[test]
    fn test_comparison_payload_has_no_ansi_escapes() {
        let responses = responses_with(vec![comparison_quote("winner", 1_000, 3)]);
        let payloads =
            capture_comparison(&comparison_order(), &responses, &QuoteOptions::default());

        assert_eq!(payloads.len(), 1);
        assert!(!payloads[0].contains('\u{1b}'), "escape sequence in payload: {}", payloads[0]);
    }

    #[test]
    fn test_comparison_payload_is_logfmt_tokenised() {
        let line = comparison_line_for("winner", 1_000, 3);
        for token in line.split_whitespace() {
            assert!(token.contains('='), "token `{token}` is not key=value in: {line}");
        }
    }

    fn comparison_line_for(worker_pool: &str, net: u64, solve_time_ms: u64) -> String {
        let responses = responses_with(vec![comparison_quote(worker_pool, net, solve_time_ms)]);
        comparison_payloads(&responses)
            .pop()
            .expect("one line per quote")
    }

    /// Improvement is measured against the weakest quote, so the floor reads 0 and the winner
    /// reports what it added over it.
    #[test]
    fn test_improvement_measured_against_weakest_quote() {
        let responses = responses_with(vec![
            comparison_quote("winner", 1_000, 3),
            comparison_quote("laggard", 900, 11),
        ]);
        let payloads = comparison_payloads(&responses);

        assert_eq!(payloads.len(), 2);
        // 1_000 over a 900 floor is 1_111.11 bps.
        assert!(payloads[0].contains("pool=winner"), "{}", payloads[0]);
        assert!(payloads[0].contains("improvement_bps=1111.1111"), "{}", payloads[0]);
        assert!(payloads[0].contains("is_best=true"), "{}", payloads[0]);
        assert!(payloads[1].contains("improvement_bps=0.0000"), "{}", payloads[1]);
        assert!(payloads[1].contains("is_best=false"), "{}", payloads[1]);
    }

    /// The solve time is the reason the change exists, so it has to reach the payload per pool.
    #[test]
    fn test_solve_time_is_reported_per_worker_pool() {
        let responses = responses_with(vec![
            comparison_quote("fast", 1_000, 3),
            comparison_quote("slow", 900, 417),
        ]);
        let payloads = comparison_payloads(&responses);

        assert!(payloads[0].contains("solve_time_ms=3"), "{}", payloads[0]);
        assert!(payloads[1].contains("solve_time_ms=417"), "{}", payloads[1]);
    }

    /// A pool that timed out is the slowest of the order; its line carries the elapsed time the
    /// error already knows about, under the same key as a successful pool.
    #[test]
    fn test_failed_pool_line_shares_the_schema() {
        let responses = OrderResponses {
            order_id: "o1".to_string(),
            quotes: vec![comparison_quote("winner", 1_000, 3)],
            failed_solvers: vec![("slowpoke".to_string(), SolveError::Timeout { elapsed_ms: 500 })],
        };
        let payloads = comparison_payloads(&responses);

        assert_eq!(payloads.len(), 2);
        let keys = |payload: &str| -> Vec<String> {
            payload
                .split_whitespace()
                .filter_map(|t| t.split_once('='))
                .map(|(k, _)| k.to_string())
                .collect()
        };
        assert_eq!(keys(&payloads[0]), keys(&payloads[1]), "both line kinds need one schema");
        assert!(payloads[1].contains("pool=slowpoke"), "{}", payloads[1]);
        assert!(payloads[1].contains("status=timeout"), "{}", payloads[1]);
        assert!(payloads[1].contains("solve_time_ms=500"), "{}", payloads[1]);
        assert!(payloads[1].contains("improvement_bps= "), "{}", payloads[1]);
        assert!(payloads[1].contains("responders=2"), "{}", payloads[1]);
    }

    /// A quote the request's `max_gas` rejected never competed, so it reports no improvement
    /// rather than one measured against a baseline it was not part of.
    #[test]
    fn test_quote_rejected_by_max_gas_reports_no_improvement() {
        let responses = responses_with(vec![
            comparison_quote("cheap", 900, 3),
            comparison_quote("expensive", 1_000, 4),
        ]);
        let options = QuoteOptions::default().with_max_gas(BigUint::from(1u64));
        let payloads = capture_comparison(&comparison_order(), &responses, &options);

        assert_eq!(payloads.len(), 2);
        for payload in &payloads {
            assert!(payload.contains("improvement_bps= "), "{payload}");
            assert!(payload.contains("ranked_candidates=0"), "{payload}");
            assert!(payload.contains("responders=2"), "{payload}");
        }
    }

    /// `responders` counts every pool that answered, not just the ones that survived ranking.
    #[test]
    fn test_responders_counts_answers_and_failures() {
        let responses = OrderResponses {
            order_id: "o1".to_string(),
            quotes: vec![comparison_quote("a", 1_000, 3), comparison_quote("b", 900, 4)],
            failed_solvers: vec![("c".to_string(), SolveError::QueueFull)],
        };
        let payloads = comparison_payloads(&responses);

        for payload in &payloads {
            assert!(payload.contains("responders=3"), "{payload}");
            assert!(payload.contains("ranked_candidates=2"), "{payload}");
        }
    }

    #[test]
    fn test_non_success_quote_reports_no_improvement() {
        let mut quote = comparison_quote("stale", 900, 5);
        quote.quote = OrderQuote::new(
            "o1".to_string(),
            QuoteStatus::NotReady,
            BigUint::from(1_000u64),
            BigUint::ZERO,
            BigUint::ZERO,
            BigUint::ZERO,
            BlockInfo::new(42, "0xabc".to_string(), 0),
            "stale_algo".to_string(),
            Bytes::default(),
            Bytes::default(),
            "1".to_string(),
        );
        let responses = responses_with(vec![comparison_quote("ok", 1_000, 3), quote]);
        let payloads = comparison_payloads(&responses);

        assert!(payloads[1].contains("status=not_ready"), "{}", payloads[1]);
        assert!(payloads[1].contains("improvement_bps= "), "{}", payloads[1]);
    }

    #[rstest]
    // The floor is its own baseline, so it reports no improvement.
    #[case(Some(900.0), Some(900.0), Some(0.0))]
    // A ninth above the floor is 1_111.11 bps.
    #[case(Some(900.0), Some(1_000.0), Some(1_111.111_111_111_111))]
    // Nothing to measure against, or nothing to measure: stay out of the comparison rather than
    // enter at the floor, which would read as the weakest quote.
    #[case(None, Some(900.0), None)]
    #[case(Some(900.0), None, None)]
    #[case(Some(0.0), Some(900.0), None)]
    #[case(Some(f64::INFINITY), Some(900.0), None)]
    #[case(Some(900.0), Some(f64::NAN), None)]
    // Below the baseline is possible only if the caller passes an unranked quote; it stays signed
    // rather than clamping, so the anomaly is visible.
    #[case(Some(1_000.0), Some(900.0), Some(-1_000.0))]
    fn test_improvement_bps(
        #[case] baseline_net: Option<f64>,
        #[case] net: Option<f64>,
        #[case] expected: Option<f64>,
    ) {
        match (improvement_bps(baseline_net, net), expected) {
            (Some(got), Some(want)) => assert!((got - want).abs() < 1e-9, "got {got}, want {want}"),
            (got, want) => assert_eq!(got, want),
        }
    }

    #[rstest]
    #[case(SolveError::Timeout { elapsed_ms: 7 }, "timeout")]
    #[case(SolveError::NoRouteFound { order_id: "o1".to_string(), reason: None }, "no_route")]
    #[case(SolveError::QueueFull, "queue_full")]
    #[case(SolveError::MaxGasExceeded, "max_gas_exceeded")]
    #[case(SolveError::AlgorithmError("boom".to_string()), "algorithm_error")]
    #[case(SolveError::MissingData("gas".to_string()), "missing_data")]
    #[case(SolveError::SimulationFailed("revert".to_string()), "simulation_failed")]
    #[case(SolveError::NotReady("derived".to_string()), "not_ready")]
    fn test_solver_error_label(#[case] error: SolveError, #[case] expected: &str) {
        assert_eq!(solver_error_label(&error), expected);
    }

    #[rstest]
    #[case(QuoteStatus::Success, "success")]
    #[case(QuoteStatus::NoRouteFound, "no_route")]
    #[case(QuoteStatus::Timeout, "timeout")]
    #[case(QuoteStatus::NotReady, "not_ready")]
    #[case(QuoteStatus::InsufficientLiquidity, "insufficient_liquidity")]
    #[case(QuoteStatus::PriceCheckFailed, "price_check_failed")]
    fn test_quote_status_label(#[case] status: QuoteStatus, #[case] expected: &str) {
        assert_eq!(quote_status_label(status), expected);
    }

    /// The two label functions feed one `status` key, so their vocabularies must not overlap in
    /// spelling while meaning different things, nor diverge in casing.
    #[test]
    fn test_status_vocabularies_agree() {
        assert_eq!(
            quote_status_label(QuoteStatus::Timeout),
            solver_error_label(&SolveError::Timeout { elapsed_ms: 1 })
        );
        assert_eq!(
            quote_status_label(QuoteStatus::NoRouteFound),
            solver_error_label(&SolveError::NoRouteFound {
                order_id: "o1".to_string(),
                reason: None
            })
        );
    }
}
