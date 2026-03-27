//! Benchmark execution engine.
//!
//! Implements three scheduling strategies (sequential, fixed-concurrency,
//! rate-based) that send quote requests and collect timing data.

use std::{sync::Arc, time::Instant};

use fynd_client::{FyndClient, FyndError, Quote, QuoteStatus};
use tokio::sync::{Mutex, Semaphore};

use crate::{config::ParallelizationMode, requests::SwapRequest};

pub struct RunnerResults {
    pub round_trip_times: Vec<u64>,
    pub solve_times: Vec<u64>,
    pub successful_requests: usize,
    pub orders_solved: usize,
    pub orders_unsolved: usize,
}

impl ParallelizationMode {
    /// Run benchmark with this parallelization mode
    pub async fn run(
        &self,
        client: Arc<FyndClient>,
        requests: &[SwapRequest],
        num_requests: usize,
    ) -> RunnerResults {
        match self {
            Self::Sequential => run_sequential(client, requests, num_requests).await,
            Self::FixedConcurrency { concurrency } => {
                run_fixed_concurrency(client, requests, num_requests, *concurrency).await
            }
            Self::RateBased { interval_ms } => {
                run_rate_based(client, requests, num_requests, *interval_ms).await
            }
        }
    }
}

/// Sequential execution: wait for each response before firing the next request
async fn run_sequential(
    client: Arc<FyndClient>,
    requests: &[SwapRequest],
    num_requests: usize,
) -> RunnerResults {
    let mut round_trip_times = Vec::new();
    let mut solve_times = Vec::new();
    let mut successful_requests = 0;
    let mut total_solved = 0usize;
    let mut total_unsolved = 0usize;

    tracing::info!("Running {} requests sequentially...", num_requests);

    for i in 1..=num_requests {
        print!("Request {}/{}: ", i, num_requests);
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let template = fastrand::choice(requests).unwrap();
        let params = template.to_quote_params();

        let start = Instant::now();
        let result = client.quote(params).await;
        let round_trip_ms = start.elapsed().as_millis() as u64;

        if let Some((solve_time, solved, unsolved)) = handle_result(result, round_trip_ms, i == 1) {
            successful_requests += 1;
            round_trip_times.push(round_trip_ms);
            solve_times.push(solve_time);
            total_solved += solved;
            total_unsolved += unsolved;
        }
    }

    RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_solved: total_solved,
        orders_unsolved: total_unsolved,
    }
}

/// Fixed concurrency execution: maintain exactly N concurrent requests at all times.
/// When one request completes, immediately fire a new one to maintain the concurrency level.
async fn run_fixed_concurrency(
    client: Arc<FyndClient>,
    requests: &[SwapRequest],
    num_requests: usize,
    concurrency: usize,
) -> RunnerResults {
    tracing::info!("Running {} requests with fixed concurrency of {}", num_requests, concurrency);

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let requests = Arc::new(requests.to_vec());
    let round_trip_times = Arc::new(Mutex::new(Vec::new()));
    let solve_times = Arc::new(Mutex::new(Vec::new()));
    let successful_requests = Arc::new(Mutex::new(0usize));
    let orders_solved = Arc::new(Mutex::new(0usize));
    let orders_unsolved = Arc::new(Mutex::new(0usize));
    let completed_count = Arc::new(Mutex::new(0usize));
    let first_response_printed = Arc::new(Mutex::new(false));

    let mut tasks = Vec::new();

    for _ in 1..=num_requests {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let client = client.clone();
        let requests = requests.clone();
        let round_trip_times = round_trip_times.clone();
        let solve_times = solve_times.clone();
        let successful_requests = successful_requests.clone();
        let orders_solved = orders_solved.clone();
        let orders_unsolved = orders_unsolved.clone();
        let completed_count = completed_count.clone();
        let first_response_printed = first_response_printed.clone();

        let task = tokio::spawn(async move {
            let template = fastrand::choice(requests.as_slice()).unwrap();
            let params = template.to_quote_params();

            let start = Instant::now();
            let result = client.quote(params).await;
            let round_trip_ms = start.elapsed().as_millis() as u64;

            let mut first_printed = first_response_printed.lock().await;
            let is_first = !*first_printed;
            if is_first {
                *first_printed = true;
            }
            drop(first_printed);

            let mut count = completed_count.lock().await;
            *count += 1;
            let current_count = *count;
            drop(count);

            print!("Request {}/{}: ", current_count, num_requests);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            if let Some((solve_time, solved, unsolved)) =
                handle_result(result, round_trip_ms, is_first)
            {
                round_trip_times
                    .lock()
                    .await
                    .push(round_trip_ms);
                solve_times
                    .lock()
                    .await
                    .push(solve_time);
                *successful_requests.lock().await += 1;
                *orders_solved.lock().await += solved;
                *orders_unsolved.lock().await += unsolved;
            }

            drop(permit);
        });

        tasks.push(task);
    }

    for task in tasks {
        task.await.ok();
    }

    let round_trip_times = Arc::try_unwrap(round_trip_times)
        .unwrap()
        .into_inner();
    let solve_times = Arc::try_unwrap(solve_times)
        .unwrap()
        .into_inner();
    let successful_requests = Arc::try_unwrap(successful_requests)
        .unwrap()
        .into_inner();
    let orders_solved = Arc::try_unwrap(orders_solved)
        .unwrap()
        .into_inner();
    let orders_unsolved = Arc::try_unwrap(orders_unsolved)
        .unwrap()
        .into_inner();

    RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_solved,
        orders_unsolved,
    }
}

