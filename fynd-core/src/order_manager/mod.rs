//! Order Manager for orchestrating multiple solver pools.
//!
//! The OrderManager sits between the API layer and multiple solver pools.
//! It fans out each order to all configured solvers, manages timeouts,
//! and selects the best solution based on `amount_out_net_gas`.

//! # Responsibilities
//!
//! 1. **Fan-out**: Distribute each order to solver pools. Its distribution algorithm can be
//!    customized, but initially it's set to relay to all solvers.
//! 2. **Timeout**: Cancel if solver response takes too long
//! 3. **Collection**: Wait for N responses OR timeout per order
//! 4. **Selection**: Choose best solution (max `amount_out_net_gas`)

pub mod config;

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use config::OrderManagerConfig;
use futures::stream::{FuturesUnordered, StreamExt};
use metrics::{counter, histogram};
use num_bigint::BigUint;
use tracing::{debug, warn};

use crate::{
    encoding::encoder::Encoder,
    price_guard::PriceGuard,
    types::{
        solution::{BlockInfo, Order, SolutionOptions, SolutionRequest},
        OrderSolution, Solution, SolutionStatus, SolveError,
    },
    worker_pool::task_queue::TaskQueueHandle,
};

/// Handle to a solver pool for dispatching orders.
#[derive(Clone)]
pub struct SolverPoolHandle {
    /// Human-readable name for this pool (used in logging & metrics).
    pub name: String,
    /// Queue handle for this pool.
    pub queue: TaskQueueHandle,
}

impl SolverPoolHandle {
    /// Creates a new solver pool handle.
    pub fn new(name: impl Into<String>, queue: TaskQueueHandle) -> Self {
        Self { name: name.into(), queue }
    }
}

/// Collected responses for a single order from multiple solvers.
#[derive(Debug)]
pub(crate) struct OrderResponses {
    /// ID of the order these responses correspond to.
    pub order_id: String,
    /// Solutions received from each solver pool (pool_name, solution).
    pub solutions: Vec<(String, OrderSolution)>,
    /// Solver pools that failed with their respective errors (pool_name, error).
    /// This captures all error types: timeouts, no routes, algorithm errors, etc.
    pub failed_solvers: Vec<(String, SolveError)>,
}

/// Orchestrates multiple solver pools to find the best solution.
pub struct OrderManager {
    /// All registered solver pools.
    solver_pools: Vec<SolverPoolHandle>,
    /// Configuration for the order manager.
    config: OrderManagerConfig,
    encoder: Option<Encoder>,
    price_guard: Option<PriceGuard>,
}

impl OrderManager {
    /// Creates a new OrderManager with the given solver pools and config.
    pub fn new(
        solver_pools: Vec<SolverPoolHandle>,
        config: OrderManagerConfig,
        encoder: Option<Encoder>,
        price_guard: Option<PriceGuard>,
    ) -> Self {
        Self { solver_pools, config, encoder, price_guard }
    }

    /// Returns the number of registered solver pools.
    pub fn num_pools(&self) -> usize {
        self.solver_pools.len()
    }

