//! Orchestrates multiple solver pools to find the best quote per request.
//!
//! The WorkerPoolRouter sits between the API layer and multiple solver pools.
//! It fans out each order to all configured solvers, manages timeouts,
//! and selects the best quote based on `amount_out_net_gas`.

//! # Responsibilities
//!
//! 1. **Fan-out**: Distribute each order to solver pools. Its distribution algorithm can be
//!    customized, but initially it's set to relay to all solvers.
//! 2. **Timeout**: Cancel if solver response takes too long
//! 3. **Collection**: Wait for N responses OR timeout per order
//! 4. **Selection**: Choose best quote (max `amount_out_net_gas`)

pub mod config;

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use config::WorkerPoolRouterConfig;
use futures::stream::{FuturesUnordered, StreamExt};
use metrics::{counter, histogram};
use num_bigint::BigUint;
use tracing::{debug, warn};
use tycho_simulation::tycho_common::Bytes;

use crate::{
    encoding::encoder::Encoder, worker_pool::task_queue::TaskQueueHandle, BlockInfo, Order,
    OrderQuote, Quote, QuoteOptions, QuoteRequest, QuoteStatus, SolveError,
};

/// Handle to a solver pool for dispatching orders.
#[derive(Clone)]
pub struct SolverPoolHandle {
    /// Human-readable name for this pool (used in logging & metrics).
    name: String,
    /// Queue handle for this pool.
    queue: TaskQueueHandle,
}

impl SolverPoolHandle {
    /// Creates a new solver pool handle.
    pub fn new(name: impl Into<String>, queue: TaskQueueHandle) -> Self {
        Self { name: name.into(), queue }
    }

    /// Returns the pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the task queue handle.
    pub fn queue(&self) -> &TaskQueueHandle {
        &self.queue
    }
}

/// Collected responses for a single order from multiple solvers.
#[derive(Debug)]
pub(crate) struct OrderResponses {
    /// ID of the order these responses correspond to.
    order_id: String,
    /// Quotes received from each solver pool (pool_name, quote).
    quotes: Vec<(String, OrderQuote)>,
    /// Solver pools that failed with their respective errors (pool_name, error).
    /// This captures all error types: timeouts, no routes, algorithm errors, etc.
    failed_solvers: Vec<(String, SolveError)>,
}

/// Orchestrates multiple solver pools to find the best quote.
pub struct WorkerPoolRouter {
    /// All registered solver pools.
    solver_pools: Vec<SolverPoolHandle>,
    /// Configuration for the worker router.
    config: WorkerPoolRouterConfig,
    /// Encoder for encoding solutions into on-chain transactions.
    encoder: Encoder,
}

impl WorkerPoolRouter {
    /// Creates a new WorkerPoolRouter with the given solver pools, config, and encoder.
    pub fn new(
        solver_pools: Vec<SolverPoolHandle>,
        config: WorkerPoolRouterConfig,
        encoder: Encoder,
    ) -> Self {
        Self { solver_pools, config, encoder }
    }

    /// Returns the number of registered solver pools.
    pub fn num_pools(&self) -> usize {
        self.solver_pools.len()
    }

    /// Returns a quote by fanning out to all solver pools.
    ///
    /// For each order in the request:
    /// 1. Sends the order to all solver pools in parallel
    /// 2. Waits for responses with timeout
    /// 3. Selects the best quote based on `amount_out_net_gas`
    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, SolveError> {
        let start = Instant::now();
        let deadline = start + self.effective_timeout(request.options());
        let min_responses = request
            .options()
            .min_responses()
            .unwrap_or(self.config.min_responses());

        if self.solver_pools.is_empty() {
            return Err(SolveError::Internal("no solver pools configured".to_string()));
        }

        // Process each order independently in parallel
        let order_futures: Vec<_> = request
            .orders()
            .iter()
            .map(|order| self.solve_order(order.clone(), deadline, min_responses))
            .collect();

        let order_responses = futures::future::join_all(order_futures).await;

        // Select best quote for each order
        let mut order_quotes: Vec<OrderQuote> = order_responses
            .into_iter()
            .map(|responses| self.select_best(&responses, request.options()))
            .collect();

        // Encode solutions if encoding_options is set
        if let Some(encoding_options) = request.options().encoding_options() {
            order_quotes = self
                .encoder
                .encode(order_quotes, encoding_options.clone())
                .await?;
        }

        // Calculate totals
        let total_gas_estimate = order_quotes
            .iter()
            .map(|o| o.gas_estimate())
            .fold(BigUint::ZERO, |acc, g| acc + g);

        let solve_time_ms = start.elapsed().as_millis() as u64;

        Ok(Quote::new(order_quotes, total_gas_estimate, solve_time_ms))
    }

