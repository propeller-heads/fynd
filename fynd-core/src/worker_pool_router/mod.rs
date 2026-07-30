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
//! 4. **Gas refinement**: Before ranking across worker pools, replace each candidate's naive
//!    `route.total_gas()` estimate (used internally by algorithms for ranking within a worker pool)
//!    with the more accurate `estimate_gas_usage` from tycho-execution, which accounts for token
//!    transfer costs and router overhead. The `amount_out_net_gas` values are rescaled
//!    proportionally so the final ranking reflects realistic execution cost.
//! 5. **Selection**: Choose best quote (max refined `amount_out_net_gas`)
//! 6. **Encoding**: If [`EncodingOptions`](crate::EncodingOptions) are provided in the request,
//!    encode winning solutions into executable on-chain transactions via the
//!    [`encoding::encoder::Encoder`](crate::encoding::encoder::Encoder)

pub mod config;

use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
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
    encoding::encoder::Encoder, feed::exclusivity::is_exclusive, price_guard::guard::PriceGuard,
    worker_pool::task_queue::TaskQueueHandle, BlockInfo, EncodingOptions, Order, OrderQuote, Quote,
    QuoteOptions, QuoteRequest, QuoteStatus, SolveError, SolveParams, SurplusInfo,
};

/// Environment variable overriding [`DEFAULT_USER_IMPROVEMENT_SHARE_BPS`]. Read once, on the
/// first quote that overlays an exclusive-access candidate.
const ENV_USER_IMPROVEMENT_SHARE_BPS: &str = "EXCLUSIVE_ROUTE_USER_SHARE_BPS";

/// Share of an exclusive route's improvement over the public market that is handed to the user,
/// in basis points of that improvement: `1_000` gives the user a tenth of it.
const DEFAULT_USER_IMPROVEMENT_SHARE_BPS: u32 = 1_000;

/// Basis-point denominator: `10_000` bps is the whole improvement.
const BPS_DENOMINATOR: u32 = 10_000;

/// The configured user share, parsed from [`ENV_USER_IMPROVEMENT_SHARE_BPS`].
static USER_IMPROVEMENT_SHARE_BPS: LazyLock<u32> = LazyLock::new(user_improvement_share_bps_env);

/// Reads the user share from the environment, falling back to
/// [`DEFAULT_USER_IMPROVEMENT_SHARE_BPS`] when the variable is unset or unusable.
fn user_improvement_share_bps_env() -> u32 {
    let Ok(raw) = std::env::var(ENV_USER_IMPROVEMENT_SHARE_BPS) else {
        return DEFAULT_USER_IMPROVEMENT_SHARE_BPS;
    };
    match parse_user_improvement_share_bps(&raw) {
        Some(bps) => bps,
        None => {
            warn!(
                value = %raw,
                default_bps = DEFAULT_USER_IMPROVEMENT_SHARE_BPS,
                "{ENV_USER_IMPROVEMENT_SHARE_BPS} must be an integer from 0 to \
                 {BPS_DENOMINATOR} basis points; using the default",
            );
            DEFAULT_USER_IMPROVEMENT_SHARE_BPS
        }
    }
}

/// Parses a user share in basis points, rejecting anything above the whole improvement — a share
/// over `10_000` bps would commit more output than the route produces.
fn parse_user_improvement_share_bps(raw: &str) -> Option<u32> {
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|bps| *bps <= BPS_DENOMINATOR)
}

/// Returns the part of `improvement` handed to the user, `user_share_bps` of it.
///
/// Rounded up, so a share that would otherwise round away still moves the price in the user's
/// favour. Never exceeds `improvement`, so the commitment it feeds stays within what the route
/// produces.
fn user_margin(improvement: &BigUint, user_share_bps: u32) -> BigUint {
    let denominator = BigUint::from(BPS_DENOMINATOR);
    (improvement * BigUint::from(user_share_bps) + (&denominator - 1u32)) / denominator
}

/// Which liquidity a solver pool (a group of workers) routes through, and therefore what its
/// candidates mean in a quote.
///
/// A `PublicOnly` worker pool routes only through public liquidity and provides the committed
/// (quoted) reference output; its workers filter exclusive components out of their graphs. An
/// `All` worker pool applies no filtering — its workers ingest whatever the deployment's stream
/// delivers. In a deployment opted into exclusive components at the stream filter, an `All`
/// worker pool may beat the public reference — in which case the protocol captures the surplus.
/// Without that opt-in, no exclusive components ever arrive and the two scopes behave
/// identically.
///
/// Serialized in snake_case (`"public_only"` / `"all"`) in `worker_pools.toml` via
/// [`PoolConfig`].
///
/// [`PoolConfig`]: crate::PoolConfig
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiquidityScope {
    /// Routes through public liquidity only, establishing the committed reference output.
    PublicOnly,
    /// No filtering: routes through whatever the stream delivers, exclusive components
    /// included if the deployment opted into them. Candidates from this scope may capture
    /// surplus above the public reference.
    #[default]
    All,
}