    /// Solves a request by fanning out to all solver pools.
    ///
    /// For each order in the request:
    /// 1. Sends the order to all solver pools in parallel
    /// 2. Waits for responses with timeout
    /// 3. Selects the best solution based on `amount_out_net_gas`
    pub async fn solve(&self, request: SolutionRequest) -> Result<Solution, SolveError> {
        let start = Instant::now();
        let deadline = start + self.effective_timeout(&request.options);
        let min_responses = request
            .options
            .min_responses
            .unwrap_or(self.config.min_responses);

        if self.solver_pools.is_empty() {
            return Err(SolveError::Internal("no solver pools configured".to_string()));
        }

        // Process each order independently in parallel
        let order_futures: Vec<_> = request
            .orders
            .iter()
            .map(|order| self.solve_order(order.clone(), deadline, min_responses))
            .collect();

        let order_responses = futures::future::join_all(order_futures).await;

        // Select best solution for each order
        let mut order_solutions: Vec<OrderSolution> = order_responses
            .into_iter()
            .map(|responses| self.select_best(&responses, &request.options))
            .collect();

        // Calculate totals
        let total_gas_estimate = order_solutions
            .iter()
            .map(|o| &o.gas_estimate)
            .fold(BigUint::ZERO, |acc, g| acc + g);

        let solve_time_ms = start.elapsed().as_millis() as u64;

        // Validate against external prices (if configured)
        if let Some(ref guard) = self.price_guard {
            order_solutions = guard.validate(order_solutions).await?;
        }

        if request.options.include_encoding {
            match &self.encoder {
                Some(encoder) => {
                    // Only encode solutions that are ready for on-chain execution
                    let (to_encode, rest): (Vec<_>, Vec<_>) = order_solutions
                        .into_iter()
                        .partition(|s| s.status == SolutionStatus::Success);
                    let mut encoded = encoder
                        .encode(to_encode, request.options.slippage)
                        .await?;
                    encoded.extend(rest);
                    order_solutions = encoded;
                }
                None => {
                    return Err(SolveError::Internal(
                        "encoding requested but no encoder configured".to_string(),
                    ));
                }
            }
        }

        Ok(Solution { orders: order_solutions, total_gas_estimate, solve_time_ms })
    }

