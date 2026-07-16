//! Orchestrates multiple solver pools to find the best quote per request.
//!
//! The WorkerPoolRouter sits between the API layer and multiple solver pools.
//! It fans out each order to all configured solvers, manages timeouts,
//! selects the best quote based on `amount_out_net_gas`, and optionally
//! encodes the winning solution into an on-chain transaction.

//! # Responsibilities
//!
//! 1. **Fan-out**: Distribute each order to solver pools. Its distribution algorithm can be
//!    customized, but initially it's set to relay to all solvers.
//! 2. **Timeout**: Cancel if solver response takes too long
//! 3. **Collection**: Wait for N responses OR timeout per order
//! 4. **Gas refinement**: Before cross-pool ranking, replace each candidate's naive
//!    `route.total_gas()` estimate (used internally by algorithms for intra-pool ranking) with the
//!    more accurate `estimate_gas_usage` from tycho-execution, which accounts for token transfer
//!    costs and router overhead. The `amount_out_net_gas` values are rescaled proportionally so the
//!    final ranking reflects realistic execution cost.
//! 5. **Selection**: Choose best quote (max refined `amount_out_net_gas`)
//! 6. **Encoding**: If [`EncodingOptions`](crate::EncodingOptions) are provided in the request,
//!    encode winning solutions into executable on-chain transactions via the
//!    [`encoding::encoder::Encoder`](crate::encoding::encoder::Encoder)

pub mod config;

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use config::WorkerPoolRouterConfig;
use futures::stream::{FuturesUnordered, StreamExt};
use metrics::{counter, histogram};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use tycho_execution::encoding::{
    evm::gas_estimator::estimate_gas_usage,
    models::{Solution, Strategy},
};
use tycho_simulation::tycho_common::Bytes;

use crate::{
    encoding::encoder::Encoder, feed::permission::PermissionPolicy, price_guard::guard::PriceGuard,
    worker_pool::task_queue::TaskQueueHandle, BlockInfo, EncodingOptions, Order, OrderQuote, Quote,
    QuoteOptions, QuoteRequest, QuoteStatus, SolveError, SolveParams, SurplusInfo,
};

/// The role a solver pool (a group of workers) plays in a quote.
///
/// A `Public` worker routes only through public liquidity and provides the committed (quoted)
/// reference output. An `All` worker also routes through exclusive components and may beat that
/// reference, in which case the protocol captures the surplus.
///
/// Serialized in lowercase (`"public"` / `"all"`) in `worker_pools.toml` via [`PoolConfig`].
///
/// [`PoolConfig`]: crate::PoolConfig
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolRole {
    /// Routes through public liquidity only. Establishes the committed reference output. Default.
    #[default]
    Public,
    /// Routes through all liquidity, including exclusive components; source of surplus quotes.
    All,
}

/// Handle to a solver pool for dispatching orders.
#[derive(Clone)]
pub struct SolverPoolHandle {
    /// Human-readable name for this pool (used in logging & metrics).
    name: String,
    /// Queue handle for this pool.
    queue: TaskQueueHandle,
    /// Whether this pool routes public-only or all liquidity.
    role: PoolRole,
}

impl SolverPoolHandle {
    /// Creates a new solver pool handle with the default [`PoolRole::Public`] role.
    pub fn new(name: impl Into<String>, queue: TaskQueueHandle) -> Self {
        Self { name: name.into(), queue, role: PoolRole::Public }
    }

    /// Sets the pool's role (e.g. [`PoolRole::All`] for the pool that includes exclusive liquidity).
    pub fn with_role(mut self, role: PoolRole) -> Self {
        self.role = role;
        self
    }

    /// Returns the pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the task queue handle.
    pub fn queue(&self) -> &TaskQueueHandle {
        &self.queue
    }