/// Handle to a solver pool for dispatching orders.
#[derive(Clone)]
pub struct SolverPoolHandle {
    /// Human-readable name for this worker pool (used in logging & metrics).
    name: String,
    /// Queue handle for this worker pool.
    queue: TaskQueueHandle,
    /// Whether this worker pool routes public-only or all liquidity.
    liquidity_scope: LiquidityScope,
}

impl SolverPoolHandle {
    /// Creates a new solver pool handle with the default [`LiquidityScope::All`] scope.
    pub fn new(name: impl Into<String>, queue: TaskQueueHandle) -> Self {
        Self { name: name.into(), queue, liquidity_scope: LiquidityScope::default() }
    }

    /// Sets the worker pool's liquidity scope (e.g. [`LiquidityScope::PublicOnly`] for a worker
    /// pool that establishes the committed public reference).
    pub fn with_liquidity_scope(mut self, scope: LiquidityScope) -> Self {
        self.liquidity_scope = scope;
        self
    }

    /// Returns the worker pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the task queue handle.
    pub fn queue(&self) -> &TaskQueueHandle {
        &self.queue
    }

    /// Returns the worker pool's liquidity scope.
    pub fn liquidity_scope(&self) -> LiquidityScope {
        self.liquidity_scope
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
    /// Returns a copy keeping only candidates from public-scoped worker pools.
    ///
    /// These form the committed reference and the ranked fallback chain (ranked by `rank_quotes`,
    /// consumed by the price guard); exclusive-access candidates are overlaid separately by
    /// `combine_with_surplus`. `failed_solvers` is retained so placeholder construction is
    /// unchanged.
    fn public_only(&self, pool_scopes: &HashMap<String, LiquidityScope>) -> OrderResponses {
        let quotes = self
            .quotes
            .iter()
            .filter(|(pool, _)| pool_scopes.get(pool) != Some(&LiquidityScope::All))
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
}

impl WorkerPoolRouter {
    /// Creates a new WorkerPoolRouter with the given solver pools, config, and encoder.
    pub fn new(
        solver_pools: Vec<SolverPoolHandle>,
        config: WorkerPoolRouterConfig,
        encoder: Encoder,
    ) -> Self {
        Self { solver_pools, config, encoder, price_guard: None }
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

        // Map each worker pool name to its liquidity scope so candidate quotes can be split into
        // public vs exclusive-access.
        let pool_scopes: HashMap<String, LiquidityScope> = self
            .solver_pools
            .iter()
            .map(|p| (p.name().to_string(), p.liquidity_scope()))
            .collect();
        // Surplus routing needs both scopes: a `public_only` pool for the committed reference
        // and an `all` pool that may beat it.
        let surplus_routing_active = pool_scopes
            .values()
            .any(|r| *r == LiquidityScope::All) &&
            pool_scopes
                .values()
                .any(|r| *r == LiquidityScope::PublicOnly);

        // Rank quotes for each order (sorted by refined amount_out_net_gas descending).
        // `rank_quotes` produces the public ranking — the committed reference AND the price-guard
        // fallback chain. When both scopes are configured, the winning exclusive-access
        // candidate is overlaid onto that ranked list (prepended) by `combine_with_surplus`, so
        // the fallbacks are preserved.
        let ranked_quotes: Vec<Vec<OrderQuote>> = order_responses
            .into_iter()
            .map(|responses| {
                if surplus_routing_active {
                    let public_ranked =
                        self.rank_quotes(&responses.public_only(&pool_scopes), request.options());
                    combine_with_surplus(
                        &responses,
                        &pool_scopes,
                        request.options(),
                        public_ranked,
                        *USER_IMPROVEMENT_SHARE_BPS,
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

        // Pre-compute which worker pool names have the exclusive-access (`All`) scope, for
        // scope-aware early return gating. The gating only applies when both scopes are
        // configured — the surplus overlay then needs one response from each; with a single
        // scope (e.g. every worker pool on the `All` default), plain count-based gating applies.
        let exclusive_access_pool_names: HashSet<String> = self
            .solver_pools
            .iter()
            .filter(|p| p.liquidity_scope() == LiquidityScope::All)
            .map(|p| p.name().to_string())
            .collect();
        let surplus_routing_active = !exclusive_access_pool_names.is_empty() &&
            exclusive_access_pool_names.len() < self.solver_pools.len();

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
                    // Mark all remaining worker pools as timed out
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

                            // Scope-aware early return: when surplus routing is active, only
                            // fire once we have ≥1 public AND ≥1 exclusive-access response (so
                            // the surplus overlay has both inputs). Otherwise, use pure
                            // count-based gating (original behaviour).
                            let scope_ready = if surplus_routing_active {
                                has_public_response && has_exclusive_access_response
                            } else {
                                true
                            };
                            if min_responses > 0
                                && quotes.len() >= min_responses
                                && scope_ready
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
                            // A failed exclusive-access worker pool still counts as "responded"
                            // for gating — we know it won't produce a surplus quote, so the
                            // public worker pools can early-return without waiting for a result
                            // that will never come.
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

        // Record failures by worker pool and error type
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
        let fallback = if let Some((_, any_q)) = responses.quotes.first() {
            counter!("worker_router_orders_total", "status" => "no_route").increment(1);
            let mut fallback = OrderQuote::new(
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
            );
            // Only label the cause when a quote is actually over the request's
            // max_gas, so a future filter or non-Success status cannot silently
            // get attributed to the gas cap.
            let over_max_gas = responses.quotes.iter().any(|(_, q)| {
                options
                    .max_gas()
                    .is_some_and(|max| q.gas_estimate() > max)
            });
            fallback.set_no_route_cause(over_max_gas.then_some(SolveError::MaxGasExceeded));
            fallback
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
            let mut fallback = OrderQuote::new(
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
            );
            fallback.set_no_route_cause(aggregate_no_route_cause(&responses.failed_solvers));
            fallback
        };
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

/// Builds the final ranked quote list for one order by deciding whether an exclusive-access
/// route should execute instead of the best public route.
///
/// Inputs: `public_ranked` is the ranking of public-worker-pool quotes from `rank_quotes`; its
/// head is the public reference from which the committed amount is derived. `responses`
/// additionally holds the candidates from `ExclusiveAccess`-scoped worker pools (routes that may
/// use exclusive components).
///
/// A candidate must at least match the public reference net of gas; what it produces on top,
/// `improvement = exclusive_net − public_net`, is then split. The user's net target is
/// `required_net = public_net + margin`, where `margin` is `user_share_bps` of `improvement` — so
/// the mark-up is only as large as the route can afford, and a route with more to give quotes a
/// better user price. Splitting the improvement rather than marking up the trade also puts the
/// mark-up below a basis point of the trade whenever the improvement is small.
///
/// The committed amount is the larger of two lower bounds:
/// `max(public_amount_out, required_net + exclusive_gas)`. The first guarantees the quoted
/// `amount_out` is never below the public market's; the second guarantees the user — who pays
/// the exclusive route's gas — nets at least `required_net`.
///
/// Which bound is larger depends on the gas comparison:
/// - `exclusive_gas > public_gas`: committed = `required_net + exclusive_gas`, which exceeds
///   `public_amount_out`. The user receives more tokens than the public quote and nets exactly
///   `required_net`.
/// - `exclusive_gas <= public_gas`: committed = `public_amount_out`. The user nets
///   `public_amount_out − exclusive_gas`, which exceeds `public_net` by the gas difference. A
///   commitment of `required_net + exclusive_gas` would still leave the user whole and capture
///   more, but it is below `public_amount_out` — quoting less than the public market is ruled out,
///   so the gas difference stays with the user.
///
/// If the best exclusive-access candidate reaches `public_net` and produces at
/// least the committed amount, this returns a new list whose head is the pinned surplus quote,
/// followed by every public candidate as price-guard fallbacks. The pinned quote is the winning
/// candidate with:
/// - `amount_out` pinned to the committed amount,
/// - `Swap::committed_amount_out` set on each exclusive leg (consumed by the encoder), and
/// - an order-level [`SurplusInfo`] attached (observability).
///
/// Otherwise `public_ranked` is returned unchanged. Either way the user is never worse off than
/// the public market — neither in quoted `amount_out` nor net of gas. A candidate that ties the
/// public reference is quoted, with zero surplus; the trade then executes on exclusive liquidity
/// at the public price.
///
/// Per-leg attribution: the route's surplus over the committed amount (`realized − committed`)
/// is deducted from the exclusive legs — each leg absorbs what it can, capped at its own output,
/// in route order. Only exclusive legs can withhold output, so the whole
/// surplus must come out of them; public branches pay out in full. If the exclusive legs cannot
/// absorb all of it, the remainder is left with the user (who then receives more than the
/// committed amount).
///
/// Exact-in orders only: the commitment, both gates, and the surplus are all denominated in
/// `amount_out`. Exact-out support would invert the logic — fixed output, commitment and surplus
/// on the input side — and needs its own treatment here.
fn combine_with_surplus(
    responses: &OrderResponses,
    pool_scopes: &HashMap<String, LiquidityScope>,
    options: &QuoteOptions,
    public_ranked: Vec<OrderQuote>,
    user_share_bps: u32,
) -> Vec<OrderQuote> {
    let committed = match public_ranked.first() {
        Some(q) if q.status() == QuoteStatus::Success => q,
        _ => return public_ranked,
    };

    // Find the best exclusive-access candidate, respecting max_gas and route shape constraints.
    let best_exclusive_access_candidate = responses
        .quotes
        .iter()
        .filter(|(pool, _)| pool_scopes.get(pool) == Some(&LiquidityScope::All))
        .filter(|(_, q)| q.status() == QuoteStatus::Success)
        .filter(|(_, q)| {
            options
                .max_gas()
                .map(|max| q.gas_estimate() <= max)
                .unwrap_or(true)
        })
        .filter(|(_, q)| has_valid_exclusive_route(q))
        .max_by(|(_, a), (_, b)| {
            a.amount_out_net_gas()
                .cmp(b.amount_out_net_gas())
        })
        .map(|(_, q)| q);

    let Some(exclusive_candidate) = best_exclusive_access_candidate else {
        return public_ranked;
    };

    // The candidate route must match the public reference net-of-gas; anything it produces on
    // top is the improvement, of which the user keeps a share (the margin).
    let public_net_amount_out = committed.amount_out_net_gas();
    if exclusive_candidate.amount_out_net_gas() < public_net_amount_out {
        return public_ranked;
    }
    let improvement = exclusive_candidate.amount_out_net_gas() - public_net_amount_out;
    let required_net_amount_out = public_net_amount_out + user_margin(&improvement, user_share_bps);

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

    // The commitment is the larger of two lower bounds: the quoted amount_out is never below
    // the public market's, and the user — who pays the exclusive route's gas — never nets less
    // than the public route would leave them plus their share of the improvement (required_net +
    // gas). The margin never exceeds the improvement, so together with the gates above the route
    // always covers the committed amount.
    let committed_amount_out =
        (&required_net_amount_out + &exclusive_gas_cost).max(public_amount_out.clone());

    // What the exclusive components capture: everything the route produces above the commitment.
    let surplus_amount = exclusive_route_amount_out - &committed_amount_out;

    // Pin the winning candidate: stamp per-leg committed amounts, pin amount_out to committed,
    // attach SurplusInfo.
    let mut surplus_quote = exclusive_candidate.clone();

    // Final output of each swap's path, walked backwards (a path's terminal output propagates
    // to its chained predecessors). Converting captured surplus into a leg's token needs the
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
        // Capture the route's surplus at the exclusive legs.
        // Each leg absorbs up to its path's capacity; any remainder is left with the user.
        let mut surplus = exclusive_route_amount_out - &committed_amount_out;
        for (i, swap) in route.swaps_mut().iter_mut().enumerate() {
            if is_exclusive(swap.protocol_component()) {
                let Some(path_final_out) = path_final_outs.get(i) else {
                    continue;
                };
                let captured = surplus
                    .clone()
                    .min(path_final_out.clone());
                // Convert into the leg's own token, rounding down so any error is taken
                // from the protocol, not the user. Today the leg is always the last hop of
                // its path (validator), so path_final_out equals the leg's output and this
                // divides by itself — a plain subtraction. Once mid-path legs are allowed,
                // the "user gets at least the committed amount" guarantee also requires the
                // components after the leg to have diminishing returns.
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
                surplus -= captured;
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

/// Returns `true` only for routes carrying exactly one exclusive leg that is the terminal leg of
/// its path.
///
/// Returns `false` for routes with no exclusive leg, more than one exclusive leg, an empty
/// route, no route at all, or an exclusive leg that sits mid-path.
///
/// Both constraints are v1 restrictions that keep per-leg surplus attribution exact and
/// unambiguous: a mid-route leg would need inverse simulation to convert the surplus into its
/// token, and multiple exclusive legs make the per-component attribution non-unique. Both are
/// deferred to a future version. Terminal is defined as producing the route's overall output
/// token (`Route::output_token`).
fn has_valid_exclusive_route(quote: &OrderQuote) -> bool {
    let Some(route) = quote.route() else {
        return false;
    };

    let swaps = route.swaps();
    if swaps.is_empty() {
        return false;
    }

    let Some(output_token) = route.output_token() else {
        return false;
    };

    let mut exclusive_count = 0;

    for swap in swaps {
        if !is_exclusive(swap.protocol_component()) {
            continue;
        }

        if *swap.token_out() != output_token {
            return false;
        }
        exclusive_count += 1;
    }

    exclusive_count == 1
}

/// Shared-graph facts beat path-specific reasons (most specific first:
/// amount-too-small, no-scorable-paths, no-graph-path), which beat liquidity,
/// data, algorithm, and infrastructure errors; the first error wins within a
/// tier.
fn aggregate_no_route_cause(failed_solvers: &[(String, SolveError)]) -> Option<SolveError> {
    failed_solvers
        .iter()
        .map(|(_, error)| error)
        .min_by_key(|error| cause_tier(error))
        .cloned()
}

fn cause_tier(error: &SolveError) -> u8 {
    use crate::algorithm::NoPathReason;
    match error {
        SolveError::NoRouteFound { reason: Some(reason), .. } => match reason {
            NoPathReason::SourceTokenNotInGraph | NoPathReason::DestinationTokenNotInGraph => 0,
            NoPathReason::AmountTooSmall => 1,
            NoPathReason::NoScorablePaths => 2,
            NoPathReason::NoGraphPath => 3,
        },
        SolveError::InsufficientLiquidity { .. } | SolveError::MaxGasExceeded => 2,
        SolveError::MissingData(_) |
        SolveError::MarketDataStale { .. } |
        SolveError::ComputationFailed(_) |
        SolveError::NotReady(_) => 3,
        SolveError::SimulationFailed(_) | SolveError::AlgorithmError(_) => 4,
        SolveError::Timeout { .. } |
        SolveError::QueueFull |
        SolveError::Internal(_) |
        SolveError::InvalidOrder(_) |
        SolveError::FailedEncoding(_) |
        SolveError::EncodingUnavailable(_) |
        SolveError::PriceCheckFailed { .. } => 5,
        SolveError::NoRouteFound { reason: None, .. } => 6,
    }
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
        feed::exclusivity::mark_exclusive,
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
    async fn test_router_early_return_scope_gating(
        #[case] public_delay_ms: u64,
        #[case] exclusive_delay_ms: Option<u64>,
        #[case] expect_surplus: bool,
    ) {
        let (public_pool, public_worker) =
            create_mock_pool("public_pool", Ok(make_single_quote(800)), public_delay_ms);
        let exclusive_response = match exclusive_delay_ms {
            Some(_) => Ok(make_exclusive_quote(1100)),
            None => {
                Err(SolveError::NoRouteFound { order_id: "test-order".to_string(), reason: None })
            }
        };
        let public_pool = public_pool.with_liquidity_scope(LiquidityScope::PublicOnly);
        let (exclusive_pool, exclusive_worker) =
            create_mock_pool("exclusive_pool", exclusive_response, exclusive_delay_ms.unwrap_or(0));
        let exclusive_pool = exclusive_pool.with_liquidity_scope(LiquidityScope::All);

        let config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(2000))
            .with_min_responses(1);
        let worker_router =
            WorkerPoolRouter::new(vec![public_pool, exclusive_pool], config, default_encoder());
        let request = QuoteRequest::new(vec![make_order()], QuoteOptions::default());

        let start = Instant::now();
        let result = worker_router
            .quote(request)
            .await
            .expect("quote should succeed");
        let elapsed = start.elapsed();

        // Well under the 2s timeout: the gate releases as soon as both scopes have responded.
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
    fn test_aggregate_cause_surfaces_amount_too_small() {
        use crate::algorithm::NoPathReason;
        let failed = vec![(
            "bf".to_string(),
            SolveError::no_route_found_with_reason("o1", NoPathReason::AmountTooSmall),
        )];
        assert!(matches!(
            aggregate_no_route_cause(&failed),
            Some(SolveError::NoRouteFound { reason: Some(NoPathReason::AmountTooSmall), .. })
        ));
    }

    #[test]
    fn test_aggregate_no_route_cause_independent_of_pool_order() {
        use crate::algorithm::NoPathReason;
        let dust = || SolveError::no_route_found_with_reason("o1", NoPathReason::AmountTooSmall);
        let no_path = || SolveError::no_route_found_with_reason("o1", NoPathReason::NoGraphPath);
        let forward = vec![("bf1".to_string(), no_path()), ("bf3".to_string(), dust())];
        let reversed = vec![("bf3".to_string(), dust()), ("bf1".to_string(), no_path())];
        for failed in [forward, reversed] {
            assert!(matches!(
                aggregate_no_route_cause(&failed),
                Some(SolveError::NoRouteFound { reason: Some(NoPathReason::AmountTooSmall), .. })
            ));
        }
    }

    #[test]
    fn test_aggregate_token_not_in_graph_wins_over_amount_too_small() {
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
        assert!(matches!(
            aggregate_no_route_cause(&failed),
            Some(SolveError::NoRouteFound {
                reason: Some(NoPathReason::DestinationTokenNotInGraph),
                ..
            })
        ));
    }

    #[test]
    fn test_aggregate_prefers_liquidity_over_infra() {
        let failed = vec![
            ("a".to_string(), SolveError::QueueFull),
            ("b".to_string(), SolveError::insufficient_liquidity(1u32.into(), 0u32.into())),
        ];
        assert!(matches!(
            aggregate_no_route_cause(&failed),
            Some(SolveError::InsufficientLiquidity { .. })
        ));
    }

    #[test]
    fn test_rank_quotes_max_gas_filtered_sets_max_gas_exceeded() {
        let responses = OrderResponses {
            order_id: "o1".to_string(),
            quotes: vec![(
                "pool".to_string(),
                OrderQuote::new(
                    "o1".to_string(),
                    QuoteStatus::Success,
                    BigUint::from(1_000u64),
                    BigUint::from(990u64),
                    BigUint::from(100_000u64),
                    BigUint::from(990u64),
                    BlockInfo::new(1, "0xabc".to_string(), 0),
                    "test".to_string(),
                    Bytes::default(),
                    Bytes::default(),
                    "1".to_string(),
                ),
            )],
            failed_solvers: vec![],
        };
        let options = QuoteOptions::default().with_max_gas(BigUint::from(1u64));
        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &options);
        assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
        assert!(matches!(result[0].no_route_cause(), Some(SolveError::MaxGasExceeded)));
    }

    #[test]
    fn test_rank_quotes_no_cause_when_gas_within_max() {
        let responses = OrderResponses {
            order_id: "o1".to_string(),
            quotes: vec![(
                "pool".to_string(),
                OrderQuote::new(
                    "o1".to_string(),
                    QuoteStatus::NoRouteFound,
                    BigUint::from(1_000u64),
                    BigUint::from(990u64),
                    BigUint::from(100u64),
                    BigUint::from(990u64),
                    BlockInfo::new(1, "0xabc".to_string(), 0),
                    "test".to_string(),
                    Bytes::default(),
                    Bytes::default(),
                    "1".to_string(),
                ),
            )],
            failed_solvers: vec![],
        };
        let options = QuoteOptions::default().with_max_gas(BigUint::from(1_000u64));
        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &options);
        assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
        assert!(
            result[0].no_route_cause().is_none(),
            "gas within max_gas must not be labelled MaxGasExceeded: {:?}",
            result[0].no_route_cause()
        );
    }

    #[test]
    fn test_all_timeout_fallback_carries_timeout_cause() {
        let responses = OrderResponses {
            order_id: "o1".to_string(),
            quotes: vec![],
            failed_solvers: vec![("pool".to_string(), SolveError::Timeout { elapsed_ms: 9 })],
        };
        let worker_router =
            WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), default_encoder());
        let result = worker_router.rank_quotes(&responses, &QuoteOptions::default());
        assert_eq!(result[0].status(), QuoteStatus::Timeout);
        assert!(matches!(result[0].no_route_cause(), Some(SolveError::Timeout { .. })));
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
        mark_exclusive(&mut comp);
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

    /// Split route with two parallel branches (same token pair): a public component producing
    /// `public_leg_out` and an exclusive component producing `exclusive_leg_out`. Route output is
    /// the sum of the branches; zero gas cost.
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
        mark_exclusive(&mut exclusive_comp);
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

    fn exclusive_access_pool_scopes() -> HashMap<String, LiquidityScope> {
        HashMap::from([
            ("public_pool".to_string(), LiquidityScope::PublicOnly),
            ("exclusive_access_pool".to_string(), LiquidityScope::All),
        ])
    }

    /// Head selection across the gate case matrix. Each route is given as `(gross, net)`, followed
    /// by the user's share of the improvement in bps; expected is
    /// `(head amount_out, head net, captured surplus)`. A surplus win prepends the pinned quote to
    /// the public fallbacks; otherwise the public ranking is returned unchanged.
    #[rstest]
    // A tenth of the 50 improvement goes to the user, the rest is captured.
    #[case::exclusive_beats_public((900, 900), (950, 950), 1_000, (905, 905, Some(45)))]
    #[case::exclusive_below_public((950, 950), (900, 900), 1_000, (950, 950, None))]
    // A tie is quoted at the public price, with nothing to capture.
    #[case::exclusive_ties_public_net((900, 900), (900, 900), 1_000, (900, 900, Some(0)))]
    // An improvement of 1 rounds the user's tenth up to the whole unit, leaving zero surplus.
    #[case::exclusive_marginally_better((999, 999), (1000, 1000), 1_000, (1000, 1000, Some(0)))]
    #[case::exclusive_cannot_cover_public_gross((1000, 950), (990, 980), 1_000, (1000, 950, None))]
    // Gas-heavier exclusive route: the committed amount rises to max(1000, 951 + 140) = 1091 so
    // the user nets the public 950 plus a tenth of the 10 improvement; the protocol captures 9.
    #[case::exclusive_with_higher_gas((1000, 950), (1100, 960), 1_000, (1091, 951, Some(9)))]
    // Gas-cheaper exclusive route: the improvement (110) includes the gas saving, so the user
    // nets 950 + 11 = 961 and the protocol captures 1100 - (961 + 40) = 99.
    #[case::exclusive_with_lower_gas((1000, 950), (1100, 1060), 1_000, (1001, 961, Some(99)))]
    // A 50 improvement on a 1_000_000 trade: the user's 5 is 0.05 bps of the trade, below what a
    // mark-up on the trade itself could express.
    #[case::sub_bps_markup((1_000_000, 1_000_000), (1_000_050, 1_000_050), 1_000,
        (1_000_005, 1_000_005, Some(45)))]
    // Zero share: the user is left at the public net and the protocol captures the improvement.
    #[case::zero_share((900, 900), (950, 950), 0, (900, 900, Some(50)))]
    // Full share: the whole improvement goes to the user and there is nothing left to capture.
    #[case::full_share((900, 900), (950, 950), 10_000, (950, 950, Some(0)))]
    fn test_combine_head_selection(
        #[case] public: (u64, u64),
        #[case] exclusive: (u64, u64),
        #[case] user_share_bps: u32,
        #[case] expected: (u64, u64, Option<u64>),
    ) {
        let (public_out, public_net) = public;
        let (exclusive_out, exclusive_net) = exclusive;
        let (expected_amount_out, expected_net, expected_surplus) = expected;

        let responses = responses_with_gas(public_out, public_net, exclusive_out, exclusive_net);
        let public_ranked = vec![make_public_quote_with_net(public_out, public_net)
            .order()
            .clone()];
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_scopes(),
            &QuoteOptions::default(),
            public_ranked,
            user_share_bps,
        );

        let expected_surplus = expected_surplus.map(BigUint::from);
        assert_eq!(combined.len(), if expected_surplus.is_some() { 2 } else { 1 });
        assert_eq!(*combined[0].amount_out(), BigUint::from(expected_amount_out));
        assert_eq!(*combined[0].amount_out_net_gas(), BigUint::from(expected_net));
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
    fn test_combine_filters_exclusive_candidate(
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
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_scopes(),
            &options,
            public_ranked,
            1_000,
        );

        assert_eq!(combined.len(), 1);
        assert_eq!(*combined[0].amount_out(), BigUint::from(900u64));
        assert_eq!(combined[0].surplus_amount(), None);
    }

    #[test]
    fn test_combine_stamps_per_leg_committed_amount_out() {
        let responses = exclusive_access_responses(900, 1000);
        let public_ranked = vec![make_public_quote_zero_gas(900)
            .order()
            .clone()];
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_scopes(),
            &QuoteOptions::default(),
            public_ranked,
            1_000,
        );

        let surplus_quote = &combined[0];
        let route = surplus_quote
            .route()
            .expect("surplus quote should have a route");
        let perm_swap = route
            .swaps()
            .iter()
            .find(|s| is_exclusive(s.protocol_component()))
            .expect("should have an exclusive swap");

        // committed_leg = leg.amount_out * committed_route_out / realized_route_out
        // = 1000 * 910 / 1000 = 910
        assert_eq!(perm_swap.committed_amount_out(), Some(&BigUint::from(910u64)),);
    }

    #[test]
    fn test_combine_committed_leg_deduction() {
        // leg = 995, committed = 910, realized = 1000: the route's surplus (90) is deducted from
        // the exclusive leg in full — committed_leg = 995 − 90 = 905, exactly, no rounding.
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
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_scopes(),
            &QuoteOptions::default(),
            public_ranked,
            1_000,
        );

        let route = combined[0]
            .route()
            .expect("surplus quote should have a route");
        let perm_swap = route
            .swaps()
            .iter()
            .find(|s| is_exclusive(s.protocol_component()))
            .expect("should have an exclusive swap");
        assert_eq!(perm_swap.committed_amount_out(), Some(&BigUint::from(905u64)));
    }

    #[rstest]
    #[case::zero("0", Some(0))]
    #[case::whole_improvement("10000", Some(10_000))]
    #[case::padded(" 2500 ", Some(2_500))]
    #[case::above_whole_improvement("10001", None)]
    #[case::negative("-1", None)]
    #[case::fractional("0.5", None)]
    #[case::empty("", None)]
    fn test_parse_user_improvement_share_bps(#[case] raw: &str, #[case] expected: Option<u32>) {
        assert_eq!(parse_user_improvement_share_bps(raw), expected);
    }

    #[test]
    fn test_combine_split_route_attribution() {
        // Split route: public branch 600 + exclusive branch 500 = 1100 realized vs 1010
        // committed (zero gas, a tenth of the 100 improvement to the user). Only the exclusive
        // leg is stamped; the public branch flows to the user untouched.
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
        let combined = combine_with_surplus(
            &responses,
            &exclusive_access_pool_scopes(),
            &QuoteOptions::default(),
            public_ranked,
            1_000,
        );

        assert_eq!(*combined[0].amount_out(), BigUint::from(1010u64));
        let route = combined[0]
            .route()
            .expect("surplus quote should have a route");
        let public_leg = route
            .swaps()
            .iter()
            .find(|s| !is_exclusive(s.protocol_component()))
            .expect("should have a public swap");
        assert_eq!(public_leg.committed_amount_out(), None);

        // The public branch (600) pays out in full, so the entire surplus
        // (1100 − 1010 = 90) is deducted from the exclusive leg: committed_leg = 500 − 90 =
        // 410. The user receives 600 + 410 = 1010 (exactly the committed amount) and the
        // exclusive component captures all 90.
        let exclusive_leg = route
            .swaps()
            .iter()
            .find(|s| is_exclusive(s.protocol_component()))
            .expect("should have an exclusive swap");
        assert_eq!(exclusive_leg.committed_amount_out(), Some(&BigUint::from(410u64)));
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
            if *protocol_system == "vm:exclusive" {
                mark_exclusive(&mut comp);
            }
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
    // Two exclusive legs: out of scope for v1 (ambiguous per-component attribution).
    #[case::two_exclusive_legs(
        &[("vm:exclusive", 0x01, 0x02), ("vm:exclusive", 0x01, 0x02)], false)]
    // Diamond split: 0x01 splits into 0x01->0x02 (exclusive) and 0x01->0x03, both merging into
    // 0x02->0x04 and 0x03->0x04. The exclusive leg feeds the merge point, not the route's output
    // (0x04), so it's mid-path even though the next serialized swap starts a sibling branch.
    #[case::exclusive_leg_feeding_diamond_merge(
        &[
            ("vm:exclusive", 0x01, 0x02),
            ("uniswap_v2", 0x01, 0x03),
            ("uniswap_v2", 0x02, 0x04),
            ("uniswap_v2", 0x03, 0x04),
        ],
        false)]
    // Same diamond shape, reordered so the exclusive leg's real continuation is adjacent to it —
    // regression case for a prior bug where terminal-ness was inferred from adjacency alone.
    #[case::exclusive_leg_feeding_diamond_merge_reordered(
        &[
            ("vm:exclusive", 0x01, 0x02),
            ("uniswap_v2", 0x02, 0x04),
            ("uniswap_v2", 0x01, 0x03),
            ("uniswap_v2", 0x03, 0x04),
        ],
        false)]
    // Sibling branches sharing a prefix: 0x01->0x02->0x03->0x04 alongside
    // 0x01->0x02->0x05->0x04(exclusive). The exclusive leg is the terminal hop of its own branch
    // and produces the route's output token, so it's valid despite sharing 0x01->0x02 with the
    // other branch.
    #[case::exclusive_leg_on_sibling_branch(
        &[
            ("uniswap_v2", 0x01, 0x02),
            ("uniswap_v2", 0x02, 0x03),
            ("uniswap_v2", 0x03, 0x04),
            ("uniswap_v2", 0x02, 0x05),
            ("vm:exclusive", 0x05, 0x04),
        ],
        true)]
    fn test_exclusive_route_validation(#[case] legs: &[(&str, u8, u8)], #[case] expected: bool) {
        let quote = make_route_quote(legs);
        assert_eq!(has_valid_exclusive_route(&quote), expected);
    }
}