/// Rate-based execution: fire requests at a fixed interval regardless of response timing.
/// All requests are spawned as independent tasks (fire-and-forget pattern).
async fn run_rate_based(
    client: Arc<FyndClient>,
    requests: &[SwapRequest],
    num_requests: usize,
    interval_ms: u64,
) -> RunnerResults {
    tracing::info!(
        "Running {} requests at {}ms intervals (fire-and-forget)",
        num_requests,
        interval_ms
    );

    let requests = Arc::new(requests.to_vec());
    let round_trip_times = Arc::new(Mutex::new(Vec::new()));
    let solve_times = Arc::new(Mutex::new(Vec::new()));
    let successful_requests = Arc::new(Mutex::new(0usize));
    let orders_solved = Arc::new(Mutex::new(0usize));
    let orders_unsolved = Arc::new(Mutex::new(0usize));
    let first_response_printed = Arc::new(Mutex::new(false));

    let mut tasks = Vec::new();
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    for i in 1..=num_requests {
        interval.tick().await;

        let client = client.clone();
        let requests = requests.clone();
        let round_trip_times = round_trip_times.clone();
        let solve_times = solve_times.clone();
        let successful_requests = successful_requests.clone();
        let orders_solved = orders_solved.clone();
        let orders_unsolved = orders_unsolved.clone();
        let first_response_printed = first_response_printed.clone();

        let task = tokio::spawn(async move {
            let template = fastrand::choice(requests.as_slice()).unwrap();
            let params = template.to_quote_params();

            let start = Instant::now();
            let result = client.quote(params).await;
            let round_trip_ms = start.elapsed().as_millis() as u64;

            let mut first_printed = first_response_printed.lock().await;
            let is_first = !*first_printed;
            if is_first {
                *first_printed = true;
            }
            drop(first_printed);

            print!("Request {}: ", i);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            if let Some((solve_time, solved, unsolved)) =
                handle_result(result, round_trip_ms, is_first)
            {
                round_trip_times
                    .lock()
                    .await
                    .push(round_trip_ms);
                solve_times
                    .lock()
                    .await
                    .push(solve_time);
                *successful_requests.lock().await += 1;
                *orders_solved.lock().await += solved;
                *orders_unsolved.lock().await += unsolved;
            }
        });

        tasks.push(task);
    }

    for task in tasks {
        task.await.ok();
    }

    let round_trip_times = Arc::try_unwrap(round_trip_times)
        .unwrap()
        .into_inner();
    let solve_times = Arc::try_unwrap(solve_times)
        .unwrap()
        .into_inner();
    let successful_requests = Arc::try_unwrap(successful_requests)
        .unwrap()
        .into_inner();
    let orders_solved = Arc::try_unwrap(orders_solved)
        .unwrap()
        .into_inner();
    let orders_unsolved = Arc::try_unwrap(orders_unsolved)
        .unwrap()
        .into_inner();

    RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_solved,
        orders_unsolved,
    }
}

/// Pure logic: given quote status and solve time, return (solve_time, found, not_found).
fn classify_quote(status: QuoteStatus, solve_time_ms: u64) -> (u64, usize, usize) {
    let solved = usize::from(status == QuoteStatus::Success);
    (solve_time_ms, solved, 1 - solved)
}