    /// Solves a single order by fanning out to all solver pools.
    async fn solve_order(
        &self,
        order: Order,
        deadline: Instant,
        min_responses: usize,
    ) -> OrderResponses {
        let start_time = Instant::now();
        let order_id = order.id.clone();

        // Fan-out: send order to all solver pools
        // perf: In the future, we can add new distribution algorithms, like sending short-timeout
        // only to fast workers.
        let mut pending: FuturesUnordered<_> = self
            .solver_pools
            .iter()
            .map(|pool| {
                let order_clone = order.clone();
                let pool_name = pool.name.clone();
                let queue = pool.queue.clone();

                async move {
                    let result = queue.enqueue(order_clone).await;
                    (pool_name, result)
                }
            })
            .collect();

        let mut solutions = Vec::new();
        let mut failed_solvers: Vec<(String, SolveError)> = Vec::new();
        let mut remaining_pools: HashSet<String> = self
            .solver_pools
            .iter()
            .map(|p| p.name.clone())
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
                        Some((pool_name, Ok(single_solution))) => {
                            // Remove from remaining
                            remaining_pools.remove(&pool_name);

                            // Extract the OrderSolution from SingleOrderSolution
                            solutions.push((pool_name.clone(), single_solution.order));

                            // Early return if min_responses reached
                            if min_responses > 0 && solutions.len() >= min_responses {
                                debug!(
                                    order_id = %order_id,
                                    responses = solutions.len(),
                                    min_responses,
                                    "early return: min_responses reached"
                                );
                                counter!("order_manager_early_returns_total").increment(1);
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
        histogram!("order_manager_solve_duration_seconds").record(duration);
        histogram!("order_manager_solver_responses").record(solutions.len() as f64);

        // Record failures by pool and error type
        for (pool_name, error) in &failed_solvers {
            let error_type = match error {
                SolveError::Timeout { .. } => "timeout",
                SolveError::NoRouteFound { .. } => "no_route",
                SolveError::QueueFull => "queue_full",
                SolveError::Internal(_) => "internal",
                SolveError::PriceCheckFailed { .. } => "price_check_failed",
                _ => "other",
            };
            counter!("order_manager_solver_failures_total", "pool" => pool_name.clone(), "error_type" => error_type).increment(1);
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

        OrderResponses { order_id, solutions, failed_solvers }
    }

    /// Selects the best solution from collected responses.
    ///
    /// Selection criteria:
    /// 1. Filter by constraints (e.g., max_gas)
    /// 2. Select by maximum `amount_out_net_gas`
    fn select_best(&self, responses: &OrderResponses, options: &SolutionOptions) -> OrderSolution {
        let valid_solutions: Vec<_> = responses
            .solutions
            .iter()
            // Only consider successful solutions
            .filter(|(_, sol)| sol.status == SolutionStatus::Success)
            // Filter by max_gas constraint if specified
            .filter(|(_, sol)| {
                options
                    .max_gas
                    .as_ref()
                    .map(|max| &sol.gas_estimate <= max)
                    .unwrap_or(true)
            })
            .collect();

        // Select by max amount_out_net_gas
        if let Some((pool_name, best)) = valid_solutions
            .into_iter()
            .max_by_key(|(_, sol)| &sol.amount_out_net_gas)
        {
            // Record metrics for successful selection
            counter!("order_manager_orders_total", "status" => "success").increment(1);
            counter!("order_manager_best_solution_pool", "pool" => pool_name.clone()).increment(1);

            debug!(
                order_id = %best.order_id,
                pool = %pool_name,
                amount_out_net_gas = %best.amount_out_net_gas,
                "selected best solution"
            );
            return best.clone();
        }

        // No valid solution found - return a NoRouteFound response
        // Try to get any response to extract block info, or create a placeholder
        if let Some((_, any_sol)) = responses.solutions.first() {
            counter!("order_manager_orders_total", "status" => "no_route").increment(1);
            OrderSolution {
                order_id: responses.order_id.clone(),
                status: SolutionStatus::NoRouteFound,
                route: None,
                amount_in: any_sol.amount_in.clone(),
                amount_out: BigUint::ZERO,
                gas_estimate: BigUint::ZERO,
                price_impact_bps: None,
                amount_out_net_gas: BigUint::ZERO,
                block: any_sol.block.clone(),
                algorithm: String::new(),
                transaction: None,
            }
        } else {
            // No responses at all - determine status from failure types
            let status = if responses.failed_solvers.is_empty() {
                SolutionStatus::NoRouteFound
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
                    SolutionStatus::Timeout
                } else if all_not_ready {
                    SolutionStatus::NotReady
                } else {
                    SolutionStatus::NoRouteFound
                }
            };

            // Record status metric
            let status_label = match status {
                SolutionStatus::Timeout => "timeout",
                SolutionStatus::NotReady => "not_ready",
                _ => "no_route",
            };
            counter!("order_manager_orders_total", "status" => status_label).increment(1);

            OrderSolution {
                order_id: responses.order_id.clone(),
                status,
                route: None,
                amount_in: BigUint::ZERO,
                amount_out: BigUint::ZERO,
                gas_estimate: BigUint::ZERO,
                price_impact_bps: None,
                amount_out_net_gas: BigUint::ZERO,
                block: BlockInfo { number: 0, hash: String::new(), timestamp: 0 },
                algorithm: String::new(),
                transaction: None,
            }
        }
    }

    /// Returns the effective timeout for a request.
    fn effective_timeout(&self, options: &SolutionOptions) -> Duration {
        options
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_timeout)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_simulation::tycho_core::models::Address;

    use super::*;
    use crate::{types::internal::SolveTask, OrderSide, SingleOrderSolution};

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order() -> Order {
        Order {
            id: "test-order".to_string(),
            token_in: make_address(0x01),
            token_out: make_address(0x02),
            amount: BigUint::from(1000u64),
            side: OrderSide::Sell,
            sender: make_address(0xAA),
            receiver: None,
        }
    }

    fn make_single_solution(amount_out_net_gas: u64) -> SingleOrderSolution {
        SingleOrderSolution {
            order: OrderSolution {
                order_id: "test-order".to_string(),
                status: SolutionStatus::Success,
                route: None,
                amount_in: BigUint::from(1000u64),
                amount_out: BigUint::from(990u64),
                gas_estimate: BigUint::from(100_000u64),
                price_impact_bps: None,
                amount_out_net_gas: BigUint::from(amount_out_net_gas),
                block: BlockInfo { number: 1, hash: "0x123".to_string(), timestamp: 1000 },
                algorithm: "test".to_string(),
                transaction: None,
            },
            solve_time_ms: 5,
        }
    }

    // Helper to create a mock solver pool that responds with a given solution
    fn create_mock_pool(
        name: &str,
        response: Result<SingleOrderSolution, SolveError>,
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
        let config = OrderManagerConfig::default();
        assert_eq!(config.default_timeout, Duration::from_secs(1));
        assert_eq!(config.min_responses, 1);
    }

    #[test]
    fn test_config_builder() {
        let config = OrderManagerConfig::default()
            .with_timeout(Duration::from_millis(500))
            .with_min_responses(2);
        assert_eq!(config.default_timeout, Duration::from_millis(500));
        assert_eq!(config.min_responses, 2);
    }

    #[tokio::test]
    async fn test_order_manager_no_pools() {
        let manager = OrderManager::new(vec![], OrderManagerConfig::default(), None, None);
        let request =
            SolutionRequest { orders: vec![make_order()], options: SolutionOptions::default() };

        let result = manager.solve(request).await;
        assert!(matches!(result, Err(SolveError::Internal(_))));
    }

    #[tokio::test]
    async fn test_order_manager_single_pool_success() {
        let (pool, worker) = create_mock_pool("pool_a", Ok(make_single_solution(900)), 0);

        let manager = OrderManager::new(vec![pool], OrderManagerConfig::default(), None, None);
        let request =
            SolutionRequest { orders: vec![make_order()], options: SolutionOptions::default() };

        let result = manager.solve(request).await;
        assert!(result.is_ok());

        let solution = result.unwrap();
        assert_eq!(solution.orders.len(), 1);
        assert_eq!(solution.orders[0].status, SolutionStatus::Success);
        assert_eq!(solution.orders[0].amount_out_net_gas, BigUint::from(900u64));

        drop(manager);
        worker.abort();
    }

    #[tokio::test]
    async fn test_order_manager_selects_best_of_two() {
        // Pool A: worse solution (net gas = 800)
        let (pool_a, worker_a) = create_mock_pool("pool_a", Ok(make_single_solution(800)), 0);
        // Pool B: better solution (net gas = 950)
        let (pool_b, worker_b) = create_mock_pool("pool_b", Ok(make_single_solution(950)), 0);

        // Wait for both responses to test best selection logic
        let config = OrderManagerConfig::default().with_min_responses(2);
        let manager = OrderManager::new(vec![pool_a, pool_b], config, None, None);
        let request =
            SolutionRequest { orders: vec![make_order()], options: SolutionOptions::default() };

        let result = manager.solve(request).await;
        assert!(result.is_ok());

        let solution = result.unwrap();
        assert_eq!(solution.orders.len(), 1);
        // Should select pool_b's solution (higher amount_out_net_gas)
        assert_eq!(solution.orders[0].amount_out_net_gas, BigUint::from(950u64));

        drop(manager);
        worker_a.abort();
        worker_b.abort();
    }

    #[tokio::test]
    async fn test_order_manager_timeout() {
        // Pool that takes too long
        let (pool, worker) = create_mock_pool("slow_pool", Ok(make_single_solution(900)), 500);

        let config = OrderManagerConfig::default().with_timeout(Duration::from_millis(50));
        let manager = OrderManager::new(vec![pool], config, None, None);
        let request =
            SolutionRequest { orders: vec![make_order()], options: SolutionOptions::default() };

        let result = manager.solve(request).await;
        assert!(result.is_ok());

        let solution = result.unwrap();
        // Should timeout and return NoRouteFound or Timeout status
        assert_eq!(solution.orders.len(), 1);
        assert!(matches!(
            solution.orders[0].status,
            SolutionStatus::Timeout | SolutionStatus::NoRouteFound
        ));

        drop(manager);
        worker.abort();
    }

    #[tokio::test]
    async fn test_order_manager_early_return_on_min_responses() {
        // Pool A: fast
        let (pool_a, worker_a) = create_mock_pool("fast_pool", Ok(make_single_solution(800)), 0);
        // Pool B: slow (but we won't wait for it)
        let (pool_b, worker_b) = create_mock_pool("slow_pool", Ok(make_single_solution(950)), 500);

        let config = OrderManagerConfig::default()
            .with_timeout(Duration::from_millis(1000))
            .with_min_responses(1);
        let manager = OrderManager::new(vec![pool_a, pool_b], config, None, None);

        let start = Instant::now();
        let request =
            SolutionRequest { orders: vec![make_order()], options: SolutionOptions::default() };

        let result = manager.solve(request).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        // Should return quickly (not waiting for pool_b)
        assert!(elapsed < Duration::from_millis(200));

        // Should have pool_a's solution
        let solution = result.unwrap();
        assert_eq!(solution.orders.len(), 1);
        assert_eq!(solution.orders[0].status, SolutionStatus::Success);

        drop(manager);
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
            solutions: vec![(
                "pool".to_string(),
                OrderSolution {
                    order_id: "test".to_string(),
                    status: SolutionStatus::Success,
                    route: None,
                    amount_in: BigUint::from(1000u64),
                    amount_out: BigUint::from(990u64),
                    gas_estimate: BigUint::from(gas_estimate),
                    price_impact_bps: None,
                    amount_out_net_gas: BigUint::from(900u64),
                    block: BlockInfo { number: 1, hash: "0x123".to_string(), timestamp: 1000 },
                    algorithm: "test".to_string(),
                    encoded_solution: None,
                },
            )],
            failed_solvers: vec![],
        };

        let options = SolutionOptions {
            timeout_ms: None,
            min_responses: None,
            max_gas: max_gas.map(BigUint::from),
            include_encoding: false,
        };

        let manager = OrderManager::new(vec![], OrderManagerConfig::default(), None, None);
        let result = manager.select_best(&responses, &options);

        if should_pass {
            assert_eq!(result.status, SolutionStatus::Success);
        } else {
            assert_eq!(result.status, SolutionStatus::NoRouteFound);
        }
    }

    #[tokio::test]
    async fn test_order_manager_captures_solver_errors() {
        // Pool that returns an error
        let (pool, worker) = create_mock_pool(
            "error_pool",
            Err(SolveError::NoRouteFound { order_id: "test-order".to_string() }),
            0,
        );

        let manager = OrderManager::new(vec![pool], OrderManagerConfig::default(), None, None);
        let request =
            SolutionRequest { orders: vec![make_order()], options: SolutionOptions::default() };

        let result = manager.solve(request).await;
        assert!(result.is_ok());

        let solution = result.unwrap();
        assert_eq!(solution.orders.len(), 1);
        // Should be NoRouteFound since the only solver returned an error
        assert_eq!(solution.orders[0].status, SolutionStatus::NoRouteFound);

        drop(manager);
        worker.abort();
    }

    #[test]
    fn test_select_best_all_timeouts_returns_timeout_status() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            solutions: vec![],
            failed_solvers: vec![
                ("pool_a".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
                ("pool_b".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
            ],
        };

        let manager = OrderManager::new(vec![], OrderManagerConfig::default(), None, None);
        let result = manager.select_best(&responses, &SolutionOptions::default());

        assert_eq!(result.status, SolutionStatus::Timeout);
    }

    #[test]
    fn test_select_best_mixed_failures_returns_no_route_found() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            solutions: vec![],
            failed_solvers: vec![
                ("pool_a".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
                ("pool_b".to_string(), SolveError::NoRouteFound { order_id: "test".to_string() }),
            ],
        };

        let manager = OrderManager::new(vec![], OrderManagerConfig::default(), None, None);
        let result = manager.select_best(&responses, &SolutionOptions::default());

        // Mixed failures (not all timeouts) should return NoRouteFound
        assert_eq!(result.status, SolutionStatus::NoRouteFound);
    }

    #[test]
    fn test_select_best_no_failures_returns_no_route_found() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            solutions: vec![],
            failed_solvers: vec![],
        };

        let manager = OrderManager::new(vec![], OrderManagerConfig::default(), None, None);
        let result = manager.select_best(&responses, &SolutionOptions::default());

        // No failures but also no solutions means NoRouteFound
        assert_eq!(result.status, SolutionStatus::NoRouteFound);
    }
}
