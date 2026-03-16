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
    pub orders_found: usize,
    pub orders_not_found: usize,
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

/// Dispatch to the scheduling strategy selected by `mode`.
pub async fn run_benchmark(
    client: Arc<FyndClient>,
    requests: &[SwapRequest],
    num_requests: usize,
    mode: &ParallelizationMode,
) -> RunnerResults {
    mode.run(client, requests, num_requests)
        .await
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
    let mut total_orders_found = 0usize;
    let mut total_orders_not_found = 0usize;

    tracing::info!("Running {} requests sequentially...", num_requests);

    for i in 1..=num_requests {
        print!("Request {}/{}: ", i, num_requests);
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let template = fastrand::choice(requests).unwrap();
        let params = template.to_quote_params();

        let start = Instant::now();
        let result = client.quote(params).await;
        let round_trip_ms = start.elapsed().as_millis() as u64;

        if let Some((solve_time, _is_first, found, not_found)) =
            handle_result(result, round_trip_ms, i == 1)
        {
            successful_requests += 1;
            round_trip_times.push(round_trip_ms);
            solve_times.push(solve_time);
            total_orders_found += found;
            total_orders_not_found += not_found;
        }
    }

    RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_found: total_orders_found,
        orders_not_found: total_orders_not_found,
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
    let orders_found = Arc::new(Mutex::new(0usize));
    let orders_not_found = Arc::new(Mutex::new(0usize));
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
        let orders_found = orders_found.clone();
        let orders_not_found = orders_not_found.clone();
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

            if let Some((solve_time, _printed_first, found, not_found)) =
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
                *orders_found.lock().await += found;
                *orders_not_found.lock().await += not_found;
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
    let orders_found = Arc::try_unwrap(orders_found)
        .unwrap()
        .into_inner();
    let orders_not_found = Arc::try_unwrap(orders_not_found)
        .unwrap()
        .into_inner();

    RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_found,
        orders_not_found,
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
    let orders_found = Arc::new(Mutex::new(0usize));
    let orders_not_found = Arc::new(Mutex::new(0usize));
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
        let orders_found = orders_found.clone();
        let orders_not_found = orders_not_found.clone();
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

            if let Some((solve_time, _printed_first, found, not_found)) =
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
                *orders_found.lock().await += found;
                *orders_not_found.lock().await += not_found;
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
    let orders_found = Arc::try_unwrap(orders_found)
        .unwrap()
        .into_inner();
    let orders_not_found = Arc::try_unwrap(orders_not_found)
        .unwrap()
        .into_inner();

    RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_found,
        orders_not_found,
    }
}

/// Extract timing and order counts from a quote result.
/// Returns `None` on failure (logged at error level).
fn handle_result(
    result: Result<Quote, FyndError>,
    round_trip_ms: u64,
    is_first: bool,
) -> Option<(u64, bool, usize, usize)> {
    match result {
        Ok(quote) => {
            let orders_found = usize::from(quote.status() == QuoteStatus::Success);
            let orders_not_found = 1 - orders_found;

            tracing::info!(
                "✓ Round-trip: {}ms, Server solve time: {}ms, Orders solved: {}/1",
                round_trip_ms,
                quote.solve_time_ms(),
                orders_found,
            );

            if is_first {
                tracing::info!("First quote details:");
                tracing::info!(
                    "  Order 0: status={:?}, amount_out={}",
                    quote.status(),
                    quote.amount_out(),
                );
            }

            Some((quote.solve_time_ms(), is_first, orders_found, orders_not_found))
        }
        Err(e) => {
            tracing::error!("✗ Request failed: {}", e);
            None
        }
    }
}