    /// Solves a single order by fanning out to all solver pools.
    async fn solve_order(
        &self,
        order: Order,
        deadline: Instant,
        min_responses: usize,
    ) -> OrderResponses {
        let start_time = Instant::now();
        let order_id = order.id().to_string();

        // Fan-out: send order to all solver pools
        // perf: In the future, we can add new distribution algorithms, like sending short-timeout
        // only to fast workers.
        let mut pending: FuturesUnordered<_> = self
            .solver_pools
            .iter()
            .map(|pool| {
                let order_clone = order.clone();
                let pool_name = pool.name().to_string();
                let queue = pool.queue().clone();

                async move {
                    let result = queue.enqueue(order_clone).await;
                    (pool_name, result)
                }
            })
            .collect();

        let mut quotes = Vec::new();
        let mut failed_solvers: Vec<(String, SolveError)> = Vec::new();
        let mut remaining_pools: HashSet<String> = self
            .solver_pools
            .iter()
            .map(|p| p.name().to_string())
            .collect();

        // Collect responses with timeout
        loop {
            let deadline_instant = tokio::time::Instant::from_std(deadline);

            tokio::select! {
                // Always checks timeout first, ensuring we respect the deadline
                biased;

                // Timeout reached
                _ = tokio::time::sleep_until(deadline_instant) => {
                    // Mark all remaining pools as timed out
                    let elapsed_ms = deadline.saturating_duration_since(Instant::now())
                        .as_millis() as u64;
                    for pool_name in remaining_pools.drain() {
                        failed_solvers.push((
                            pool_name,
                            SolveError::Timeout { elapsed_ms },
                        ));
                    }
                    break;
                }

                // Response received
                result = pending.next() => {
                    match result {
                        Some((pool_name, Ok(single_quote))) => {
                            // Remove from remaining
                            remaining_pools.remove(&pool_name);

                            // Extract the OrderQuote from SingleOrderQuote
                            quotes.push((pool_name.clone(), single_quote.order().clone()));

                            // Early return if min_responses reached
                            if min_responses > 0 && quotes.len() >= min_responses {
                                debug!(
                                    order_id = %order_id,
                                    responses = quotes.len(),
                                    min_responses,
                                    "early return: min_responses reached"
                                );
                                counter!("worker_router_early_returns_total").increment(1);
                                break;
                            }
                        }
                        Some((pool_name, Err(e))) => {
                            remaining_pools.remove(&pool_name);
                            warn!(
                                pool = %pool_name,
                                order_id = %order_id,
                                error = %e,
                                "solver pool failed"
                            );
                            failed_solvers.push((pool_name, e));
                        }
                        None => {
                            // All futures completed
                            break;
                        }
                    }
                }
            }
        }

        // Record metrics
        let duration = start_time.elapsed().as_secs_f64();
        histogram!("worker_router_solve_duration_seconds").record(duration);
        histogram!("worker_router_solver_responses").record(quotes.len() as f64);

        // Record failures by pool and error type
        for (pool_name, error) in &failed_solvers {
            let error_type = match error {
                SolveError::Timeout { .. } => "timeout",
                SolveError::NoRouteFound { .. } => "no_route",
                SolveError::QueueFull => "queue_full",
                SolveError::Internal(_) => "internal",
                _ => "other",
            };
            counter!("worker_router_solver_failures_total", "pool" => pool_name.clone(), "error_type" => error_type).increment(1);
        }

        if !failed_solvers.is_empty() {
            let timeout_count = failed_solvers
                .iter()
                .filter(|(_, e)| matches!(e, SolveError::Timeout { .. }))
                .count();
            let other_count = failed_solvers.len() - timeout_count;
            warn!(
                order_id = %order_id,
                timeout_count,
                other_failures = other_count,
                "some solver pools failed"
            );
        }

        OrderResponses { order_id, quotes, failed_solvers }
    }

