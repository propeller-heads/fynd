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
    encoding::encoder::Encoder,
    feed::scope::{ExclusivityPolicy, LiquidityScope},
    price_guard::guard::PriceGuard,
    worker_pool::task_queue::TaskQueueHandle,
    BlockInfo, EncodingOptions, Order, OrderQuote, Quote, QuoteOptions, QuoteRequest, QuoteStatus,
    SolveError, SolveParams, SurplusInfo,
};

/// The role a solver pool (a group of workers) plays in a quote.
///
/// A `Public` worker routes only through public liquidity and provides the committed (quoted)
/// reference output. An `ExclusiveAccess` worker routes through all liquidity, public included,
/// plus exclusive components, and may beat that reference — in which case the protocol captures
/// the surplus.
///
/// Serialized in snake_case (`"public"` / `"exclusive_access"`) in `worker_pools.toml` via
/// [`PoolConfig`].
///
/// [`PoolConfig`]: crate::PoolConfig
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolRole {
    /// Routes through public liquidity only. Establishes the committed reference output. Default.
    #[default]
    Public,
    /// Routes through all liquidity, public and exclusive components alike; candidates from
    /// this role may capture surplus above the public reference.
    ExclusiveAccess,
}

impl PoolRole {
    /// The per-worker liquidity scope for a pool of this role.
    ///
    /// Only a `Public` pool with a policy filters; an `ExclusiveAccess` pool (and any pool
    /// without a policy) sees everything.
    pub(crate) fn liquidity_scope(self, policy: Option<ExclusivityPolicy>) -> LiquidityScope {
        match (self, policy) {
            (PoolRole::Public, Some(policy)) => LiquidityScope::PublicOnly(policy),
            (PoolRole::Public, None) | (PoolRole::ExclusiveAccess, _) => LiquidityScope::All,
        }
    }
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

    /// Sets the pool's role (e.g. [`PoolRole::ExclusiveAccess`] for a pool that routes through
    /// exclusive liquidity as well).
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
    /// consumed by the price guard); exclusive-access candidates are overlaid separately by
    /// `combine_with_surplus`. `failed_solvers` is retained so placeholder construction is
    /// unchanged.
    fn public_only(&self, pool_roles: &HashMap<String, PoolRole>) -> OrderResponses {
        let quotes = self
            .quotes
            .iter()
            .filter(|(pool, _)| pool_roles.get(pool) != Some(&PoolRole::ExclusiveAccess))
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
    /// Predicate identifying the exclusive legs in exclusive-access routes. `None` when no
    /// exclusive components are configured.
    exclusivity_policy: Option<ExclusivityPolicy>,
}

impl WorkerPoolRouter {
    /// Creates a new WorkerPoolRouter with the given solver pools, config, and encoder.
    pub fn new(
        solver_pools: Vec<SolverPoolHandle>,
        config: WorkerPoolRouterConfig,
        encoder: Encoder,
    ) -> Self {
        Self { solver_pools, config, encoder, price_guard: None, exclusivity_policy: None }
    }