    /// Returns the pool's role.
    pub fn role(&self) -> PoolRole {
        self.role
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

impl OrderResponses {
    /// Returns a copy keeping only candidates from public-role pools.
    ///
    /// These form the committed reference and the ranked fallback chain (ranked by `rank_quotes`,
    /// consumed by the price guard); surplus-pool candidates are overlaid separately by
    /// `combine_with_surplus`. `failed_solvers` is retained so placeholder construction is
    /// unchanged.
    fn public_only(&self, pool_roles: &HashMap<String, PoolRole>) -> OrderResponses {
        let quotes = self
            .quotes
            .iter()
            .filter(|(pool, _)| pool_roles.get(pool) != Some(&PoolRole::All))
            .cloned()
            .collect();
        OrderResponses {
            order_id: self.order_id.clone(),
            quotes,
            failed_solvers: self.failed_solvers.clone(),
        }
    }
}

/// Orchestrates multiple solver pools to find the best quote.
pub struct WorkerPoolRouter {
    /// All registered solver pools.
    solver_pools: Vec<SolverPoolHandle>,
    /// Configuration for the worker router.
    config: WorkerPoolRouterConfig,
    /// Encoder for encoding solutions into on-chain transactions.
    encoder: Encoder,
    /// Validates solution outputs against external price sources.
    /// Present when the server has price guard enabled; `None` when disabled.
    price_guard: Option<PriceGuard>,
    /// Predicate identifying exclusive components, used by `combine_with_surplus` to locate the
    /// exclusive leg(s) in a surplus route. `None` when no exclusive pools are configured.
    permission_policy: Option<PermissionPolicy>,
}

impl WorkerPoolRouter {
    /// Creates a new WorkerPoolRouter with the given solver pools, config, and encoder.
    pub fn new(
        solver_pools: Vec<SolverPoolHandle>,
        config: WorkerPoolRouterConfig,
        encoder: Encoder,
    ) -> Self {
        Self { solver_pools, config, encoder, price_guard: None, permission_policy: None }
    }

    /// Attaches the permission policy used to identify exclusive legs in surplus routes.
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = Some(policy);
        self
    }

    /// Makes price guard validation available for this router.
    ///
    /// Providers are started and caches stay warm. Validation only runs for
    /// requests where the client sets `enabled: true` in `PriceGuardConfig`.
    pub fn with_price_guard(mut self, price_guard: PriceGuard) -> Self {
        self.price_guard = Some(price_guard);
        self
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
    /// 4. If `encoding_options` are set on the request, encodes winning solutions into on-chain
    ///    transactions
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

        let params = match request.options().state_label().cloned() {
            Some(label) => SolveParams::default().with_state_label(label),
            None => SolveParams::default(),
        };

        // Process each order independently in parallel
        let order_futures: Vec<_> = request
            .orders()
            .iter()
            .map(|order| self.solve_order(order.clone(), params.clone(), deadline, min_responses))
            .collect();

        let mut order_responses = futures::future::join_all(order_futures).await;

        // Refine gas estimates for all candidates using estimate_gas_usage before ranking,
        // so ranking uses accurate gas costs rather than naive route.total_gas().
        if let Some(encoding_options) = request.options().encoding_options() {
            refine_gas_estimates(&mut order_responses, encoding_options)?;
        }

        // Map each pool name to its role so candidate quotes can be split into public vs surplus.
        let pool_roles: HashMap<String, PoolRole> = self
            .solver_pools
            .iter()
            .map(|p| (p.name().to_string(), p.role()))
            .collect();
        let has_surplus_pool = pool_roles
            .values()
            .any(|r| *r == PoolRole::All);

        // Rank quotes for each order (sorted by refined amount_out_net_gas descending).
        // `rank_quotes` produces the public ranking — the committed reference AND the price-guard
        // fallback chain. When an `All`-role pool is configured, the surplus winner is overlaid
        // onto that ranked list (prepended) by `combine_with_surplus`, so the fallbacks are
        // preserved.
        let ranked_quotes: Vec<Vec<OrderQuote>> = order_responses
            .into_iter()
            .map(|responses| {
                if has_surplus_pool {
                    let public_ranked =
                        self.rank_quotes(&responses.public_only(&pool_roles), request.options());
                    combine_with_surplus(
                        &responses,
                        &pool_roles,
                        request.options(),
                        public_ranked,
                        self.permission_policy.as_ref(),
                    )
                } else {
                    self.rank_quotes(&responses, request.options())
                }
            })
            .collect();

        // Validate against external prices when the client explicitly enables it.
        let price_guard_config = request
            .options()
            .encoding_options()
            .map(|e| e.price_guard())
            .filter(|c| c.enabled());

        let mut order_quotes: Vec<OrderQuote> = match (&self.price_guard, price_guard_config) {
            (Some(guard), Some(config)) => guard
                .validate(ranked_quotes, config)
                .map_err(|e| {
                    warn!(error = %e, "price guard validation error");
                    SolveError::Internal(e.to_string())
                })?,
            (None, Some(_)) => {
                return Err(SolveError::Internal(
                    "price guard config provided but price guard is not enabled on this server"
                        .to_string(),
                ));
            }
            _ => ranked_quotes
                .into_iter()
                .filter_map(|candidates| candidates.into_iter().next())
                .collect(),
        };

        // Encode solutions if encoding_options is set
        if let Some(encoding_options) = request.options().encoding_options() {
            let encode_start = Instant::now();
            let encoded = self
                .encoder
                .encode(order_quotes, encoding_options.clone())
                .await;
            histogram!("encoding_duration_seconds").record(encode_start.elapsed().as_secs_f64());
            order_quotes = match encoded {
                Ok(quotes) => quotes,
                Err(e) => {
                    counter!("encoding_failures_total").increment(1);
                    return Err(e);
                }
            };
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
        params: SolveParams,
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
                let task_params = params.clone();

                async move {
                    let result = queue
                        .enqueue(order_clone, task_params)
                        .await;
                    (pool_name, result)
                }
            })
            .collect();

        // Pre-compute which pool names have role All, for role-aware early return gating.
        let surplus_pool_names: HashSet<String> = self
            .solver_pools
            .iter()
            .filter(|p| p.role() == PoolRole::All)
            .map(|p| p.name().to_string())
            .collect();
        let has_surplus_pool = !surplus_pool_names.is_empty();

        let mut quotes = Vec::new();
        let mut failed_solvers: Vec<(String, SolveError)> = Vec::new();
        let mut remaining_pools: HashSet<String> = self
            .solver_pools
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        let mut has_public_response = false;
        let mut has_surplus_response = false;

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

                            if surplus_pool_names.contains(&pool_name) {
                                has_surplus_response = true;
                            } else {
                                has_public_response = true;
                            }

                            // Extract the OrderQuote from SingleOrderQuote
                            quotes.push((pool_name.clone(), single_quote.order().clone()));

                            // Role-aware early return: when a surplus pool is configured, only
                            // fire once we have ≥1 public AND the surplus pool (so the surplus
                            // overlay has both inputs). Without a surplus pool, use pure
                            // count-based gating (original behaviour).
                            let role_ready = if has_surplus_pool {
                                has_public_response && has_surplus_response
                            } else {
                                true
                            };
                            if min_responses > 0
                                && quotes.len() >= min_responses
                                && role_ready
                            {
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
                            // A failed surplus pool still counts as "responded" for gating — we
                            // know it won't produce a surplus quote, so the public pool can
                            // early-return without waiting for a result that will never come.
                            if surplus_pool_names.contains(&pool_name) {
                                has_surplus_response = true;
                            }
                            debug!(
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
                SolveError::PriceCheckFailed { .. } => "price_check_failed",
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

    /// Returns all valid quotes for an order, ranked by `amount_out_net_gas` descending.
    ///
    /// If no valid quotes exist, returns a single-element vec with a placeholder
    /// (`NoRouteFound` or `Timeout`) so that downstream always has at least one
    /// candidate per order.
    fn rank_quotes(&self, responses: &OrderResponses, options: &QuoteOptions) -> Vec<OrderQuote> {
        let mut valid_quotes: Vec<_> = responses
            .quotes
            .iter()
            .filter(|(_, q)| q.status() == QuoteStatus::Success)
            .filter(|(_, q)| {
                options
                    .max_gas()
                    .map(|max| q.gas_estimate() <= max)
                    .unwrap_or(true)
            })
            .collect();

        // Sort descending by amount_out_net_gas
        valid_quotes.sort_by(|(_, a), (_, b)| {
            b.amount_out_net_gas()
                .cmp(a.amount_out_net_gas())
        });

        if !valid_quotes.is_empty() {
            counter!("worker_router_orders_total", "status" => "success").increment(1);
            let (pool_name, best) = valid_quotes[0];
            counter!("worker_router_best_quote_pool", "pool" => pool_name.clone()).increment(1);
            debug!(
                order_id = %best.order_id(),
                number_of_candidates = valid_quotes.len(),
                "ranked quotes"
            );
            return valid_quotes
                .into_iter()
                .map(|(_, q)| q.clone())
                .collect();
        }

        // No valid quote found - return a NoRouteFound response
        // Try to get any response to extract block info, or create a placeholder
        let mut fallback = if let Some((_, any_q)) = responses.quotes.first() {
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
                any_q.sender().clone(),
                any_q.receiver().clone(),
                any_q.solved_against().clone(),
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

            // No worker responded — use the requested label if set, otherwise "0"
            // (we have no block context here since no worker completed).
            let label = options
                .state_label()
                .cloned()
                .unwrap_or_else(|| "0".to_string());
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
                label,
            )
        };
        if fallback.status() == QuoteStatus::NoRouteFound {
            fallback.set_no_route_reason(aggregate_no_route_reason(&responses.failed_solvers));
        }
        vec![fallback]
    }

    /// Returns the effective timeout for a request.
    fn effective_timeout(&self, options: &QuoteOptions) -> Duration {
        options
            .timeout_ms()
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_timeout())
    }
}

/// Picks the most informative no-route reason across the failed pools.
///
/// Reasons are ranked into tiers so the result does not depend on pool
/// completion order: token-not-in-graph reasons are shared-graph facts and
/// rank highest, then `AmountTooSmall`, `NoScorablePaths`, `NoGraphPath`.
fn aggregate_no_route_reason(
    failed_solvers: &[(String, SolveError)],
) -> Option<crate::algorithm::NoPathReason> {
    let mut best = None;
    for (_, error) in failed_solvers {
        if let SolveError::NoRouteFound { reason: Some(reason), .. } = error {
            if best.is_none_or(|b| reason_tier(b) < reason_tier(*reason)) {
                best = Some(*reason);
            }
        }
    }
    best
}

fn reason_tier(reason: crate::algorithm::NoPathReason) -> u8 {
    use crate::algorithm::NoPathReason;
    match reason {
        NoPathReason::SourceTokenNotInGraph | NoPathReason::DestinationTokenNotInGraph => 3,
        NoPathReason::AmountTooSmall => 2,
        NoPathReason::NoScorablePaths => 1,
        NoPathReason::NoGraphPath => 0,
    }
}

/// Overlays the surplus winner onto the ranked public fallback list for one order.
///
/// `public_ranked` is the public-only ranking from `rank_quotes` — both the committed reference and
/// the price-guard fallback chain. If the best surplus candidate beats the committed reference
/// net-of-gas, the executed surplus quote is returned at the head of the list (its `amount_out`
/// pinned to the committed reference, an order-level `SurplusInfo` attached, and each exclusive
/// leg's `Swap::committed_amount_out` set), preserving the public candidates as fallbacks.
/// Otherwise `public_ranked` is returned unchanged, so the user is never quoted worse than the
/// public market.
///
/// Each exclusive leg's `committed_amount_out` is its realized output reduced by the same
/// proportion as the order-level reduction; the protocol captures the difference. The per-leg
/// attribution formula and the bound that keeps the user at or above the committed reference are
/// derived in the design plan.
fn combine_with_surplus(
    responses: &OrderResponses,
    pool_roles: &HashMap<String, PoolRole>,
    options: &QuoteOptions,
    public_ranked: Vec<OrderQuote>,
    permission_policy: Option<&PermissionPolicy>,
) -> Vec<OrderQuote> {
    let Some(policy) = permission_policy else {
        return public_ranked;
    };

    let committed = match public_ranked.first() {
        Some(q) if q.status() == QuoteStatus::Success => q,
        _ => return public_ranked,
    };

    // Find best surplus candidate from All-role pools, respecting max_gas and layout constraints.
    let best_surplus = responses
        .quotes
        .iter()
        .filter(|(pool, _)| pool_roles.get(pool) == Some(&PoolRole::All))
        .filter(|(_, q)| q.status() == QuoteStatus::Success)
        .filter(|(_, q)| {
            options
                .max_gas()
                .map(|max| q.gas_estimate() <= max)
                .unwrap_or(true)
        })
        .filter(|(_, q)| has_valid_exclusive_layout(q, policy))
        .max_by(|(_, a), (_, b)| {
            a.amount_out_net_gas()
                .cmp(b.amount_out_net_gas())
        })
        .map(|(_, q)| q);

    let Some(surplus_quote) = best_surplus else {
        return public_ranked;
    };

    // The surplus route must beat the committed reference net-of-gas.
    if surplus_quote.amount_out_net_gas() <= committed.amount_out_net_gas() {
        return public_ranked;
    }

    let realized_route_out = surplus_quote.amount_out();
    let committed_route_out = committed.amount_out();

    if realized_route_out <= committed_route_out {
        return public_ranked;
    }

    let surplus_amount = realized_route_out - committed_route_out;

    // Build the executed quote: clone the surplus candidate, stamp per-leg committed amounts,
    // pin amount_out to committed, attach SurplusInfo.
    let mut executed = surplus_quote.clone();

    if let Some(route) = executed.route_mut() {
        for swap in route.swaps_mut() {
            if policy.is_exclusive(swap.protocol_component()) {
                // Proportional reduction: committed_leg = leg.amount_out * committed / realized
                // (rounds down — safe direction: under-capture)
                let committed_leg = swap.amount_out() * committed_route_out / realized_route_out;
                swap.set_committed_amount_out(committed_leg);
            }
        }
    }

    executed.set_amount_out(committed_route_out.clone());

    // Recompute amount_out_net_gas relative to committed output.
    let gas_cost = if *surplus_quote.amount_out() >= *surplus_quote.amount_out_net_gas() {
        surplus_quote.amount_out() - surplus_quote.amount_out_net_gas()
    } else {
        BigUint::ZERO
    };
    let committed_net_gas = if *committed_route_out >= gas_cost {
        committed_route_out - &gas_cost
    } else {
        BigUint::ZERO
    };
    executed.set_amount_out_net_gas(committed_net_gas);

    let surplus_info = SurplusInfo::new(surplus_amount, committed_route_out.clone());
    executed = executed.with_surplus(surplus_info);

    debug_assert!(
        executed.amount_out() >= committed.amount_out(),
        "user output ({}) must be >= committed reference ({})",
        executed.amount_out(),
        committed.amount_out(),
    );

    let mut result = Vec::with_capacity(public_ranked.len() + 1);
    result.push(executed);
    result.extend(public_ranked);
    result
}

/// Returns `true` if the quote has a valid exclusive layout for v1: at least one exclusive
/// leg, all positioned as terminal legs of their respective paths (no mid-route exclusive legs).
///
/// Path boundaries are detected by checking whether the next swap's `token_in` differs from the
/// current swap's `token_out`. This heuristic is correct for Fynd's sequential route
/// representation but would need revisiting if routes gain explicit path-boundary markers.
fn has_valid_exclusive_layout(quote: &OrderQuote, policy: &PermissionPolicy) -> bool {
    let Some(route) = quote.route() else {
        return false;
    };

    let swaps = route.swaps();
    if swaps.is_empty() {
        return false;
    }

    let mut exclusive_count = 0;

    for (i, swap) in swaps.iter().enumerate() {
        if !policy.is_exclusive(swap.protocol_component()) {
            continue;
        }

        // A swap is terminal if it's the last swap or the next swap starts a new path
        // (its token_in doesn't match this swap's token_out).
        let is_terminal = i == swaps.len() - 1 || swaps[i + 1].token_in() != swap.token_out();

        if !is_terminal {
            return false;
        }
        exclusive_count += 1;
    }

    exclusive_count >= 1
}

fn refine_gas_estimates(
    order_responses: &mut Vec<OrderResponses>,
    encoding_options: &EncodingOptions,
) -> Result<(), SolveError> {
    for responses in order_responses {
        for (_, quote) in &mut responses.quotes {
            if quote.status() != QuoteStatus::Success {
                continue;
            }
            let solution = Solution::try_from(&*quote)?
                .with_user_transfer_type(encoding_options.transfer_type().clone());
            let refined_gas = estimate_gas_usage(&solution, derive_strategy(quote));
            let naive_gas = quote.gas_estimate().clone();
            if naive_gas > BigUint::ZERO {
                let gas_cost_in_token_out = quote.amount_out() - quote.amount_out_net_gas();
                let new_gas_cost = &gas_cost_in_token_out * &refined_gas / &naive_gas;
                let new_net = if new_gas_cost <= *quote.amount_out() {
                    quote.amount_out() - &new_gas_cost
                } else {
                    BigUint::ZERO
                };
                quote.set_amount_out_net_gas(new_net);
                quote.set_gas_estimate(refined_gas);
            }
        }
    }
    Ok(())
}

fn derive_strategy(quote: &OrderQuote) -> Strategy {
    let Some(route) = quote.route() else { return Strategy::Single };
    let swaps = route.swaps();
    if swaps.len() == 1 {
        Strategy::Single
    } else if swaps.iter().any(|s| *s.split() > 0.0) {
        Strategy::Split
    } else {
        Strategy::Sequential
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
        let encoder =
            Encoder::new(Chain::Ethereum, registry).expect("encoder creation should succeed");
        // Load fees so encoding can run; the fetcher supplies on-chain values in production.
        encoder
            .router_fees()
            .set(crate::encoding::router_fees::RouterFees::new(
                100_000_000,
                100_000,
                20_000_000,
                std::collections::HashMap::new(),
            ));
        encoder
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
        let tin_token = make_token(tin.clone());
        let tout_token = make_token(tout.clone());
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
                &[tin_token.clone(), tout_token.clone()],
            ),
            Box::new(MockProtocolSim::default()),
        );
        let mut tokens = HashMap::new();
        tokens.insert(tin, tin_token);
        tokens.insert(tout, tout_token);
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
            "1".to_string(),
        )
        .with_route(Route::new(vec![swap], tokens).expect("non-empty route"));
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
        // amount_out_net_gas is refined using estimate_gas_usage before ranking
        assert_eq!(*quote.orders()[0].amount_out_net_gas(), BigUint::from(873u64));
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
        // Pool B wins (higher refined amount_out_net_gas after estimate_gas_usage)
        assert_eq!(*quote.orders()[0].amount_out_net_gas(), BigUint::from(938u64));
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
                    "1".to_string(),
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
        let result = worker_router.rank_quotes(&responses, &options);

        if should_pass {
            assert_eq!(result[0].status(), QuoteStatus::Success);
        } else {
            assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
        }
    }

    #[tokio::test]
    async fn test_router_captures_solver_errors() {
        // Pool that returns an error
        let (pool, worker) =
            create_mock_pool("error_pool", Err(SolveError::no_route_found("test-order")), 0);

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
    fn test_rank_quotes_all_timeouts_returns_timeout_status() {
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
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status(), QuoteStatus::Timeout);
    }

    #[test]
    fn test_rank_quotes_mixed_failures_returns_no_route_found() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![],
            failed_solvers: vec![
                ("pool_a".to_string(), SolveError::Timeout { elapsed_ms: 100 }),
                ("pool_b".to_string(), SolveError::no_route_found("test")),
            ],
        };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
    }

    #[test]
    fn test_rank_quotes_no_failures_returns_no_route_found() {
        let responses =
            OrderResponses { order_id: "test".to_string(), quotes: vec![], failed_solvers: vec![] };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
    }

    #[test]
    fn rank_quotes_attaches_no_route_reason() {
        use crate::algorithm::NoPathReason;
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![],
            failed_solvers: vec![
                (
                    "pool_a".to_string(),
                    SolveError::no_route_found_with_reason("test", NoPathReason::NoGraphPath),
                ),
                (
                    "pool_b".to_string(),
                    SolveError::no_route_found_with_reason(
                        "test",
                        NoPathReason::DestinationTokenNotInGraph,
                    ),
                ),
            ],
        };
        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
        // Token-not-in-graph wins over no_graph_path regardless of pool order.
        assert_eq!(result[0].no_route_reason(), Some(NoPathReason::DestinationTokenNotInGraph));
    }

    #[test]
    fn test_rank_quotes_no_route_reason_single_failure() {
        use crate::algorithm::NoPathReason;
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![],
            failed_solvers: vec![(
                "pool_a".to_string(),
                SolveError::no_route_found_with_reason("test", NoPathReason::NoGraphPath),
            )],
        };
        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());
        assert_eq!(result[0].no_route_reason(), Some(NoPathReason::NoGraphPath));
    }

    #[test]
    fn aggregate_no_route_reason_surfaces_amount_too_small() {
        use crate::algorithm::NoPathReason;
        let failed = vec![(
            "bf".to_string(),
            SolveError::no_route_found_with_reason("o1", NoPathReason::AmountTooSmall),
        )];
        assert_eq!(aggregate_no_route_reason(&failed), Some(NoPathReason::AmountTooSmall));
    }

    #[test]
    fn test_aggregate_no_route_reason_independent_of_pool_order() {
        use crate::algorithm::NoPathReason;
        let dust = || SolveError::no_route_found_with_reason("o1", NoPathReason::AmountTooSmall);
        let no_path = || SolveError::no_route_found_with_reason("o1", NoPathReason::NoGraphPath);
        let forward = vec![("bf1".to_string(), no_path()), ("bf3".to_string(), dust())];
        let reversed = vec![("bf3".to_string(), dust()), ("bf1".to_string(), no_path())];
        assert_eq!(aggregate_no_route_reason(&forward), Some(NoPathReason::AmountTooSmall));
        assert_eq!(aggregate_no_route_reason(&reversed), Some(NoPathReason::AmountTooSmall));
    }

    #[test]
    fn aggregate_token_not_in_graph_wins_over_amount_too_small() {
        use crate::algorithm::NoPathReason;
        let failed = vec![
            (
                "bf".to_string(),
                SolveError::no_route_found_with_reason("o1", NoPathReason::AmountTooSmall),
            ),
            (
                "ml".to_string(),
                SolveError::no_route_found_with_reason(
                    "o2",
                    NoPathReason::DestinationTokenNotInGraph,
                ),
            ),
        ];
        assert_eq!(
            aggregate_no_route_reason(&failed),
            Some(NoPathReason::DestinationTokenNotInGraph)
        );
    }

    #[test]
    fn test_rank_quotes_returns_sorted_candidates() {
        let responses = OrderResponses {
            order_id: "test".to_string(),
            quotes: vec![
                (
                    "pool_a".to_string(),
                    OrderQuote::new(
                        "test".to_string(),
                        QuoteStatus::Success,
                        BigUint::from(1000u64),
                        BigUint::from(800u64),
                        BigUint::from(100_000u64),
                        BigUint::from(800u64),
                        BlockInfo::new(1, "0x123".to_string(), 1000),
                        "test".to_string(),
                        Bytes::from(make_address(0xAA).as_ref()),
                        Bytes::from(make_address(0xAA).as_ref()),
                        "1".to_string(),
                    ),
                ),
                (
                    "pool_b".to_string(),
                    OrderQuote::new(
                        "test".to_string(),
                        QuoteStatus::Success,
                        BigUint::from(1000u64),
                        BigUint::from(950u64),
                        BigUint::from(100_000u64),
                        BigUint::from(950u64),
                        BlockInfo::new(1, "0x123".to_string(), 1000),
                        "test".to_string(),
                        Bytes::from(make_address(0xAA).as_ref()),
                        Bytes::from(make_address(0xAA).as_ref()),
                        "1".to_string(),
                    ),
                ),
            ],
            failed_solvers: vec![],
        };

        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());

        assert_eq!(result.len(), 2);
        assert_eq!(*result[0].amount_out_net_gas(), BigUint::from(950u64));
        assert_eq!(*result[1].amount_out_net_gas(), BigUint::from(800u64));
    }

    fn exclusive_policy() -> PermissionPolicy {
        PermissionPolicy::new(|c| c.protocol_system == "vm:exclusive")
    }

    /// Like `make_single_quote` but the swap uses an exclusive protocol component.
    /// `amount_out` is used as both the route output and `amount_out_net_gas` (gas cost = 0 for
    /// simplicity in surplus unit tests).
    fn make_exclusive_quote(amount_out: u64) -> SingleOrderQuote {
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
        let tin_token = make_token(tin.clone());
        let tout_token = make_token(tout.clone());
        let mut comp = component(
            "0x0000000000000000000000000000000000000002",
            &[tin_token.clone(), tout_token.clone()],
        );
        comp.protocol_system = "vm:exclusive".to_string();
        let swap = Swap::new(
            "pool-perm".to_string(),
            "vm:exclusive".to_string(),
            tin.clone(),
            tout.clone(),
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(50_000u64),
            comp,
            Box::new(MockProtocolSim::default()),
        );
        let mut tokens = HashMap::new();
        tokens.insert(tin, tin_token);
        tokens.insert(tout, tout_token);
        let quote = OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(100_000u64),
            BigUint::from(amount_out),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
            "1".to_string(),
        )
        .with_route(Route::new(vec![swap], tokens).expect("non-empty route"));
        SingleOrderQuote::new(quote, 5)
    }

    /// Like `make_single_quote` but `amount_out` = `amount_out_net_gas` (zero gas cost) for
    /// easier surplus math in tests.
    fn make_public_quote_zero_gas(amount_out: u64) -> SingleOrderQuote {
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
        let tin_token = make_token(tin.clone());
        let tout_token = make_token(tout.clone());
        let swap = Swap::new(
            "pool-1".to_string(),
            "uniswap_v2".to_string(),
            tin.clone(),
            tout.clone(),
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(50_000u64),
            component(
                "0x0000000000000000000000000000000000000001",
                &[tin_token.clone(), tout_token.clone()],
            ),
            Box::new(MockProtocolSim::default()),
        );
        let mut tokens = HashMap::new();
        tokens.insert(tin, tin_token);
        tokens.insert(tout, tout_token);
        let quote = OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(100_000u64),
            BigUint::from(amount_out),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
            "1".to_string(),
        )
        .with_route(Route::new(vec![swap], tokens).expect("non-empty route"));
        SingleOrderQuote::new(quote, 5)
    }

    /// Builds an `OrderResponses` with a public quote and a surplus quote (zero gas cost).
    fn surplus_responses(public_out: u64, surplus_out: u64) -> OrderResponses {
        let public = make_public_quote_zero_gas(public_out)
            .order()
            .clone();
        let surplus = make_exclusive_quote(surplus_out)
            .order()
            .clone();
        OrderResponses {
            order_id: "test-order".to_string(),
            quotes: vec![
                ("public_pool".to_string(), public),
                ("surplus_pool".to_string(), surplus),
            ],
            failed_solvers: vec![],
        }
    }

    fn surplus_pool_roles() -> HashMap<String, PoolRole> {
        HashMap::from([
            ("public_pool".to_string(), PoolRole::Public),
            ("surplus_pool".to_string(), PoolRole::All),
        ])
    }

    #[test]
    fn combine_prefers_surplus_when_it_beats_public() {
        let responses = surplus_responses(900, 950);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &surplus_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        // The surplus winner is at the head: user is quoted the committed public output, protocol
        // captures the surplus. The public candidate remains as a fallback.
        assert_eq!(combined.len(), 2);
        assert_eq!(*combined[0].amount_out(), BigUint::from(900u64));
        assert_eq!(combined[0].committed_amount_out(), Some(&BigUint::from(900u64)));
        assert_eq!(combined[0].surplus_amount(), Some(&BigUint::from(50u64)));
    }

    #[test]
    fn combine_falls_back_to_public_when_surplus_does_not_beat_it() {
        let responses = surplus_responses(950, 900);
        let public_ranked = vec![make_public_quote_zero_gas(950)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &surplus_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        assert_eq!(combined.len(), 1);
        assert_eq!(*combined[0].amount_out(), BigUint::from(950u64));
        assert_eq!(combined[0].surplus_amount(), None);
    }

    #[test]
    fn combine_stamps_per_leg_committed_amount_out() {
        let responses = surplus_responses(900, 1000);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &surplus_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        let surplus_quote = &combined[0];
        let route = surplus_quote
            .route()
            .expect("surplus quote should have a route");
        let perm_swap = route
            .swaps()
            .iter()
            .find(|s| policy.is_exclusive(s.protocol_component()))
            .expect("should have an exclusive swap");

        // committed_leg = leg.amount_out * committed_route_out / realized_route_out
        // = 1000 * 900 / 1000 = 900
        assert_eq!(perm_swap.committed_amount_out(), Some(&BigUint::from(900u64)),);
    }

    #[test]
    fn combine_user_never_short_changed() {
        // Surplus is only slightly better — verify the invariant holds.
        let responses = surplus_responses(999, 1000);
        let public_ranked = vec![make_public_quote_zero_gas(999)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &surplus_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        let surplus_quote = &combined[0];
        assert!(
            surplus_quote.amount_out() >= &BigUint::from(999u64),
            "user must receive at least the committed amount"
        );
    }

    #[test]
    fn combine_no_policy_returns_public_unchanged() {
        let responses = surplus_responses(900, 950);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let combined = combine_with_surplus(
            &responses,
            &surplus_pool_roles(),
            &QuoteOptions::default(),
            public_ranked.clone(),
            None,
        );

        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].surplus_amount(), None);
    }

    #[test]
    fn combine_equal_net_gas_falls_back_to_public() {
        // Equal net-of-gas does NOT count as "beats" — no surplus captured.
        let responses = surplus_responses(900, 900);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &surplus_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].surplus_amount(), None);
    }
}