    /// Selects the best quote from collected responses.
    ///
    /// Selection criteria:
    /// 1. Filter by constraints (e.g., max_gas)
    /// 2. Select by maximum `amount_out_net_gas`
    fn select_best(&self, responses: &OrderResponses, options: &QuoteOptions) -> OrderQuote {
        let valid_quotes: Vec<_> = responses
            .quotes
            .iter()
            // Only consider successful quotes
            .filter(|(_, q)| q.status() == QuoteStatus::Success)
            // Filter by max_gas constraint if specified
            .filter(|(_, q)| {
                options
                    .max_gas()
                    .map(|max| q.gas_estimate() <= max)
                    .unwrap_or(true)
            })
            .collect();

        // Select by max amount_out_net_gas
        if let Some((pool_name, best)) = valid_quotes
            .into_iter()
            .max_by_key(|(_, q)| q.amount_out_net_gas())
        {
            // Record metrics for successful selection
            counter!("worker_router_orders_total", "status" => "success").increment(1);
            counter!("worker_router_best_quote_pool", "pool" => pool_name.clone()).increment(1);

            debug!(
                order_id = %best.order_id(),
                pool = %pool_name,
                amount_out_net_gas = %best.amount_out_net_gas(),
                "selected best quote"
            );
            return best.clone();
        }

        // No valid quote found - return a NoRouteFound response
        // Try to get any response to extract block info, or create a placeholder
        if let Some((_, any_q)) = responses.quotes.first() {
            counter!("worker_router_orders_total", "status" => "no_route").increment(1);
            OrderQuote::new(
                responses.order_id.clone(),
                QuoteStatus::NoRouteFound,
                any_q.amount_in().clone(),
                BigUint::ZERO,
                BigUint::ZERO,
                BigUint::ZERO,
                any_q.block().clone(),
                String::new(),
                any_q.sender.clone(),
                any_q.receiver.clone(),
            )
        } else {
            // No responses at all - determine status from failure types
            let status = if responses.failed_solvers.is_empty() {
                QuoteStatus::NoRouteFound
            } else {
                // If all failures are timeouts, report as Timeout
                // Otherwise report as NoRouteFound (more general failure)
                let all_timeouts = responses
                    .failed_solvers
                    .iter()
                    .all(|(_, e)| matches!(e, SolveError::Timeout { .. }));
                let all_not_ready = responses
                    .failed_solvers
                    .iter()
                    .all(|(_, e)| matches!(e, SolveError::NotReady(_)));
                if all_timeouts {
                    QuoteStatus::Timeout
                } else if all_not_ready {
                    QuoteStatus::NotReady
                } else {
                    QuoteStatus::NoRouteFound
                }
            };

            // Record status metric
            let status_label = match status {
                QuoteStatus::Timeout => "timeout",
                QuoteStatus::NotReady => "not_ready",
                _ => "no_route",
            };
            counter!("worker_router_orders_total", "status" => status_label).increment(1);

            OrderQuote::new(
                responses.order_id.clone(),
                status,
                BigUint::ZERO,
                BigUint::ZERO,
                BigUint::ZERO,
                BigUint::ZERO,
                BlockInfo::new(0, String::new(), 0),
                String::new(),
                Bytes::default(),
                Bytes::default(),
            )
        }
    }