    /// Attaches the policy used to identify exclusive legs in exclusive-access routes.
    pub fn with_exclusivity_policy(mut self, policy: ExclusivityPolicy) -> Self {
        self.exclusivity_policy = Some(policy);
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

        // Map each pool name to its role so candidate quotes can be split into public vs
        // exclusive-access.
        let pool_roles: HashMap<String, PoolRole> = self
            .solver_pools
            .iter()
            .map(|p| (p.name().to_string(), p.role()))
            .collect();
        let has_exclusive_access_pool = pool_roles
            .values()
            .any(|r| *r == PoolRole::ExclusiveAccess);

        // Rank quotes for each order (sorted by refined amount_out_net_gas descending).
        // `rank_quotes` produces the public ranking — the committed reference AND the price-guard
        // fallback chain. When an `ExclusiveAccess`-role pool is configured, the winning
        // exclusive-access candidate is overlaid
        // onto that ranked list (prepended) by `combine_with_surplus`, so the fallbacks are
        // preserved.
        let ranked_quotes: Vec<Vec<OrderQuote>> = order_responses
            .into_iter()
            .map(|responses| {
                if has_exclusive_access_pool {
                    let public_ranked =
                        self.rank_quotes(&responses.public_only(&pool_roles), request.options());
                    combine_with_surplus(
                        &responses,
                        &pool_roles,
                        request.options(),
                        public_ranked,
                        self.exclusivity_policy.as_ref(),
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
        let exclusive_access_pool_names: HashSet<String> = self
            .solver_pools
            .iter()
            .filter(|p| p.role() == PoolRole::ExclusiveAccess)
            .map(|p| p.name().to_string())
            .collect();
        let has_exclusive_access_pool = !exclusive_access_pool_names.is_empty();

        let mut quotes = Vec::new();
        let mut failed_solvers: Vec<(String, SolveError)> = Vec::new();
        let mut remaining_pools: HashSet<String> = self
            .solver_pools
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        let mut has_public_response = false;
        let mut has_exclusive_access_response = false;

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

                            if exclusive_access_pool_names.contains(&pool_name) {
                                has_exclusive_access_response = true;
                            } else {
                                has_public_response = true;
                            }

                            // Extract the OrderQuote from SingleOrderQuote
                            quotes.push((pool_name.clone(), single_quote.order().clone()));

                            // Role-aware early return: when an exclusive-access pool is
                            // configured, only fire once we have ≥1 public AND the
                            // exclusive-access pool (so the surplus overlay has both inputs).
                            // Without an exclusive-access pool, use pure
                            // count-based gating (original behaviour).
                            let role_ready = if has_exclusive_access_pool {
                                has_public_response && has_exclusive_access_response
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
                            // A failed exclusive-access pool still counts as "responded" for
                            // gating — we know it won't produce a surplus quote, so the public
                            // pool can
                            // early-return without waiting for a result that will never come.
                            if exclusive_access_pool_names.contains(&pool_name) {
                                has_exclusive_access_response = true;
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

/// Builds the final ranked quote list for one order by deciding whether an exclusive-access
/// route should execute instead of the best public route.
///
/// Inputs: `public_ranked` is the ranking of public-pool quotes from `rank_quotes`; its head is
/// the public reference from which the committed amount is derived. `responses` additionally
/// holds the candidates from `ExclusiveAccess`-role pools (routes that may use exclusive
/// components).
///
/// The committed amount honors two floors: `max(public_amount_out, public_net + exclusive_gas)`.
/// The first keeps the quoted `amount_out` from ever dropping below the public market's; the
/// second keeps the user — who pays the exclusive route's gas — netting at least what the public
/// route would leave them.
///
/// If the best exclusive-access candidate beats the public reference net-of-gas and produces at
/// least the committed amount, this returns a new list whose head is the pinned surplus quote,
/// followed by every public candidate as price-guard fallbacks. The pinned quote is the winning
/// candidate with:
/// - `amount_out` pinned to the committed amount,
/// - `Swap::committed_amount_out` set on each exclusive leg (consumed by the encoder), and
/// - an order-level [`SurplusInfo`] attached (observability).
///
/// Otherwise `public_ranked` is returned unchanged. Either way the user is never worse off than
/// the public market — neither in quoted `amount_out` nor net of gas.
///
/// Per-leg attribution: the route's excess over the committed amount (`realized − committed`)
/// is deducted from the exclusive legs — each leg absorbs what it can, capped at its own output,
/// in route order. Public branches of a split cannot be skimmed, so the whole excess
/// comes out of the exclusive legs; if they cannot absorb all of it, the remainder is
/// under-captured (user-safe: the user then receives more than the committed amount).
///
/// Exact-in orders only: the commitment, both gates, and the surplus are all denominated in
/// `amount_out`. Exact-out support would invert the logic — fixed output, commitment and surplus
/// on the input side — and needs its own treatment here.
fn combine_with_surplus(
    responses: &OrderResponses,
    pool_roles: &HashMap<String, PoolRole>,
    options: &QuoteOptions,
    public_ranked: Vec<OrderQuote>,
    exclusivity_policy: Option<&ExclusivityPolicy>,
) -> Vec<OrderQuote> {
    let Some(policy) = exclusivity_policy else {
        return public_ranked;
    };

    let committed = match public_ranked.first() {
        Some(q) if q.status() == QuoteStatus::Success => q,
        _ => return public_ranked,
    };

    // Find the best exclusive-access candidate, respecting max_gas and route shape constraints.
    let best_exclusive_access_candidate = responses
        .quotes
        .iter()
        .filter(|(pool, _)| pool_roles.get(pool) == Some(&PoolRole::ExclusiveAccess))
        .filter(|(_, q)| q.status() == QuoteStatus::Success)
        .filter(|(_, q)| {
            options
                .max_gas()
                .map(|max| q.gas_estimate() <= max)
                .unwrap_or(true)
        })
        .filter(|(_, q)| has_valid_exclusive_route(q, policy))
        .max_by(|(_, a), (_, b)| {
            a.amount_out_net_gas()
                .cmp(b.amount_out_net_gas())
        })
        .map(|(_, q)| q);

    let Some(exclusive_candidate) = best_exclusive_access_candidate else {
        return public_ranked;
    };

    // The candidate route must beat the committed reference net-of-gas.
    if exclusive_candidate.amount_out_net_gas() <= committed.amount_out_net_gas() {
        return public_ranked;
    }

    // Exact-in assumption: everything below compares and commits output amounts. For exact-out
    // orders this comparison would have to run on amount_in instead (see function docs).
    let exclusive_route_amount_out = exclusive_candidate.amount_out();
    let public_amount_out = committed.amount_out();

    // We promise the user at least the public route's output. A private route that produces
    // less can't keep that promise, so we skip it — even when its gas savings make it better
    // net.
    if exclusive_route_amount_out < public_amount_out {
        return public_ranked;
    }

    // Gas the user pays to execute the exclusive route, in output-token terms.
    let exclusive_gas_cost = exclusive_route_amount_out - exclusive_candidate.amount_out_net_gas();

    // The commitment honors two floors: the quoted amount_out never drops below the public
    // market's, and the user — who pays the exclusive route's gas — never nets less than the
    // public route would leave them (public_net + gas). Together with the strict net check
    // above, these gates guarantee the route covers the committed amount.
    let committed_amount_out =
        (committed.amount_out_net_gas() + &exclusive_gas_cost).max(public_amount_out.clone());

    // What the hooks capture: everything the route produces above the commitment.
    let surplus_amount = exclusive_route_amount_out - &committed_amount_out;

    // Pin the winning candidate: stamp per-leg committed amounts, pin amount_out to committed,
    // attach SurplusInfo.
    let mut surplus_quote = exclusive_candidate.clone();

    // Final output of each swap's path, walked backwards (a path's terminal output propagates
    // to its chained predecessors). Converting captured excess into a leg's token needs the
    // realized downstream price `path_final_out / leg_out`; for terminal legs the ratio is 1.
    let path_final_outs: Vec<BigUint> = surplus_quote
        .route()
        .map(|route| {
            let swaps = route.swaps();
            let mut finals = vec![BigUint::ZERO; swaps.len()];
            let mut current_final = BigUint::ZERO;
            for i in (0..swaps.len()).rev() {
                let is_terminal =
                    i == swaps.len() - 1 || swaps[i + 1].token_in() != swaps[i].token_out();
                if is_terminal {
                    current_final = swaps[i].amount_out().clone();
                }
                finals[i] = current_final.clone();
            }
            finals
        })
        .unwrap_or_default();

    if let Some(route) = surplus_quote.route_mut() {
        // Capture the route's excess at the exclusive legs.
        // Each leg absorbs up to its path's capacity; any remainder is left with the user.
        let mut excess = exclusive_route_amount_out - &committed_amount_out;
        for (i, swap) in route.swaps_mut().iter_mut().enumerate() {
            if policy.is_exclusive(swap.protocol_component()) {
                let Some(path_final_out) = path_final_outs.get(i) else {
                    continue;
                };
                let captured = excess
                    .clone()
                    .min(path_final_out.clone());
                // Convert into the leg's own token, rounding down so any error is taken
                // from the protocol, not the user. Today the leg is always the last hop of
                // its path (validator), so path_final_out equals the leg's output and this
                // divides by itself — a plain subtraction. Once mid-path legs are allowed,
                // the "user gets at least the committed amount" guarantee also requires the
                // pools after the leg to have diminishing returns.
                let captured_leg = if *path_final_out == BigUint::ZERO {
                    BigUint::ZERO
                } else {
                    &captured * swap.amount_out() / path_final_out
                };
                debug_assert!(
                    captured_leg <= *swap.amount_out(),
                    "captured amount ({captured_leg}) must not exceed the leg's output ({})",
                    swap.amount_out(),
                );
                let committed_leg = swap.amount_out() - &captured_leg;
                excess -= captured;
                swap.set_committed_amount_out(committed_leg);
            }
        }
    }

    surplus_quote.set_amount_out(committed_amount_out.clone());

    // The user nets the committed amount minus the exclusive route's gas; by construction of
    // the committed amount this is >= the public head's net, so the returned list stays ranked
    // descending.
    surplus_quote.set_amount_out_net_gas(&committed_amount_out - &exclusive_gas_cost);

    let surplus_info = SurplusInfo::new(surplus_amount, committed_amount_out);
    surplus_quote = surplus_quote.with_surplus(surplus_info);

    let mut result = Vec::with_capacity(public_ranked.len() + 1);
    result.push(surplus_quote);
    result.extend(public_ranked);
    result
}

/// Returns `true` only for routes carrying exactly one exclusive leg, positioned as the terminal
/// leg of its path.
///
/// Returns `false` for routes with no exclusive leg, more than one exclusive leg, an empty
/// route, no route at all, or an exclusive leg that sits mid-path.
///
/// Both constraints are v1 restrictions that keep per-leg surplus attribution exact and
/// unambiguous: a mid-route leg would need inverse simulation to convert the excess into its
/// token, and multiple exclusive legs make the per-pool attribution non-unique. Both are
/// deferred to a future version. Path boundaries are detected by checking whether the next
/// swap's `token_in` differs from the current swap's `token_out` — correct for Fynd's sequential
/// route representation, but would need revisiting if routes gain explicit path-boundary markers.
fn has_valid_exclusive_route(quote: &OrderQuote, policy: &ExclusivityPolicy) -> bool {
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

    exclusive_count == 1
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
    #[case::pending_exclusive_pool(0, Some(300), true)]
    #[case::pending_public_pool(300, Some(0), true)]
    #[case::failed_exclusive_pool(0, None, false)]
    #[tokio::test]
    async fn test_router_early_return_role_gating(
        #[case] public_delay_ms: u64,
        #[case] exclusive_delay_ms: Option<u64>,
        #[case] expect_surplus: bool,
    ) {
        let (public_pool, public_worker) =
            create_mock_pool("public_pool", Ok(make_single_quote(800)), public_delay_ms);
        let exclusive_response = match exclusive_delay_ms {
            Some(_) => Ok(make_exclusive_quote(1100)),
            None => Err(SolveError::NoRouteFound { order_id: "test-order".to_string() }),
        };
        let (exclusive_pool, exclusive_worker) =
            create_mock_pool("exclusive_pool", exclusive_response, exclusive_delay_ms.unwrap_or(0));
        let exclusive_pool = exclusive_pool.with_role(PoolRole::ExclusiveAccess);

        let config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(2000))
            .with_min_responses(1);
        let worker_router =
            WorkerPoolRouter::new(vec![public_pool, exclusive_pool], config, default_encoder())
                .with_exclusivity_policy(exclusive_policy());
        let request = QuoteRequest::new(vec![make_order()], QuoteOptions::default());

        let start = Instant::now();
        let result = worker_router
            .quote(request)
            .await
            .expect("quote should succeed");
        let elapsed = start.elapsed();

        // Well under the 2s timeout: the gate releases as soon as both roles have responded.
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
        let order = &result.orders()[0];
        assert_eq!(order.status(), QuoteStatus::Success);
        assert_eq!(*order.amount_out(), BigUint::from(990u64));
        assert_eq!(order.surplus_amount().is_some(), expect_surplus);

        drop(worker_router);
        public_worker.abort();
        exclusive_worker.abort();
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

    fn exclusive_policy() -> ExclusivityPolicy {
        ExclusivityPolicy::new(|c| c.protocol_system == "vm:exclusive")
    }

    /// Like `make_single_quote` but the swap uses an exclusive protocol component. The swap's own
    /// output is `leg_amount_out`, letting tests exercise per-leg attribution where the leg
    /// differs from the route total
    fn make_exclusive_quote_with_leg(
        amount_out: u64,
        amount_out_net_gas: u64,
        leg_amount_out: u64,
    ) -> SingleOrderQuote {
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
            BigUint::from(leg_amount_out),
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

    fn make_exclusive_quote(amount_out: u64) -> SingleOrderQuote {
        make_exclusive_quote_with_leg(amount_out, amount_out, amount_out)
    }

    /// Split route with two parallel branches (same token pair): a public pool producing
    /// `public_leg_out` and an exclusive pool producing `exclusive_leg_out`. Route output is the
    /// sum of the branches; zero gas cost.
    fn make_exclusive_split_quote(public_leg_out: u64, exclusive_leg_out: u64) -> SingleOrderQuote {
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
        let public_swap = Swap::new(
            "pool-pub".to_string(),
            "uniswap_v2".to_string(),
            tin.clone(),
            tout.clone(),
            BigUint::from(500u64),
            BigUint::from(public_leg_out),
            BigUint::from(50_000u64),
            component(
                "0x0000000000000000000000000000000000000001",
                &[tin_token.clone(), tout_token.clone()],
            ),
            Box::new(MockProtocolSim::default()),
        );
        let mut exclusive_comp = component(
            "0x0000000000000000000000000000000000000002",
            &[tin_token.clone(), tout_token.clone()],
        );
        exclusive_comp.protocol_system = "vm:exclusive".to_string();
        let exclusive_swap = Swap::new(
            "pool-perm".to_string(),
            "vm:exclusive".to_string(),
            tin.clone(),
            tout.clone(),
            BigUint::from(500u64),
            BigUint::from(exclusive_leg_out),
            BigUint::from(50_000u64),
            exclusive_comp,
            Box::new(MockProtocolSim::default()),
        );
        let mut tokens = HashMap::new();
        tokens.insert(tin, tin_token);
        tokens.insert(tout, tout_token);
        let total = public_leg_out + exclusive_leg_out;
        let quote = OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(total),
            BigUint::from(100_000u64),
            BigUint::from(total),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
            "1".to_string(),
        )
        .with_route(
            Route::new(vec![public_swap, exclusive_swap], tokens).expect("non-empty route"),
        );
        SingleOrderQuote::new(quote, 5)
    }

    /// Like `make_single_quote` but with a configurable `amount_out_net_gas` so tests can
    /// express non-zero gas cost.
    fn make_public_quote_with_net(amount_out: u64, amount_out_net_gas: u64) -> SingleOrderQuote {
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

    fn make_public_quote_zero_gas(amount_out: u64) -> SingleOrderQuote {
        make_public_quote_with_net(amount_out, amount_out)
    }

    /// Builds an `OrderResponses` with a public quote and an exclusive-access quote (zero gas
    /// cost).
    fn exclusive_access_responses(public_out: u64, exclusive_out: u64) -> OrderResponses {
        let public = make_public_quote_zero_gas(public_out)
            .order()
            .clone();
        let exclusive_access = make_exclusive_quote(exclusive_out)
            .order()
            .clone();
        OrderResponses {
            order_id: "test-order".to_string(),
            quotes: vec![
                ("public_pool".to_string(), public),
                ("exclusive_access_pool".to_string(), exclusive_access),
            ],
            failed_solvers: vec![],
        }
    }

    fn exclusive_access_pool_roles() -> HashMap<String, PoolRole> {
        HashMap::from([
            ("public_pool".to_string(), PoolRole::Public),
            ("exclusive_access_pool".to_string(), PoolRole::ExclusiveAccess),
        ])
    }

    /// Head selection across the gate case matrix. Each route is given as `(gross, net)`;
    /// expected is `(head amount_out, captured surplus)`. A surplus win prepends the pinned
    /// quote to the public fallbacks; otherwise the public ranking is returned unchanged.
    #[rstest]
    #[case::exclusive_beats_public((900, 900), (950, 950), (900, Some(50)))]
    #[case::exclusive_below_public((950, 950), (900, 900), (950, None))]
    #[case::exclusive_ties_public_net((900, 900), (900, 900), (900, None))]
    #[case::exclusive_marginally_better((999, 999), (1000, 1000), (999, Some(1)))]
    #[case::exclusive_cannot_cover_public_gross((1000, 950), (990, 980), (1000, None))]
    fn combine_head_selection(
        #[case] public: (u64, u64),
        #[case] exclusive: (u64, u64),
        #[case] expected: (u64, Option<u64>),
    ) {
        let (public_out, public_net) = public;
        let (exclusive_out, exclusive_net) = exclusive;
        let (expected_amount_out, expected_surplus) = expected;

        let responses = responses_with_gas(public_out, public_net, exclusive_out, exclusive_net);
        let public_ranked = vec![make_public_quote_with_net(public_out, public_net)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        let expected_surplus = expected_surplus.map(BigUint::from);
        assert_eq!(combined.len(), if expected_surplus.is_some() { 2 } else { 1 });
        assert_eq!(*combined[0].amount_out(), BigUint::from(expected_amount_out));
        assert_eq!(combined[0].surplus_amount(), expected_surplus.as_ref());
        if expected_surplus.is_some() {
            assert_eq!(
                combined[0].committed_amount_out(),
                Some(&BigUint::from(expected_amount_out))
            );
        }
    }

    #[rstest]
    #[case::over_max_gas(
        make_exclusive_quote(1100).order().clone(),
        Some(50_000)
    )]
    #[case::mid_route_exclusive_leg(
        make_route_quote(&[("vm:exclusive", 0x01, 0x02), ("uniswap_v2", 0x02, 0x03)]),
        None
    )]
    fn combine_filters_exclusive_candidate(
        #[case] exclusive_quote: OrderQuote,
        #[case] max_gas: Option<u64>,
    ) {
        let mut options = QuoteOptions::default();
        if let Some(max) = max_gas {
            options = options.with_max_gas(BigUint::from(max));
        }
        let responses = OrderResponses {
            order_id: "test-order".to_string(),
            quotes: vec![
                (
                    "public_pool".to_string(),
                    make_public_quote_zero_gas(900)
                        .order()
                        .clone(),
                ),
                ("exclusive_access_pool".to_string(), exclusive_quote),
            ],
            failed_solvers: vec![],
        };
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &options,
            public_ranked,
            Some(&policy),
        );

        assert_eq!(combined.len(), 1);
        assert_eq!(*combined[0].amount_out(), BigUint::from(900u64));
        assert_eq!(combined[0].surplus_amount(), None);
    }

    #[test]
    fn combine_stamps_per_leg_committed_amount_out() {
        let responses = exclusive_access_responses(900, 1000);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
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
    fn combine_committed_leg_deduction() {
        // leg = 995, committed = 900, realized = 1000: the route's excess (100) is deducted from
        // the exclusive leg in full — committed_leg = 995 − 100 = 895, exactly, no rounding.
        let responses = OrderResponses {
            order_id: "test-order".to_string(),
            quotes: vec![
                (
                    "public_pool".to_string(),
                    make_public_quote_zero_gas(900)
                        .order()
                        .clone(),
                ),
                (
                    "exclusive_access_pool".to_string(),
                    make_exclusive_quote_with_leg(1000, 1000, 995)
                        .order()
                        .clone(),
                ),
            ],
            failed_solvers: vec![],
        };
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        let route = combined[0]
            .route()
            .expect("surplus quote should have a route");
        let perm_swap = route
            .swaps()
            .iter()
            .find(|s| policy.is_exclusive(s.protocol_component()))
            .expect("should have an exclusive swap");
        assert_eq!(perm_swap.committed_amount_out(), Some(&BigUint::from(895u64)));
    }

    #[test]
    fn combine_without_exclusivity_policy() {
        let responses = exclusive_access_responses(900, 950);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &QuoteOptions::default(),
            public_ranked.clone(),
            None,
        );

        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].surplus_amount(), None);
    }

    #[test]
    fn combine_gas_heavier_exclusive_compensates_user() {
        // public 1000/950 (gas 50); exclusive 1100/960 (gas 140). The committed amount rises to
        // max(1000, 950 + 140) = 1090 so the user still nets 950 after the pricier gas; the
        // protocol captures 1100 − 1090 = 10 (the value the route actually created).
        let responses = responses_with_gas(1000, 950, 1100, 960);
        let public_ranked = vec![make_public_quote_with_net(1000, 950)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        assert_eq!(combined.len(), 2);
        assert_eq!(*combined[0].amount_out(), BigUint::from(1090u64));
        assert_eq!(*combined[0].amount_out_net_gas(), BigUint::from(950u64));
        assert_eq!(combined[0].surplus_amount(), Some(&BigUint::from(10u64)));
        assert_eq!(combined[0].committed_amount_out(), Some(&BigUint::from(1090u64)));
    }

    #[test]
    fn combine_gas_cheaper_exclusive_keeps_public_amount() {
        // public 1000/950 (gas 50); exclusive 1100/1060 (gas 40). The public floor dominates:
        // committed = max(1000, 950 + 40) = 1000. The user keeps the gas saving (nets 960) and
        // the protocol captures 100.
        let responses = responses_with_gas(1000, 950, 1100, 1060);
        let public_ranked = vec![make_public_quote_with_net(1000, 950)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        assert_eq!(combined.len(), 2);
        assert_eq!(*combined[0].amount_out(), BigUint::from(1000u64));
        assert_eq!(*combined[0].amount_out_net_gas(), BigUint::from(960u64));
        assert_eq!(combined[0].surplus_amount(), Some(&BigUint::from(100u64)));
    }

    #[test]
    fn combine_split_route_attribution() {
        // Split route: public branch 600 + exclusive branch 500 = 1100 realized vs 1000
        // committed (zero gas). Only the exclusive leg is stamped; the public branch flows to
        // the user untouched.
        let responses = OrderResponses {
            order_id: "test-order".to_string(),
            quotes: vec![
                (
                    "public_pool".to_string(),
                    make_public_quote_zero_gas(1000)
                        .order()
                        .clone(),
                ),
                (
                    "exclusive_access_pool".to_string(),
                    make_exclusive_split_quote(600, 500)
                        .order()
                        .clone(),
                ),
            ],
            failed_solvers: vec![],
        };
        let public_ranked = vec![make_public_quote_zero_gas(1000)
            .order()
            .clone()];
        let policy = exclusive_policy();
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_roles(),
            &QuoteOptions::default(),
            public_ranked,
            Some(&policy),
        );

        assert_eq!(*combined[0].amount_out(), BigUint::from(1000u64));
        let route = combined[0]
            .route()
            .expect("surplus quote should have a route");
        let public_leg = route
            .swaps()
            .iter()
            .find(|s| !policy.is_exclusive(s.protocol_component()))
            .expect("should have a public swap");
        assert_eq!(public_leg.committed_amount_out(), None);

        // The public branch (600) can't be skimmed, so the full excess (1100 − 1000 = 100) is
        // deducted from the exclusive leg: committed_leg = 500 − 100 = 400. The user receives
        // 600 + 400 = 1000 (exactly the committed amount) and the hook captures all 100.
        let exclusive_leg = route
            .swaps()
            .iter()
            .find(|s| policy.is_exclusive(s.protocol_component()))
            .expect("should have an exclusive swap");
        assert_eq!(exclusive_leg.committed_amount_out(), Some(&BigUint::from(400u64)));
    }

    /// Builds an `OrderResponses` where both quotes carry explicit `amount_out_net_gas`.
    fn responses_with_gas(
        public_out: u64,
        public_net: u64,
        exclusive_out: u64,
        exclusive_net: u64,
    ) -> OrderResponses {
        OrderResponses {
            order_id: "test-order".to_string(),
            quotes: vec![
                (
                    "public_pool".to_string(),
                    make_public_quote_with_net(public_out, public_net)
                        .order()
                        .clone(),
                ),
                (
                    "exclusive_access_pool".to_string(),
                    make_exclusive_quote_with_leg(exclusive_out, exclusive_net, exclusive_out)
                        .order()
                        .clone(),
                ),
            ],
            failed_solvers: vec![],
        }
    }

    /// Builds a Success quote whose route has one swap per `(protocol_system, token_in, token_out)`
    /// leg, for exercising `has_valid_exclusive_route` on multi-leg and multi-path route shapes.
    fn make_route_quote(legs: &[(&str, u8, u8)]) -> OrderQuote {
        let make_token = |addr: &Address| Token {
            address: addr.clone(),
            symbol: "T".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: SimChain::Ethereum,
            quality: 100,
        };
        let mut tokens = HashMap::new();
        let mut swaps = Vec::new();
        for (protocol_system, tin_byte, tout_byte) in legs {
            let tin = make_address(*tin_byte);
            let tout = make_address(*tout_byte);
            let tin_token = make_token(&tin);
            let tout_token = make_token(&tout);
            let mut comp = component(
                "0x0000000000000000000000000000000000000002",
                &[tin_token.clone(), tout_token.clone()],
            );
            comp.protocol_system = protocol_system.to_string();
            swaps.push(Swap::new(
                format!("pool-{tin_byte}-{tout_byte}"),
                protocol_system.to_string(),
                tin.clone(),
                tout.clone(),
                BigUint::from(1000u64),
                BigUint::from(1000u64),
                BigUint::from(50_000u64),
                comp,
                Box::new(MockProtocolSim::default()),
            ));
            tokens.insert(tin, tin_token);
            tokens.insert(tout, tout_token);
        }
        OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(1000u64),
            BigUint::from(100_000u64),
            BigUint::from(1000u64),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
            "1".to_string(),
        )
        .with_route(Route::new(swaps, tokens).expect("non-empty route"))
    }

    /// Route-shape validation: exactly one exclusive leg, terminal in its path.
    #[rstest]
    #[case::terminal_exclusive_leg(
        &[("uniswap_v2", 0x01, 0x02), ("vm:exclusive", 0x02, 0x03)], true)]
    #[case::mid_route_exclusive_leg(
        &[("vm:exclusive", 0x01, 0x02), ("uniswap_v2", 0x02, 0x03)], false)]
    // Split route in sequential representation: path 1 is a single exclusive hop 0x01→0x02;
    // path 2 is 0x01→0x03→0x02. The exclusive leg is terminal for its path because the next
    // swap starts over from 0x01.
    #[case::exclusive_leg_ending_its_path(
        &[("vm:exclusive", 0x01, 0x02), ("uniswap_v2", 0x01, 0x03), ("uniswap_v2", 0x03, 0x02)],
        true)]
    #[case::no_exclusive_leg(
        &[("uniswap_v2", 0x01, 0x02), ("uniswap_v2", 0x02, 0x03)], false)]
    // Two exclusive legs: out of scope for v1 (ambiguous per-pool attribution).
    #[case::two_exclusive_legs(
        &[("vm:exclusive", 0x01, 0x02), ("vm:exclusive", 0x01, 0x02)], false)]
    fn exclusive_route_validation(#[case] legs: &[(&str, u8, u8)], #[case] expected: bool) {
        let quote = make_route_quote(legs);
        assert_eq!(has_valid_exclusive_route(&quote, &exclusive_policy()), expected);
    }
}