/// Extract timing and order counts from a quote result.
/// Returns `None` on failure (logged at error level).
fn handle_result(
    result: Result<Quote, FyndError>,
    round_trip_ms: u64,
    is_first: bool,
) -> Option<(u64, usize, usize)> {
    match result {
        Ok(quote) => {
            let (solve_time, solved, unsolved) =
                classify_quote(quote.status(), quote.solve_time_ms());

            tracing::info!(
                "✓ Round-trip: {}ms, Server solve time: {}ms, Orders solved: {}/1",
                round_trip_ms,
                solve_time,
                solved,
            );

            if is_first {
                tracing::info!("First quote details:");
                tracing::info!(
                    "  Order 0: status={:?}, amount_out={}",
                    quote.status(),
                    quote.amount_out(),
                );
            }

            Some((solve_time, solved, unsolved))
        }
        Err(e) => {
            tracing::error!("✗ Request failed: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fynd_client::{FyndClientBuilder, QuoteStatus};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    fn minimal_quote_json() -> serde_json::Value {
        serde_json::json!({
            "orders": [{
                "order_id": "bench-1",
                "status": "success",
                "amount_in": "1000000",
                "amount_out": "990000",
                "gas_estimate": "50000",
                "amount_out_net_gas": "940000",
                "price_impact_bps": 10,
                "block": {"number": 1, "hash": "0xabc", "timestamp": 1700000000}
            }],
            "total_gas_estimate": "50000",
            "solve_time_ms": 42
        })
    }

    fn test_requests() -> Vec<SwapRequest> {
        vec![crate::requests::default_request(10000)]
    }

    async fn setup_mock(response: ResponseTemplate) -> (MockServer, Arc<FyndClient>) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/quote"))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = Arc::new(
            FyndClientBuilder::new(server.uri())
                .build_quote_only()
                .unwrap(),
        );
        (server, client)
    }

    // classify_quote tests

    #[test]
    fn classify_success() {
        let (solve, found, not_found) = classify_quote(QuoteStatus::Success, 42);
        assert_eq!(solve, 42);
        assert_eq!(found, 1);
        assert_eq!(not_found, 0);
    }

    #[test]
    fn classify_no_route() {
        let (solve, found, not_found) = classify_quote(QuoteStatus::NoRouteFound, 10);
        assert_eq!(solve, 10);
        assert_eq!(found, 0);
        assert_eq!(not_found, 1);
    }

    #[test]
    fn classify_timeout() {
        let (solve, found, not_found) = classify_quote(QuoteStatus::Timeout, 0);
        assert_eq!(solve, 0);
        assert_eq!(found, 0);
        assert_eq!(not_found, 1);
    }

    #[test]
    fn classify_insufficient_liquidity() {
        let (solve, found, not_found) = classify_quote(QuoteStatus::InsufficientLiquidity, 5);
        assert_eq!(solve, 5);
        assert_eq!(found, 0);
        assert_eq!(not_found, 1);
    }

    #[test]
    fn classify_not_ready() {
        let (solve, found, not_found) = classify_quote(QuoteStatus::NotReady, 0);
        assert_eq!(solve, 0);
        assert_eq!(found, 0);
        assert_eq!(not_found, 1);
    }

    // Wiremock scheduling tests

    #[tokio::test]
    async fn sequential_collects_results() {
        let (_server, client) =
            setup_mock(ResponseTemplate::new(200).set_body_json(minimal_quote_json())).await;
        let results = run_sequential(client, &test_requests(), 3).await;
        assert_eq!(results.successful_requests, 3);
        assert_eq!(results.round_trip_times.len(), 3);
        assert_eq!(results.solve_times.len(), 3);
        assert!(results
            .solve_times
            .iter()
            .all(|&t| t > 0));
    }

    #[tokio::test]
    async fn fixed_concurrency_collects_results() {
        let (_server, client) =
            setup_mock(ResponseTemplate::new(200).set_body_json(minimal_quote_json())).await;
        let results = run_fixed_concurrency(client, &test_requests(), 5, 2).await;
        assert_eq!(results.successful_requests, 5);
        assert_eq!(results.round_trip_times.len(), 5);
        assert_eq!(results.solve_times.len(), 5);
    }

    #[tokio::test]
    async fn rate_based_collects_results() {
        let (_server, client) =
            setup_mock(ResponseTemplate::new(200).set_body_json(minimal_quote_json())).await;
        let results = run_rate_based(client, &test_requests(), 3, 10).await;
        assert_eq!(results.successful_requests, 3);
        assert_eq!(results.round_trip_times.len(), 3);
        assert_eq!(results.solve_times.len(), 3);
    }

    #[tokio::test]
    async fn sequential_handles_errors() {
        let (_server, client) =
            setup_mock(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "bad request",
                "code": "BAD_REQUEST"
            })))
            .await;
        let results = run_sequential(client, &test_requests(), 2).await;
        assert_eq!(results.successful_requests, 0);
        assert!(results.round_trip_times.is_empty());
        assert!(results.solve_times.is_empty());
    }
}