    /// Returns the effective timeout for a request.
    fn effective_timeout(&self, options: &QuoteOptions) -> Duration {
        options
            .timeout_ms()
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_timeout())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
    use tycho_simulation::{
        tycho_common::models::Chain,
        tycho_core::{
            models::{token::Token, Address, Chain as SimChain},
            Bytes,
        },
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component, MockProtocolSim},
        types::internal::SolveTask,
        EncodingOptions, OrderSide, Route, SingleOrderQuote, Swap,
    };

    fn default_encoder() -> Encoder {
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(None)
            .expect("default encoders should always succeed");
        Encoder::new(Chain::Ethereum, registry).expect("encoder creation should succeed")
    }

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order() -> Order {
        Order::new(
            make_address(0x01),
            make_address(0x02),
            BigUint::from(1000u64),
            OrderSide::Sell,
            make_address(0xAA),
        )
        .with_id("test-order".to_string())
    }

    fn make_single_quote(amount_out_net_gas: u64) -> SingleOrderQuote {
        let make_token = |addr: Address| Token {
            address: addr,
            symbol: "T".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: SimChain::Ethereum,
            quality: 100,
        };
        let tin = make_address(0x01);
        let tout = make_address(0x02);
        let swap = Swap::new(
            "pool-1".to_string(),
            "uniswap_v2".to_string(),
            tin.clone(),
            tout.clone(),
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(50_000u64),
            component(
                "0x0000000000000000000000000000000000000001",
                &[make_token(tin), make_token(tout)],
            ),
            Box::new(MockProtocolSim::default()),
        );
        let quote = OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(100_000u64),
            BigUint::from(amount_out_net_gas),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
        )
        .with_route(Route::new(vec![swap]));
        SingleOrderQuote::new(quote, 5)
    }

    // Helper to create a mock solver pool that responds with a given solution
    fn create_mock_pool(
        name: &str,
        response: Result<SingleOrderQuote, SolveError>,
        delay_ms: u64,
    ) -> (SolverPoolHandle, tokio::task::JoinHandle<()>) {
        let (tx, rx) = async_channel::bounded::<SolveTask>(10);
        let handle = TaskQueueHandle::from_sender(tx);

        let worker = tokio::spawn(async move {
            while let Ok(task) = rx.recv().await {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                task.respond(response.clone());
            }
        });

        (SolverPoolHandle::new(name, handle), worker)
    }

    #[test]
    fn test_config_default() {
        let config = WorkerPoolRouterConfig::default();
        assert_eq!(config.default_timeout(), Duration::from_secs(1));
        assert_eq!(config.min_responses(), 1);
    }

    #[test]
    fn test_config_builder() {
        let config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(500))
            .with_min_responses(2);
        assert_eq!(config.default_timeout(), Duration::from_millis(500));
        assert_eq!(config.min_responses(), 2);
    }

    #[tokio::test]
    async fn test_router_no_pools() {
        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let request = QuoteRequest::new(vec![make_order()], QuoteOptions::default());

        let result = worker_router.quote(request).await;
        assert!(matches!(result, Err(SolveError::Internal(_))));
    }

    #[tokio::test]
    async fn test_router_single_pool_success() {
        let (pool, worker) = create_mock_pool("pool_a", Ok(make_single_quote(900)), 0);

        let worker_router =
            WorkerPoolRouter::new(vec![pool], WorkerPoolRouterConfig::default(), default_encoder());
        let options = QuoteOptions::default().with_encoding_options(EncodingOptions::new(0.01));
        let request = QuoteRequest::new(vec![make_order()], options);

        let result = worker_router.quote(request).await;
        assert!(result.is_ok());

        let quote = result.unwrap();
        assert_eq!(quote.orders().len(), 1);
        assert_eq!(quote.orders()[0].status(), QuoteStatus::Success);
        assert_eq!(*quote.orders()[0].amount_out_net_gas(), BigUint::from(900u64));
        assert!(!quote.orders()[0]
            .transaction()
            .unwrap()
            .data()
            .is_empty());

        drop(worker_router);
        worker.abort();
    }

    #[tokio::test]
    async fn test_router_selects_best_of_two() {
        // Pool A: worse quote (net gas = 800)
        let (pool_a, worker_a) = create_mock_pool("pool_a", Ok(make_single_quote(800)), 0);
        // Pool B: better quote (net gas = 950)
        let (pool_b, worker_b) = create_mock_pool("pool_b", Ok(make_single_quote(950)), 0);

        // Wait for both responses to test best selection logic
        let config = WorkerPoolRouterConfig::default().with_min_responses(2);
        let worker_router = WorkerPoolRouter::new(vec![pool_a, pool_b], config, default_encoder());
        let options = QuoteOptions::default().with_encoding_options(EncodingOptions::new(0.01));
        let request = QuoteRequest::new(vec![make_order()], options);

        let result = worker_router.quote(request).await;
        assert!(result.is_ok());

        let quote = result.unwrap();
        assert_eq!(quote.orders().len(), 1);
        // Should select pool_b's quote (higher amount_out_net_gas)
        assert_eq!(*quote.orders()[0].amount_out_net_gas(), BigUint::from(950u64));
        assert!(!quote.orders()[0]
            .transaction()
            .unwrap()
            .data()
            .is_empty());

        drop(worker_router);
        worker_a.abort();
        worker_b.abort();
    }

    #[tokio::test]
    async fn test_router_timeout() {
        // Pool that takes too long
        let (pool, worker) = create_mock_pool("slow_pool", Ok(make_single_quote(900)), 500);

        let config = WorkerPoolRouterConfig::default().with_timeout(Duration::from_millis(50));
        let worker_router = WorkerPoolRouter::new(vec![pool], config, default_encoder());
        let request = QuoteRequest::new(vec![make_order()], QuoteOptions::default());

        let result = worker_router.quote(request).await;
        assert!(result.is_ok());

        let quote = result.unwrap();
        // Should timeout and return NoRouteFound or Timeout status
        assert_eq!(quote.orders().len(), 1);
        assert!(matches!(
            quote.orders()[0].status(),
            QuoteStatus::Timeout | QuoteStatus::NoRouteFound
        ));

        drop(worker_router);
        worker.abort();
    }

    #[tokio::test]
    async fn test_router_early_return_on_min_responses() {
        // Pool A: fast
        let (pool_a, worker_a) = create_mock_pool("fast_pool", Ok(make_single_quote(800)), 0);
        // Pool B: slow (but we won't wait for it)
        let (pool_b, worker_b) = create_mock_pool("slow_pool", Ok(make_single_quote(950)), 500);

        let config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(1000))
            .with_min_responses(1);
        let worker_router = WorkerPoolRouter::new(vec![pool_a, pool_b], config, default_encoder());

        let start = Instant::now();
        let options = QuoteOptions::default().with_encoding_options(EncodingOptions::new(0.01));
        let request = QuoteRequest::new(vec![make_order()], options);

        let result = worker_router.quote(request).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        // Should return quickly (not waiting for pool_b)
        assert!(elapsed < Duration::from_millis(200));

        // Should have pool_a's quote
        let quote = result.unwrap();
        assert_eq!(quote.orders().len(), 1);
        assert_eq!(quote.orders()[0].status(), QuoteStatus::Success);
        // Should have encoding
        assert!(!quote.orders()[0]
            .transaction()
            .unwrap()
            .data()
            .is_empty());

        drop(worker_router);
        worker_a.abort();
        worker_b.abort();
    }

    #[rstest]
    #[case::under_limit(100, Some(200), true)]
    #[case::at_limit(200, Some(200), true)]
    #[case::over_limit(300, Some(200), false)]
    #[case::no_limit(500, None, true)]
    fn test_max_gas_constraint(
        #[case] gas_estimate: u64,
        #[case] max_gas: Option<u64>,
        #[case] should_pass: bool,
    ) {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![(
                "pool".to_string(),
                OrderQuote::new(
                    "test".to_string(),
                    QuoteStatus::Success,
                    BigUint::from(1000u64),
                    BigUint::from(990u64),
                    BigUint::from(gas_estimate),
                    BigUint::from(900u64),
                    BlockInfo::new(1, "0x123".to_string(), 1000),
                    "test".to_string(),
                    Bytes::from(make_address(0xAA).as_ref()),
                    Bytes::from(make_address(0xAA).as_ref()),
                ),
            )],
            failed_solvers: vec![],
        };

        let options = match max_gas {
            Some(gas) => QuoteOptions::default().with_max_gas(BigUint::from(gas)),
            None => QuoteOptions::default(),
        };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.select_best(&responses, &options);

        if should_pass {
            assert_eq!(result.status(), QuoteStatus::Success);
        } else {
            assert_eq!(result.status(), QuoteStatus::NoRouteFound);
        }
    }

    #[tokio::test]
    async fn test_router_captures_solver_errors() {
        // Pool that returns an error
        let (pool, worker) = create_mock_pool(
            "error_pool",
            Err(SolveError::NoRouteFound { order_id: "test-order".to_string() }),
            0,
        );

        let worker_router =
            WorkerPoolRouter::new(vec![pool], WorkerPoolRouterConfig::default(), default_encoder());
        let request = QuoteRequest::new(vec![make_order()], QuoteOptions::default());

        let result = worker_router.quote(request).await;
        assert!(result.is_ok());

        let quote = result.unwrap();
        assert_eq!(quote.orders().len(), 1);
        // Should be NoRouteFound since the only solver returned an error
        assert_eq!(quote.orders()[0].status(), QuoteStatus::NoRouteFound);

        drop(worker_router);
        worker.abort();
    }

    #[test]
    fn test_select_best_all_timeouts_returns_timeout_status() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![],
            failed_solvers: vec![
                ("pool_a".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
                ("pool_b".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
            ],
        };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.select_best(&responses, &QuoteOptions::default());

        assert_eq!(result.status(), QuoteStatus::Timeout);
    }

    #[test]
    fn test_select_best_mixed_failures_returns_no_route_found() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![],
            failed_solvers: vec![
                ("pool_a".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
                ("pool_b".to_string(), SolveError::NoRouteFound { order_id: "test".to_string() }),
            ],
        };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.select_best(&responses, &QuoteOptions::default());

        // Mixed failures (not all timeouts) should return NoRouteFound
        assert_eq!(result.status(), QuoteStatus::NoRouteFound);
    }

    #[test]
    fn test_select_best_no_failures_returns_no_route_found() {
        let responses =
            OrderResponses { order_id: "test".to_string(), quotes: vec![], failed_solvers: vec![] };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.select_best(&responses, &QuoteOptions::default());

        // No failures but also no quotes means NoRouteFound
        assert_eq!(result.status(), QuoteStatus::NoRouteFound);
    }
}
