//! A Solver Worker that processes solve requests and maintains market graph state.
//!
//! The Solver Worker:
//! - Initializes graph from market topology (via a GraphManager)
//! - Consumes MarketEvents to keep local topology in sync
//! - Processes solve requests
//! - Uses an Algorithm to find routes through the market graph
//! - Coordinates market event and solve task processing

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use num_bigint::BigUint;
use tokio::sync::{broadcast, Notify};
use tracing::{debug, error, info, warn};
use tycho_simulation::{tycho_common::models::protocol::ProtocolComponent, tycho_core::Bytes};

use crate::{
    algorithm::{request::SolveRequest, Algorithm},
    derived::{
        computation::ComputationRequirements, events::DerivedDataEvent, tracker::ReadinessTracker,
        SharedDerivedDataRef,
    },
    feed::{
        component_filter::{
            filter_event, is_excluded_protocol, protocol_matches, remove_components,
        },
        events::{MarketEvent, MarketEventHandler},
        exclusivity::is_exclusive,
        market_data::{MarketData, MarketDataView, StateLabel},
    },
    graph::{EdgeWeightUpdaterWithDerived, GraphManager},
    propamm_fallback::{
        fallback_amount_out, has_pamm_leg, manager::PammManager, FallbackAmountOut,
        FallbackPoolIndex, FeeTiers, SharedFeeTiers, FALLBACK_PROTOCOL_SYSTEM,
        PROPAMM_FALLBACK_PREFIX,
    },
    types::{
        internal::{RouteRejection, SolveTask},
        ComponentId, Route, RouteExclusionFilter, RouteExclusions,
    },
    worker_pool_router::LiquidityScope,
    BlockInfo, Order, OrderQuote, QuoteStatus, SingleOrderQuote, SolveError, SolveParams,
};

/// Whether a worker with this scope and exclusion list must keep `component` out of its graph: an
/// exclusive component in a `PublicOnly` worker pool, or a component of an excluded protocol
/// system.
///
/// Holds for the life of the worker, which is what lets it filter state updates and removals as
/// well as additions. The pAMM rule in [`PammManager`] does not, and is applied beside this one.
///
/// A free function so [`PammManager`] can take it as the caller's half of the rule without
/// borrowing the whole worker.
fn should_drop_component(
    liquidity_scope: LiquidityScope,
    exclude_protocols: &[String],
    component: &ProtocolComponent,
) -> bool {
    (liquidity_scope == LiquidityScope::PublicOnly && is_exclusive(component)) ||
        is_excluded_protocol(exclude_protocols, component)
}

/// The exclusions this request resolves to against `view`, resolved once and shared.
///
/// Every order of one request, in every pool it reaches, wants the same answer, and expanding a
/// protocol system copies that system's whole component set. The market's component membership is
/// what the answer depends on, so a cached one stands until a component is added or removed.
///
/// Logs what a non-empty filter resolved to, on the one call per generation that resolves it.
fn resolve_exclusions(view: &MarketDataView<'_>, params: &SolveParams) -> Arc<RouteExclusions> {
    let generation = view
        .base_market_state()
        .component_generation();
    let mut cache = params
        .cached_exclusions()
        .lock()
        .expect("route exclusion cache lock poisoned");
    if let Some((cached_generation, exclusions)) = cache.as_ref() {
        if *cached_generation == generation {
            return exclusions.clone();
        }
    }
    let filter = params.route_filter();
    let exclusions = Arc::new(
        view.base_market_state()
            .resolve_route_filter(filter),
    );
    if !filter.is_empty() {
        debug!(
            component_generation = generation,
            named_pools = filter.excluded_pools().len(),
            named_protocols = filter.excluded_protocols().len(),
            excluded_pools = exclusions.pools.len(),
            excluded_tokens = exclusions.tokens.len(),
            "resolved a request's route filter"
        );
    }
    *cache = Some((generation, exclusions.clone()));
    exclusions
}

/// The Uniswap V3 pool a pAMM leg would fall back to and the request excludes, if there is one.
///
/// A pAMM leg clears the filter under its own component id, then settles on chain through the
/// fallback pool its fee tier selects. That pool is what the caller sees traded, so the filter has
/// to reach it too.
fn excluded_fallback_pool<'a>(
    route: &Route,
    fee_tiers: &FeeTiers,
    fallback_pools: &'a FallbackPoolIndex,
    filter: &RouteExclusionFilter,
) -> Option<&'a ComponentId> {
    let protocol_excluded = filter
        .excluded_protocols()
        .iter()
        .any(|entry| protocol_matches(entry, FALLBACK_PROTOCOL_SYSTEM));
    for swap in route.swaps() {
        if !swap
            .protocol()
            .starts_with(PROPAMM_FALLBACK_PREFIX)
        {
            continue;
        }
        let tier = fee_tiers.resolved_tier(swap.token_in(), swap.token_out());
        let Some(pool) = fallback_pools.pool_for(swap.token_in(), swap.token_out(), tier) else {
            continue;
        };
        if protocol_excluded || filter.excluded_pools().contains(pool) {
            return Some(pool);
        }
    }
    None
}

/// Check every leg, including split branches, against the original request filter. Reading
/// protocol systems from the returned components also covers pools added after exclusions were
/// resolved. The order's endpoints remain allowed by intermediate-token exclusions.
fn validate_route_filter(
    route: &Route,
    order: &Order,
    filter: &RouteExclusionFilter,
) -> Result<(), String> {
    if filter.is_empty() {
        return Ok(());
    }
    for swap in route.swaps() {
        if filter
            .excluded_pools()
            .contains(swap.component_id())
        {
            return Err(format!("route uses excluded pool {}", swap.component_id()));
        }
        let protocol = &swap
            .protocol_component()
            .protocol_system;
        if filter
            .excluded_protocols()
            .iter()
            .any(|entry| protocol_matches(entry, protocol))
        {
            return Err(format!("route uses excluded protocol {protocol}"));
        }
        for token in [swap.token_in(), swap.token_out()] {
            if token != order.token_in() &&
                token != order.token_out() &&
                filter.excluded_tokens().contains(token)
            {
                return Err(format!("route uses excluded intermediate token {token}"));
            }
        }
    }
    Ok(())
}

/// Records per-worker-pool queue metrics at task pickup: how long the task waited in the
/// queue and the depth left behind it. Queue wait growing while solve time stays
/// flat is the leading indicator of worker saturation.
fn record_task_pickup_metrics(pool_name: &str, queue_wait: Duration, queue_depth: usize) {
    metrics::histogram!("worker_pool_queue_wait_seconds", "pool" => pool_name.to_string())
        .record(queue_wait.as_secs_f64());
    metrics::gauge!("worker_pool_queue_depth", "pool" => pool_name.to_string())
        .set(queue_depth as f64);
}

/// Records successful worker-side quote latency after pickup, excluding queue wait: everything
/// from taking the order to handing the quote back, so the algorithm's solve plus route
/// validation, pAMM fallback pricing, price-impact calculation and quote construction. Unlike
/// `worker_router_solve_duration_seconds`, which times the router racing every pool and so
/// belongs to no single pool, this is attributable per pool.
///
/// Successful quotes only: a pool that exhausts its timeout returns before this point and is
/// counted in `worker_router_solver_failures_total{error_type="timeout"}` instead.
fn record_quote_duration(pool_name: &str, quote_duration: Duration) {
    // The metric keeps its established external name for dashboard compatibility.
    metrics::histogram!("worker_pool_solve_duration_seconds", "pool" => pool_name.to_string())
        .record(quote_duration.as_secs_f64());
}

/// Records end-to-end price-impact calculation time, including protocol-specific spot-price
/// probes, and how the calculation ended: `computed`, or the reason `price_impact_bps` was left
/// off the quote.
fn record_price_impact_metrics(pool_name: &str, duration: Duration, outcome: &'static str) {
    metrics::histogram!(
        "worker_pool_price_impact_duration_seconds",
        "pool" => pool_name.to_string()
    )
    .record(duration.as_secs_f64());
    metrics::counter!(
        "worker_pool_price_impact_calculations_total",
        "pool" => pool_name.to_string(),
        "outcome" => outcome
    )
    .increment(1);
}

/// A solver worker instance that maintains a market graph and processes solve requests.
pub(crate) struct SolverWorker<A>
where
    A: Algorithm,
    A::GraphManager: MarketEventHandler,
{
    /// Algorithm used for route finding.
    algorithm: A,
    /// Graph manager that maintains the graph.
    graph_manager: A::GraphManager,
    /// Reference to shared market data.
    market_data: MarketData,
    /// Reference to shared derived data (component depths, token prices).
    derived_data: SharedDerivedDataRef,
    /// Algorithm's computation requirements (which derived data to react to).
    requirements: ComputationRequirements,
    /// Tracks readiness of required derived data computations.
    readiness_tracker: ReadinessTracker,
    /// Notified when readiness state may have changed.
    ready_notify: Arc<Notify>,
    /// Whether the graph has been initialized.
    initialized: bool,
    /// Which pAMM components this worker's graph may hold, and the market facts that decide it.
    pamm_admission: PammManager,
    /// Worker identifier (for logging).
    worker_id: usize,
    /// Worker pool name (used as the `pool` metric label).
    pool_name: String,
    /// Which liquidity this worker ingests.
    liquidity_scope: LiquidityScope,
    /// Protocol systems this worker never routes through. Empty for every worker pool that does
    /// not set `exclude_protocols`.
    exclude_protocols: Vec<String>,
}

impl<A> SolverWorker<A>
where
    A: Algorithm,
    A::GraphManager: MarketEventHandler,
{
    /// Creates a new Solver.
    ///
    /// The graph manager is automatically created from the algorithm's associated type.
    ///
    /// # Arguments
    ///
    /// * `market_data` - Shared reference to market data
    /// * `derived_data` - Shared reference to derived data (component depths, token prices)
    /// * `algorithm` - The algorithm to use for route finding
    /// * `worker_id` - Identifier for this worker (for logging)
    /// * `pool_name` - Worker pool name (used as the `pool` metric label)
    pub fn new(
        market_data: MarketData,
        derived_data: SharedDerivedDataRef,
        algorithm: A,
        worker_id: usize,
        pool_name: String,
    ) -> Self {
        let requirements = algorithm.computation_requirements();
        Self {
            algorithm,
            graph_manager: A::GraphManager::default(),
            market_data,
            derived_data,
            requirements: requirements.clone(),
            readiness_tracker: ReadinessTracker::new(requirements),
            ready_notify: Arc::new(Notify::new()),
            initialized: false,
            pamm_admission: PammManager::new(pool_name.clone()),
            worker_id,
            pool_name,
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
        }
    }

    /// Sets the fee tiers used to locate a pAMM leg's Uniswap V3 fallback pool.
    pub(crate) fn with_fallback_fee_tiers(mut self, fallback_fee_tiers: SharedFeeTiers) -> Self {
        self.pamm_admission = self
            .pamm_admission
            .with_fee_tiers(fallback_fee_tiers);
        self
    }

    /// Sets which liquidity this worker ingests.
    pub(crate) fn with_liquidity_scope(mut self, scope: LiquidityScope) -> Self {
        self.liquidity_scope = scope;
        self
    }

    /// Sets the protocol systems this worker never routes through.
    pub(crate) fn with_exclude_protocols(mut self, exclude_protocols: Vec<String>) -> Self {
        self.exclude_protocols = exclude_protocols;
        self
    }

    /// A read view of the market the solve runs against: the overlay `label` names, else the live
    /// base state.
    ///
    /// # Errors
    ///
    /// [`SolveError::NotReady`] when `label` names no registered overlay.
    async fn read_market(
        &self,
        label: Option<&StateLabel>,
    ) -> Result<MarketDataView<'_>, SolveError> {
        match label {
            Some(label) => self
                .market_data
                .read_labeled(label)
                .await
                .map_err(|e| SolveError::NotReady(e.to_string())),
            None => Ok(self.market_data.read().await),
        }
    }

    /// The error for a route the algorithm returned with no swaps to read.
    ///
    /// `Route::validate` rejects an empty route before the quote reads it, so a route that reaches
    /// this point holds swaps. It is a fault in the algorithm rather than a fact about the market,
    /// which is why it is not a `NoPathReason`.
    fn route_carries_no_swaps(&self) -> SolveError {
        SolveError::AlgorithmError(format!(
            "{} returned a route with no swaps",
            self.algorithm.name()
        ))
    }

    /// Builds the graph from the market topology, and the pAMM state the event path reads from it.
    ///
    /// Call this on startup or to recreate the graph from the latest market topology. It resets
    /// four things together, from one read: the graph, the fallback pool index, which pAMMs the
    /// graph holds and which it left out, and the fee tiers all of that was decided with.
    pub async fn initialize_graph(&mut self) {
        let topology = {
            // One read: the index and the topology must describe the same market, or a pAMM whose
            // fallback pool arrived between the two reads stays out until the next rebuild.
            let market = self.market_data.read().await;
            // Owned already, so the graph is built without holding the market lock.
            let topology = market.component_topology();
            let Self { pamm_admission, liquidity_scope, exclude_protocols, .. } = self;
            let caller_drops = |component: &ProtocolComponent| {
                should_drop_component(*liquidity_scope, exclude_protocols, component)
            };
            let withheld_pamms =
                pamm_admission.withhold_from_graph(&market, &topology, &caller_drops);
            remove_components(market.base_market_state(), topology, &|component| {
                caller_drops(component) || withheld_pamms.contains(&component.id)
            })
        };

        self.graph_manager
            .initialize_graph(&topology);
        self.initialized = true;
    }

    /// Applies one market event to the graph, or rebuilds the graph when the fee tiers moved.
    ///
    /// A tier change decides which Uniswap V3 pool a pAMM falls back to, and putting a pAMM back
    /// needs the market topology, which only [`initialize_graph`](Self::initialize_graph) reads.
    /// The feed writes the market before it broadcasts, so a rebuild already reflects this event
    /// and the event is not applied on top of it. Tiers change on the fetcher's timer, not per
    /// block, so a rebuild is rare.
    pub async fn process_event(&mut self, event: MarketEvent) {
        // One read for the whole event: the fetcher writes on its own timer, and a second read
        // could decide admission against tiers the rebuild test just passed on.
        let fee_tiers = self.pamm_admission.fee_tiers();
        if self
            .pamm_admission
            .needs_rebuild(fee_tiers.as_ref())
        {
            self.initialize_graph().await;
            return;
        }

        let market_data = self.market_data.clone();
        let event = {
            let market = market_data.read().await;
            let Self { pamm_admission, liquidity_scope, exclude_protocols, .. } = self;
            let caller_drops = |component: &ProtocolComponent| {
                should_drop_component(*liquidity_scope, exclude_protocols, component)
            };
            let mut event = event;
            pamm_admission.apply_pamm_admission(
                &market,
                fee_tiers.as_ref(),
                &caller_drops,
                &mut event,
            );
            filter_event(market.base_market_state(), event, &caller_drops)
        };

        match event {
            MarketEvent::MarketUpdated { .. } => {
                if let Err(e) = self
                    .graph_manager
                    .handle_event(&event)
                    .await
                {
                    // Graph errors currently returned by handle_event are non-fatal, so we just log
                    // them.
                    warn!("Error handling market event: {:?}", e);
                }
            }
        }
    }

    /// Returns a quote for an order, optionally solved against a named state overlay.
    pub async fn quote(
        &mut self,
        order: &Order,
        params: SolveParams,
    ) -> Result<SingleOrderQuote, SolveError> {
        let start_time = Instant::now();

        // Log order details once at entry
        debug!(
            order_id = %order.id(),
            token_in = ?order.token_in(),
            token_out = ?order.token_out(),
            amount = %order.amount(),
            side = ?order.side(),
            "processing order"
        );

        // Check readiness before solving
        if self
            .readiness_tracker
            .has_requirements() &&
            !self.readiness_tracker.is_ready()
        {
            return Err(SolveError::NotReady(format!(
                "derived data not ready: missing {:?}",
                self.readiness_tracker.missing()
            )));
        }

        // Ensure we're initialized
        if !self.initialized {
            self.initialize_graph().await;
        }

        // Get the graph from the graph manager
        let graph = self.graph_manager.graph();

        // Get block info and resolve the effective state label.
        // TODO: maybe the algorithm should return the block info with the route? The block might
        // update while solving and the route returned might be for the newer block.
        let (block_info, solved_against, exclusions) = {
            // Read briefly to capture block info; drop the lock before solving so it is not held
            // across the algorithm's own read call.
            let view = self
                .read_market(params.state_label())
                .await?;
            let last_block = view
                .last_updated()
                .ok_or(SolveError::NotReady("No block info".to_string()))?;
            let block_info = BlockInfo::new(
                last_block.number(),
                last_block.hash().to_string(),
                last_block.timestamp(),
            );
            // When no overlay was requested, record the block number so callers always know which
            // state the quote was computed against.
            let solved_against = view
                .state_label()
                .cloned()
                .unwrap_or_else(|| last_block.number().to_string());
            let exclusions = resolve_exclusions(&view, &params);
            (block_info, solved_against, exclusions)
        };

        let mut request = SolveRequest::new(graph, self.market_data.clone(), order)
            .with_derived(self.derived_data.clone())
            .with_shared_exclusions(exclusions);
        if let Some(label) = params.state_label().cloned() {
            request = request.with_label(label);
        }
        let result = self
            .algorithm
            .find_best_route(request)
            .await;

        let order_quote = match result {
            Ok(result) => {
                // Extract scalar values before consuming result with into_route()
                let amount_out_net_gas = result
                    .net_amount_out()
                    .to_biguint()
                    .unwrap_or(BigUint::ZERO);
                let gas_price = result.gas_price().clone();
                let mut route = result.into_route();

                if let Err(err) = route
                    .validate()
                    .map_err(|err| err.to_string())
                    .and_then(|()| validate_route_filter(&route, order, params.route_filter()))
                {
                    error!(
                        order_id = %order.id(),
                        algorithm = self.algorithm.name(),
                        error = %err,
                        "algorithm produced an invalid route"
                    );
                    return Err(SolveError::AlgorithmError(format!(
                        "{} produced an invalid route: {err}",
                        self.algorithm.name()
                    )));
                }

                // A route with a pAMM leg needs the amount out its Uniswap V3 fallback would
                // deliver: the router checks it against `min_amount_out` before ranking and drops
                // the candidate when it falls short. A route whose fallback cannot be priced is
                // dropped here, because there is nothing to check that floor against.
                if has_pamm_leg(&route) {
                    let Some(fee_tiers) = self.pamm_admission.fee_tiers() else {
                        debug!(
                            order_id = %order.id(),
                            "dropping pAMM route: the router's fee tiers are not read yet"
                        );
                        return Err(SolveError::route_rejected(
                            order.id(),
                            RouteRejection::PammFeeTiersUnread,
                        ));
                    };
                    // The same view the algorithm solved against, so the fallback is priced on the
                    // requested overlay rather than the base state.
                    let market = self
                        .read_market(params.state_label())
                        .await?;
                    if let Some(pool) = excluded_fallback_pool(
                        &route,
                        &fee_tiers,
                        self.pamm_admission.fallback_pools(),
                        params.route_filter(),
                    ) {
                        debug!(order_id = %order.id(), fallback_pool = %pool,
                            "dropping pAMM route: request excludes its fallback pool");
                        return Err(SolveError::route_rejected(
                            order.id(),
                            RouteRejection::PammFallbackExcluded,
                        ));
                    }
                    match fallback_amount_out(
                        &route,
                        &market,
                        &fee_tiers,
                        self.pamm_admission.fallback_pools(),
                    ) {
                        FallbackAmountOut::AmountOut(amount) => {
                            route.set_fallback_amount_out(amount)
                        }
                        FallbackAmountOut::NoFallbackPool {
                            component_id,
                            fee_tier,
                            token_in,
                            token_out,
                        } => {
                            // An empty list is a pair the market holds no Uniswap V3 pool for at
                            // all, which is a different finding from holding one at a tier the
                            // router does not resolve to.
                            let tiers_in_market = self
                                .pamm_admission
                                .fallback_pools()
                                .tiers_for(&token_in, &token_out);
                            debug!(
                                order_id = %order.id(),
                                %component_id,
                                fee_tier,
                                token_in = %token_in,
                                token_out = %token_out,
                                tiers_in_market = ?tiers_in_market,
                                "dropping pAMM route: no Uniswap V3 pool at the router's fee tier"
                            );
                            return Err(SolveError::route_rejected(
                                order.id(),
                                RouteRejection::PammFallbackPoolMissing,
                            ));
                        }
                        FallbackAmountOut::NotPriceable { reason } => {
                            debug!(
                                order_id = %order.id(),
                                %reason,
                                "dropping pAMM route: the Uniswap V3 fallback could not be simulated"
                            );
                            return Err(SolveError::route_rejected(
                                order.id(),
                                RouteRejection::PammFallbackUnpriceable,
                            ));
                        }
                    }
                }

                // This is a first naive approach to getting the total gas of this quote
                // A finer estimation is done during encoding
                let gas_estimate = route.total_gas();
                let amount_in_raw = if order.is_sell() {
                    order.amount().clone()
                } else {
                    route
                        .swaps()
                        .first()
                        .map(|s| s.amount_in().clone())
                        .ok_or_else(|| {
                            error!(
                                order_id = %order.id(),
                                algorithm = self.algorithm.name(),
                                "route missing first swap for buy order"
                            );
                            self.route_carries_no_swaps()
                        })?
                };
                let amount_out_raw = if order.is_sell() {
                    let output_token = route.output_token().ok_or_else(|| {
                        error!(
                            order_id = %order.id(),
                            algorithm = self.algorithm.name(),
                            "route missing swaps for sell order"
                        );
                        self.route_carries_no_swaps()
                    })?;
                    route.amount_out(&output_token)
                } else {
                    order.amount().clone()
                };

                let price_impact_started = Instant::now();
                let price_impact_result = super::price_impact::route_price_impact(
                    &route,
                    &amount_in_raw,
                    &amount_out_raw,
                );
                let price_impact_outcome = match &price_impact_result {
                    Ok(_) => "computed",
                    Err(err) => err.outcome(),
                };
                record_price_impact_metrics(
                    &self.pool_name,
                    price_impact_started.elapsed(),
                    price_impact_outcome,
                );
                let price_impact_bps = match price_impact_result {
                    Ok(impact) => Some((impact * 10_000.0).round() as i32),
                    Err(err) => {
                        debug!(
                            order_id = %order.id(),
                            algorithm = self.algorithm.name(),
                            error = %err,
                            "price-impact calculation failed; omitting price_impact_bps from quote"
                        );
                        None
                    }
                };

                let mut quote = OrderQuote::new(
                    order.id().to_string(),
                    QuoteStatus::Success,
                    amount_in_raw,
                    amount_out_raw,
                    gas_estimate,
                    amount_out_net_gas,
                    block_info.clone(),
                    self.algorithm.name().to_string(),
                    Bytes::from(order.sender().as_ref()),
                    Bytes::from(order.effective_receiver().as_ref()),
                    solved_against,
                )
                .with_route(route)
                .with_gas_price(gas_price);
                if let Some(bps) = price_impact_bps {
                    quote = quote.with_price_impact_bps(bps);
                }
                quote
            }
            Err(err) => {
                return Err(solve_error_from_algorithm_error(order.id(), order.amount(), err))
            }
        };

        let quote_duration = start_time.elapsed();
        record_quote_duration(&self.pool_name, quote_duration);

        Ok(SingleOrderQuote::new(order_quote, quote_duration.as_millis() as u64))
    }

    /// Waits for required derived data to become ready, or until timeout.
    ///
    /// Uses a Notify pattern to know when it's available to solve.
    ///
    /// Returns `Ok(())` if ready or no requirements, `Err` if timeout reached or computation
    /// failed.
    async fn wait_until_ready(&self, timeout: Duration) -> Result<(), SolveError> {
        // Fast path: no requirements or already ready
        if !self
            .readiness_tracker
            .has_requirements() ||
            self.readiness_tracker.is_ready()
        {
            return Ok(());
        }

        let deadline = Instant::now() + timeout;

        loop {
            // Create notified future BEFORE checking state (important for race-free waiting)
            let notified = self.ready_notify.notified();

            // Check if ready
            if self.readiness_tracker.is_ready() {
                return Ok(());
            }

            // Check if blocked before waiting for a notification that may never come
            if self
                .readiness_tracker
                .is_blocked_for_current_block()
            {
                return Err(SolveError::ComputationFailed(format!(
                    "required computation failed for current block: {:?}",
                    self.readiness_tracker.missing()
                )));
            }

            // Calculate remaining time
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SolveError::NotReady(format!(
                    "timeout waiting for derived data: missing {:?}",
                    self.readiness_tracker.missing()
                )));
            }

            // Wait for notification or timeout
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    return Err(SolveError::NotReady(format!(
                        "timeout waiting for derived data: missing {:?}",
                        self.readiness_tracker.missing()
                    )));
                }
                _ = notified => {
                    // Check if any require_fresh computation permanently failed this block
                    if self.readiness_tracker.is_blocked_for_current_block() {
                        return Err(SolveError::ComputationFailed(format!(
                            "required computation failed for current block: {:?}",
                            self.readiness_tracker.missing()
                        )));
                    }
                    // Woken up by notify, loop to check readiness again
                    continue;
                }
            }
        }
    }

    /// Runs the worker's main loop, processing market events and solve tasks.
    ///
    /// This method coordinates between market events and solve requests, ensuring the graph
    /// stays up-to-date while processing solve tasks.
    ///
    /// # Arguments
    ///
    /// * `event_rx` - Receiver for market events
    /// * `derived_event_rx` - Receiver for derived data events (component depths, etc.)
    /// * `task_rx` - Shared receiver for solve tasks
    /// * `shutdown_rx` - Receiver for shutdown signals
    pub async fn run(
        &mut self,
        mut event_rx: broadcast::Receiver<MarketEvent>,
        mut derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
        task_rx: async_channel::Receiver<SolveTask>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) where
        A::GraphManager: EdgeWeightUpdaterWithDerived,
    {
        info!(self.worker_id, "worker started");

        // Once the derived-data channel closes, its recv() returns Closed instantly on every
        // call; keeping the arm in the select would turn this loop into a busy spin. The guard
        // disables the arm so the worker keeps solving with the last derived data it saw.
        let mut derived_closed = false;

        loop {
            tokio::select! {
                biased; // prioritize events in this order: shutdown, market update, derived data, solve task

                // Check for shutdown
                _ = shutdown_rx.recv() => {
                    info!(self.worker_id, "worker shutting down");
                    break;
                }

                // Process market events
                event_result = event_rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            self.process_event(event).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!(self.worker_id, "event receiver closed, shutting down");
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                self.worker_id,
                                skipped = skipped,
                                "event receiver lagged, skipped {} events. Reinitializing graph from current market state",
                                skipped
                            );
                            // Reinitialize the graph from the current market state to recover from the missed events.
                            self.initialize_graph().await;
                        }
                    }
                }

                // Process derived data events (component depths, token prices)
                derived_result = derived_event_rx.recv(), if !derived_closed => {
                    match derived_result {
                        Ok(event) => {
                            // Always update tracker with every event
                            self.readiness_tracker.handle_event(&event);

                            // Signal waiters that readiness may have changed
                            self.ready_notify.notify_waiters();

                            // Update edge weights when a relevant computation completes.
                            if let DerivedDataEvent::ComputationComplete { computation_id, block, .. } = &event {
                                if self.requirements.is_required(computation_id) {
                                    let market = self.market_data.read().await;
                                    let derived = self.derived_data.read().await;
                                    let updated = self.graph_manager.update_edge_weights_with_derived(market, &derived);
                                    debug!(
                                        self.worker_id,
                                        computation_id,
                                        block,
                                        updated,
                                        "updated edge weights with derived data"
                                    );
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            warn!(self.worker_id, "derived event receiver closed; continuing with last derived data");
                            derived_closed = true;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                self.worker_id,
                                skipped,
                                "derived event receiver lagged, skipped {} events",
                                skipped
                            );
                            // Recover by updating with whatever derived data is available.
                            let market = self.market_data.read().await;
                            let derived = self.derived_data.read().await;
                            let updated = self.graph_manager.update_edge_weights_with_derived(market, &derived);
                            debug!(
                                self.worker_id,
                                updated,
                                "recovered edge weights after lag"
                            );
                        }
                    }
                }

                // Get next solve task
                task = task_rx.recv() => {
                    match task.ok() {
                        Some(task) => {
                            let task_id = task.id();
                            record_task_pickup_metrics(
                                &self.pool_name,
                                task.wait_time(),
                                task_rx.len(),
                            );

                            // Wait for derived data readiness before solving
                            // Use algorithm timeout as the max wait time
                            if let Err(e) = self.wait_until_ready(self.algorithm.timeout()).await {
                                warn!(
                                    self.worker_id,
                                    task_id = %task_id,
                                    error = %e,
                                    "not ready to solve"
                                );
                                task.respond(Err(e));
                                continue;
                            }

                            // Process the task
                            let result = {
                                let params = task.params().clone();
                                let order = task.order();
                                self.quote(order, params).await
                            };

                            // Send response. The specific failure cause is already logged in
                            // `quote()` and returned to the caller, so we don't re-log here.
                            task.respond(result);
                        }
                        None => {
                            // Channel closed, exit
                            info!(self.worker_id, "task channel closed, exiting");
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Maps an [`AlgorithmError`](crate::AlgorithmError) to the [`SolveError`] class
/// reported upstream, logging at a severity matching the failure class.
///
/// `amount_in` seeds `InsufficientLiquidity::required`; the algorithm variant
/// carries no amounts, so `available` is reported as zero (= not reported).
fn solve_error_from_algorithm_error(
    order_id: &str,
    amount_in: &BigUint,
    err: crate::AlgorithmError,
) -> SolveError {
    match err {
        crate::AlgorithmError::NoPath { reason, .. } => {
            debug!(order_id = %order_id, error = %err, "no route found");
            SolveError::no_route_found_with_reason(order_id, reason)
        }
        crate::AlgorithmError::Timeout { elapsed_ms } => {
            warn!(order_id = %order_id, elapsed_ms, "solve timeout");
            SolveError::Timeout { elapsed_ms }
        }
        crate::AlgorithmError::InsufficientLiquidity => {
            debug!(order_id = %order_id, "insufficient liquidity on all paths");
            SolveError::insufficient_liquidity(amount_in.clone(), BigUint::ZERO)
        }
        crate::AlgorithmError::DataNotFound { kind, id } => {
            warn!(order_id = %order_id, kind, id = ?id, "required data not found");
            SolveError::MissingData(match id {
                Some(id) => format!("{kind}: {id}"),
                None => kind.to_string(),
            })
        }
        crate::AlgorithmError::SimulationFailed { component_id, error } => {
            warn!(order_id = %order_id, %component_id, %error, "simulation failed");
            SolveError::SimulationFailed(format!("{component_id}: {error}"))
        }
        crate::AlgorithmError::InvalidConfiguration { .. } |
        crate::AlgorithmError::ExactOutNotSupported |
        crate::AlgorithmError::Other(_) => {
            error!(order_id = %order_id, error = %err, "algorithm error");
            SolveError::AlgorithmError(err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::{
        algorithm::{
            most_liquid::DepthAndPrice,
            test_utils::{
                component, component_with_protocol, order, setup_market_weighted, token,
                MockProtocolSim,
            },
        },
        derived::{
            computation::DerivedComputation,
            computations::{SpotPriceComputation, TokenGasPriceComputation},
            DerivedData,
        },
        graph::petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
        propamm_fallback::{
            manager::PammState, FeeTiers, FALLBACK_PROTOCOL_SYSTEM, FEE_ATTRIBUTE,
            PROPAMM_FALLBACK_PREFIX,
        },
        types::{ComponentId, OrderSide, Route, RouteExclusionFilter, RouteResult, Swap},
        AlgorithmError,
    };

    /// A minimal mock algorithm for testing the worker.
    /// Uses DepthAndPrice as the edge weight type to satisfy trait bounds.
    struct MockAlgorithm {
        requirements: ComputationRequirements,
        timeout: Duration,
    }

    impl MockAlgorithm {
        fn new() -> Self {
            Self { requirements: ComputationRequirements::none(), timeout: Duration::from_secs(1) }
        }

        fn with_requirements(mut self, requirements: ComputationRequirements) -> Self {
            self.requirements = requirements;
            self
        }
    }

    impl Algorithm for MockAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "mock"
        }

        async fn find_best_route(
            &self,
            _request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<crate::types::RouteResult, crate::AlgorithmError> {
            Err(crate::AlgorithmError::Other("not implemented".to_string()))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            self.requirements.clone()
        }

        fn timeout(&self) -> Duration {
            self.timeout
        }
    }

    /// Mock algorithm that returns a structurally invalid route (two disconnected swaps).
    /// Used to verify the worker rejects invalid routes regardless of which algorithm produced
    /// them.
    struct InvalidRouteAlgorithm;

    impl Algorithm for InvalidRouteAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "invalid_route_mock"
        }

        async fn find_best_route(
            &self,
            _request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<RouteResult, AlgorithmError> {
            let token_a = token(0x01, "A");
            let token_b = token(0x02, "B");
            let token_c = token(0x03, "C");
            let token_d = token(0x04, "D");
            // A→B then C→D: the first swap's output (B) does not feed the second's input (C),
            // so `validate` must reject this as `DisconnectedSwaps`.
            let swap_ab = Swap::new(
                "p1".to_string(),
                "mock".to_string(),
                token_a.address.clone(),
                token_b.address.clone(),
                BigUint::from(100u64),
                BigUint::from(90u64),
                BigUint::from(1u64),
                component("p1", &[token_a.clone(), token_b.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            );
            let swap_cd = Swap::new(
                "p2".to_string(),
                "mock".to_string(),
                token_c.address.clone(),
                token_d.address.clone(),
                BigUint::from(90u64),
                BigUint::from(80u64),
                BigUint::from(1u64),
                component("p2", &[token_c.clone(), token_d.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            );
            let route =
                Route::new(vec![swap_ab, swap_cd], FxHashMap::default()).expect("non-empty route");
            Ok(RouteResult::new(route, num_bigint::BigInt::from(0), BigUint::from(1u64)))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    #[tokio::test]
    async fn test_quote_rejects_invalid_route() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, InvalidRouteAlgorithm, 0, "test_pool".to_string());

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = worker
            .quote(&ord, SolveParams::default())
            .await;

        match result {
            Err(SolveError::AlgorithmError(msg)) => {
                assert!(msg.contains("invalid route"), "unexpected message: {msg}");
            }
            other => panic!("expected AlgorithmError for invalid route, got {other:?}"),
        }
    }

    /// Deliberately ignores exclusions so the worker must enforce them on the result.
    struct FilterIgnoringAlgorithm(Route);

    impl Algorithm for FilterIgnoringAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "filter_ignoring_mock"
        }

        async fn find_best_route(
            &self,
            _request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<RouteResult, AlgorithmError> {
            Ok(RouteResult::new(self.0.clone(), num_bigint::BigInt::from(0), BigUint::from(1u64)))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    #[rstest]
    #[case::pool(RouteExclusionFilter::default().with_excluded_pools(["p2".to_string()]), Some("excluded pool p2"))]
    #[case::protocol(RouteExclusionFilter::default().with_excluded_protocols(["uniswap_v2".to_string()]), Some("excluded protocol uniswap_v2"))]
    #[case::protocol_prefix(RouteExclusionFilter::default().with_excluded_protocols(["uniswap:".to_string()]), None)]
    #[case::intermediate(RouteExclusionFilter::default().with_excluded_tokens([token(0x02, "B").address]), Some("excluded intermediate token"))]
    #[case::empty(RouteExclusionFilter::default(), None)]
    #[case::unrelated(RouteExclusionFilter::default().with_excluded_pools(["other".to_string()]).with_excluded_protocols(["other".to_string()]).with_excluded_tokens([token(0x04, "D").address]), None)]
    #[case::endpoints(RouteExclusionFilter::default().with_excluded_tokens([token(0x01, "A").address, token(0x03, "C").address]), None)]
    #[tokio::test]
    async fn test_quote_enforces_route_filter(
        #[case] filter: RouteExclusionFilter,
        #[case] rejection: Option<&str>,
        #[values(false, true)] split: bool,
    ) {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let make_swap = |id: &str,
                         token_in: &tycho_simulation::tycho_common::models::token::Token,
                         token_out: &tycho_simulation::tycho_common::models::token::Token,
                         amount_in: u64,
                         amount_out: u64| {
            Swap::new(
                id.to_string(),
                "uniswap_v2".to_string(),
                token_in.address.clone(),
                token_out.address.clone(),
                BigUint::from(amount_in),
                BigUint::from(amount_out),
                BigUint::from(1u64),
                component(id, &[token_in.clone(), token_out.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            )
        };
        let swaps = if split {
            vec![
                make_swap("direct", &token_a, &token_c, 50, 40).with_split(0.5),
                make_swap("p1", &token_a, &token_b, 50, 45),
                make_swap("p2", &token_b, &token_c, 45, 40),
            ]
        } else {
            vec![
                make_swap("p1", &token_a, &token_b, 100, 90),
                make_swap("p2", &token_b, &token_c, 90, 80),
            ]
        };
        let route = Route::new(swaps, FxHashMap::default()).unwrap();
        route.validate().unwrap();
        // The returned pools are absent from this market: protocol validation must inspect the
        // actual route rather than relying on the pool IDs resolved before the algorithm ran.
        let (market, _) = setup_market_weighted(vec![]);
        let mut worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            FilterIgnoringAlgorithm(route),
            0,
            "test_pool".to_string(),
        );
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);
        let result = worker
            .quote(&ord, SolveParams::default().with_route_filter(filter))
            .await;
        match (rejection, result) {
            (Some(expected), Err(SolveError::AlgorithmError(msg))) => {
                assert!(msg.contains(expected), "unexpected message: {msg}");
            }
            (None, Ok(quote)) => assert_eq!(quote.order().status(), QuoteStatus::Success),
            (expected, result) => panic!("expected rejection {expected:?}, got {result:?}"),
        }
    }

    /// Always fails, with an error message listing the pools the worker resolved the request's
    /// filter into.
    struct ReportExclusionsAlgorithm;

    impl Algorithm for ReportExclusionsAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "report_exclusions_mock"
        }

        async fn find_best_route(
            &self,
            request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<RouteResult, AlgorithmError> {
            let mut excluded: Vec<&str> = ["p1", "p2"]
                .into_iter()
                .filter(|id| request.exclusions().excludes_pool(id))
                .collect();
            excluded.sort_unstable();
            Err(AlgorithmError::Other(format!("excluded: {}", excluded.join(","))))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    /// Mock algorithm that returns a two-branch A→B route. This models the split-route shape
    /// that `water_fill` returns. The worker calculates the quote's price impact.
    struct SplitRouteAlgorithm;

    impl Algorithm for SplitRouteAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "split_route_mock"
        }

        async fn find_best_route(
            &self,
            _request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<RouteResult, AlgorithmError> {
            let token_a = token(0x01, "A");
            let token_b = token(0x02, "B");
            // Both pools report a spot price of 2.0. 60 A through p1 pays 114 (reference 120),
            // and the remaining 40 A through p2 pays 78 (reference 80): 192 out of a reference
            // output of 200, a 4% impact.
            let swap_p1 = Swap::new(
                "p1".to_string(),
                "mock".to_string(),
                token_a.address.clone(),
                token_b.address.clone(),
                BigUint::from(60u64),
                BigUint::from(114u64),
                BigUint::from(1u64),
                component("p1", &[token_a.clone(), token_b.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            )
            .with_split(0.6);
            let swap_p2 = Swap::new(
                "p2".to_string(),
                "mock".to_string(),
                token_a.address.clone(),
                token_b.address.clone(),
                BigUint::from(40u64),
                BigUint::from(78u64),
                BigUint::from(1u64),
                component("p2", &[token_a.clone(), token_b.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            );
            let route = Route::new(
                vec![swap_p1, swap_p2],
                [
                    (token_a.address.clone(), token_a.clone()),
                    (token_b.address.clone(), token_b.clone()),
                ],
            )
            .expect("non-empty route");
            Ok(RouteResult::new(route, num_bigint::BigInt::from(192), BigUint::from(1u64)))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    /// The request names a protocol system; the worker hands the algorithm that system's pools.
    #[tokio::test]
    async fn test_quote_resolves_the_route_filter_against_the_market() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![
            ("p1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("p2", &token_a, &token_b, MockProtocolSim::new(2.0)),
        ]);
        let derived = DerivedData::new_shared();
        let mut worker = SolverWorker::new(
            market,
            derived,
            ReportExclusionsAlgorithm,
            0,
            "test_pool".to_string(),
        );

        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);
        let params = SolveParams::default().with_route_filter(
            RouteExclusionFilter::default().with_excluded_protocols(["uniswap_v2".to_string()]),
        );

        let result = worker.quote(&ord, params).await;

        match result {
            Err(SolveError::AlgorithmError(msg)) => {
                assert!(msg.contains("excluded: p1,p2"), "unexpected message: {msg}");
            }
            other => panic!("expected the algorithm's report, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_quote_price_impact_for_split_route() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![
            ("p1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("p2", &token_a, &token_b, MockProtocolSim::new(2.0)),
        ]);
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, SplitRouteAlgorithm, 0, "test_pool".to_string());
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let quote = worker
            .quote(&ord, SolveParams::default())
            .await
            .expect("split route must quote");

        assert_eq!(quote.order().price_impact_bps(), Some(400));
    }

    /// Mock algorithm that returns a single-leg route through a pAMM executed via the
    /// PropAMMRouter. The market holds no Uniswap V3 pool for the pair, so the router's fallback
    /// would revert and the worker must drop the route.
    struct PropAMMRouteAlgorithm;

    impl Algorithm for PropAMMRouteAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "propamm_route_mock"
        }

        async fn find_best_route(
            &self,
            _request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<RouteResult, AlgorithmError> {
            let token_a = token(0x01, "A");
            let token_b = token(0x02, "B");
            let swap = Swap::new(
                "pamm".to_string(),
                format!("{PROPAMM_FALLBACK_PREFIX}fermiswap"),
                token_a.address.clone(),
                token_b.address.clone(),
                BigUint::from(100u64),
                BigUint::from(200u64),
                BigUint::from(1u64),
                component("pamm", &[token_a.clone(), token_b.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            );
            let route = Route::new(vec![swap], FxHashMap::default()).expect("non-empty route");
            Ok(RouteResult::new(route, num_bigint::BigInt::from(200), BigUint::from(1u64)))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    /// The fee tier the router resolves the route's pair to.
    const FALLBACK_FEE_TIER: u32 = 3000;

    /// Quotes the pAMM route above through a worker holding `fee_tiers`, against a market with no
    /// fallback pool in it.
    async fn quote_pamm_route(fee_tiers: SharedFeeTiers) -> Result<SingleOrderQuote, SolveError> {
        let (market, _) = setup_market_weighted(vec![]);
        quote_pamm_route_against(market, fee_tiers).await
    }

    /// Quotes the pAMM route above through a worker holding `fee_tiers`, against `market`.
    ///
    /// The worker indexes the market's fallback pools on `initialize_graph`, so the market has to
    /// be in place before the quote.
    async fn quote_pamm_route_against(
        market: MarketData,
        fee_tiers: SharedFeeTiers,
    ) -> Result<SingleOrderQuote, SolveError> {
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, PropAMMRouteAlgorithm, 0, "test_pool".to_string())
                .with_fallback_fee_tiers(fee_tiers);
        worker.initialize_graph().await;

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        worker
            .quote(&ord, SolveParams::default())
            .await
    }

    /// A market holding one Uniswap V3 pool for the route's pair, at the tier the router resolves
    /// to, but with too little liquidity to price the swap the fallback would make.
    fn market_with_unpriceable_fallback_pool() -> MarketData {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let mut fallback = component_with_protocol(
            "fallback_pool",
            FALLBACK_PROTOCOL_SYSTEM,
            &[token_a.clone(), token_b.clone()],
        );
        fallback.static_attributes.insert(
            FEE_ATTRIBUTE.to_string(),
            Bytes::from(FALLBACK_FEE_TIER.to_be_bytes().to_vec()),
        );

        // Built on the shared setup so the market carries the block info and gas price a quote
        // needs; only the fallback pool is added here.
        let (market, _) = setup_market_weighted(vec![]);
        {
            let mut state = market.try_write().expect("uncontended");
            state.upsert_tokens([token_a, token_b]);
            state.upsert_components([fallback]);
            state.update_states([(
                "fallback_pool".to_string(),
                Box::new(MockProtocolSim::new(2.0).with_liquidity(1)) as Box<dyn ProtocolSim>,
            )]);
        }
        market
    }

    #[rstest]
    #[case::pool(RouteExclusionFilter::default().with_excluded_pools(["fallback_pool".to_string()]))]
    #[case::protocol(RouteExclusionFilter::default().with_excluded_protocols(["uniswap_v3".to_string()]))]
    #[tokio::test]
    async fn test_quote_pamm_route_with_excluded_fallback(#[case] filter: RouteExclusionFilter) {
        let tiers = SharedFeeTiers::default();
        tiers.set(FeeTiers::new(FALLBACK_FEE_TIER));
        let mut worker = SolverWorker::new(
            market_with_unpriceable_fallback_pool(),
            DerivedData::new_shared(),
            PropAMMRouteAlgorithm,
            0,
            "test_pool".to_string(),
        )
        .with_fallback_fee_tiers(tiers);
        worker.initialize_graph().await;
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let order = order(&a, &b, 100, OrderSide::Sell);
        let result = worker
            .quote(&order, SolveParams::default().with_route_filter(filter))
            .await;
        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected { reason: RouteRejection::PammFallbackExcluded, .. })
            ),
            "expected rejection before fallback simulation, got {result:?}"
        );
    }

    /// Without a Uniswap V3 pool at the router's fee tier the fallback reverts too, so there is no
    /// fallback amount to check `min_amount_out` against.
    #[tokio::test]
    async fn test_quote_pamm_route_without_fallback_pool() {
        let fee_tiers = SharedFeeTiers::default();
        fee_tiers.set(crate::propamm_fallback::FeeTiers::new(3000));

        let result = quote_pamm_route(fee_tiers).await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected {
                    reason: RouteRejection::PammFallbackPoolMissing,
                    ..
                })
            ),
            "expected the unbacked pAMM route to be dropped, got {result:?}"
        );
    }

    /// A fallback pool that cannot price the swap leaves no amount to check `min_amount_out`
    /// against, so the route is dropped rather than ranked on the pAMM's own amount.
    #[tokio::test]
    async fn test_quote_pamm_route_with_unpriceable_fallback() {
        let fee_tiers = SharedFeeTiers::default();
        fee_tiers.set(FeeTiers::new(FALLBACK_FEE_TIER));

        let result =
            quote_pamm_route_against(market_with_unpriceable_fallback_pool(), fee_tiers).await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected {
                    reason: RouteRejection::PammFallbackUnpriceable,
                    ..
                })
            ),
            "expected the unpriceable fallback to drop the route, got {result:?}"
        );
    }

    /// Before the fetcher reads the router's tiers there is no tier to price the fallback at, so
    /// the route is dropped rather than priced against a guessed one.
    #[tokio::test]
    async fn test_quote_pamm_route_without_fee_tiers() {
        let result = quote_pamm_route(SharedFeeTiers::default()).await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected { reason: RouteRejection::PammFeeTiersUnread, .. })
            ),
            "expected the pAMM route to be dropped, got {result:?}"
        );
    }

    // ==================== wait_until_ready Tests ====================

    #[tokio::test]
    async fn wait_until_ready_returns_immediately_when_no_requirements() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let algorithm = MockAlgorithm::new();
        let worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Should return immediately since there are no requirements
        let result = worker
            .wait_until_ready(Duration::from_millis(10))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_until_ready_returns_immediately_when_already_ready() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .allow_stale(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Mark as ready by handling a completion event
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationComplete {
                computation_id: SpotPriceComputation::ID,
                block: 1,
                failed_items: vec![],
            });

        // Should return immediately since already ready
        let result = worker
            .wait_until_ready(Duration::from_millis(10))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_until_ready_times_out_when_not_ready() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Should timeout since no events are received
        let result = worker
            .wait_until_ready(Duration::from_millis(50))
            .await;

        assert!(result.is_err());
        match result {
            Err(SolveError::NotReady(msg)) => {
                assert!(msg.contains("timeout"));
                assert!(msg.contains("spot_prices"));
            }
            other => panic!("Expected NotReady error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn wait_until_ready_wakes_up_on_notify() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Clone the notify handle to simulate the main loop notifying
        let notify = worker.ready_notify.clone();

        // Spawn a task that will notify after a short delay
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            notify.notify_waiters();
        });

        // wait_until_ready should wake up when notified but still timeout
        // because we didn't actually update the tracker
        let result = worker
            .wait_until_ready(Duration::from_millis(100))
            .await;

        handle.await.unwrap();

        // Should still timeout because notify woke us up but we're not actually ready
        assert!(result.is_err());
    }

    /// `exclude_protocols` keeps a whole protocol family out of this worker's graph, while the
    /// worker pools that do not set it keep routing through those components.
    #[test]
    fn test_should_drop_component_by_excluded_protocol() {
        let (market, _) = setup_market_weighted(vec![]);
        let worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        )
        .with_exclude_protocols(vec![PROPAMM_FALLBACK_PREFIX.to_string()]);

        let pamm =
            component_with_protocol("pamm-1", "propammfallback:fermiswap", &[token(0x01, "A")]);
        let public = component("uni-1", &[token(0x01, "A")]);

        assert!(should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, &pamm));
        assert!(!should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, &public));
    }

    /// The pAMM every admission test decides.
    const PAMM: &str = "pamm-1";

    /// The Uniswap V3 pool the pAMM falls back to.
    const FALLBACK_POOL: &str = "uni-1";

    /// A market holding the pAMM and the Uniswap V3 pool it falls back to.
    fn market_with_pamm_and_fallback() -> MarketData {
        let market = market_with_pamm();
        add_fallback_pool(&market);
        market
    }

    /// A market holding the pAMM and no fallback pool.
    fn market_with_pamm() -> MarketData {
        let (market, _) = setup_market_weighted(vec![]);
        add_pamm(&market);
        market
    }

    /// Adds the pAMM the admission tests decide.
    fn add_pamm(market: &MarketData) {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let pamm = component_with_protocol(
            PAMM,
            "propammfallback:fermiswap",
            &[token_a.clone(), token_b.clone()],
        );
        let mut state = market.try_write().expect("uncontended");
        state.upsert_tokens([token_a, token_b]);
        state.upsert_components([pamm]);
    }

    /// Adds the Uniswap V3 pool the pAMM falls back to at [`FALLBACK_FEE_TIER`].
    fn add_fallback_pool(market: &MarketData) {
        let mut fallback = component_with_protocol(
            FALLBACK_POOL,
            FALLBACK_PROTOCOL_SYSTEM,
            &[token(0x01, "A"), token(0x02, "B")],
        );
        fallback.static_attributes.insert(
            FEE_ATTRIBUTE.to_string(),
            Bytes::from(FALLBACK_FEE_TIER.to_be_bytes().to_vec()),
        );
        let mut state = market.try_write().expect("uncontended");
        state.upsert_components([fallback]);
    }

    /// A worker whose fallback pool index describes `market`, holding `fee_tiers`, beside the
    /// shared handle a test writes a later tier read through.
    fn admission_worker(
        market: MarketData,
        fee_tiers: Option<FeeTiers>,
    ) -> (SolverWorker<MockAlgorithm>, SharedFeeTiers) {
        let shared = SharedFeeTiers::default();
        if let Some(fee_tiers) = fee_tiers {
            shared.set(fee_tiers);
        }
        let mut worker = SolverWorker::new(
            market.clone(),
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        )
        .with_fallback_fee_tiers(shared.clone());
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        worker
            .pamm_admission
            .rebuild_pools(&view);
        (worker, shared)
    }

    /// A `MarketUpdated` event that adds one component, by id.
    fn added_component_event(component_id: &str) -> MarketEvent {
        MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(component_id.to_string(), Vec::new())]),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        }
    }

    /// A `MarketUpdated` event that removes one component, by id.
    fn removed_component_event(component_id: &str) -> MarketEvent {
        MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: vec![component_id.to_string()],
            updated_components: Vec::new(),
        }
    }

    fn added_ids(event: &MarketEvent) -> Vec<ComponentId> {
        let MarketEvent::MarketUpdated { added_components, .. } = event;
        added_components
            .keys()
            .cloned()
            .collect()
    }

    fn removed_ids(event: &MarketEvent) -> Vec<ComponentId> {
        let MarketEvent::MarketUpdated { removed_components, .. } = event;
        removed_components.clone()
    }

    /// Runs `event` through both of the worker's rules, in the order `process_event` runs them.
    fn admit(
        worker: &mut SolverWorker<MockAlgorithm>,
        market: &MarketData,
        mut event: MarketEvent,
    ) -> MarketEvent {
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let fee_tiers = worker.pamm_admission.fee_tiers();
        let caller_drops = |component: &ProtocolComponent| {
            should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, component)
        };
        worker
            .pamm_admission
            .apply_pamm_admission(&view, fee_tiers.as_ref(), &caller_drops, &mut event);
        filter_event(view.base_market_state(), event, &caller_drops)
    }

    /// A pAMM only reaches the chain through the Uniswap V3 pool it falls back to, so the worker
    /// admits one exactly when this market holds that pool at the tier the router resolves. With
    /// no tiers read there is no tier to look up, and guessing one prices the wrong pool.
    #[rstest]
    #[case::without_fallback_pool(
        market_with_pamm(),
        Some(FeeTiers::new(FALLBACK_FEE_TIER)),
        false
    )]
    #[case::with_fallback_pool(
        market_with_pamm_and_fallback(),
        Some(FeeTiers::new(FALLBACK_FEE_TIER)),
        true
    )]
    #[case::before_fee_tiers(market_with_pamm_and_fallback(), None, false)]
    fn test_apply_pamm_admission(
        #[case] market: MarketData,
        #[case] fee_tiers: Option<FeeTiers>,
        #[case] expect_admitted: bool,
    ) {
        let (mut worker, _shared_tiers) = admission_worker(market.clone(), fee_tiers);

        let event = admit(&mut worker, &market, added_component_event(PAMM));

        let admitted = added_ids(&event) == vec![PAMM.to_string()];
        assert_eq!(admitted, expect_admitted, "additions were {:?}", added_ids(&event));
    }

    /// The event that removes a Uniswap V3 pool does not name the pAMMs that fell back to it, so
    /// the pAMM would otherwise stay in the graph and produce routes the worker cannot price.
    #[test]
    fn test_apply_pamm_admission_after_the_fallback_pool_leaves() {
        let market = market_with_pamm_and_fallback();
        let (mut worker, _shared_tiers) =
            admission_worker(market.clone(), Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        admit(&mut worker, &market, added_component_event(PAMM));
        let event = admit(&mut worker, &market, removed_component_event(FALLBACK_POOL));

        assert!(
            removed_ids(&event).contains(&PAMM.to_string()),
            "the pAMM must leave with its fallback pool: {:?}",
            removed_ids(&event)
        );
        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Withheld),
            "kept, so it can come back"
        );
    }

    /// The market names a component in `added_components` once, so the event that adds the
    /// fallback pool does not name the pAMM. Without the withheld set the pAMM would stay out
    /// until the next rebuild, which is worse than before this rule existed.
    #[test]
    fn test_apply_pamm_admission_after_the_fallback_pool_arrives() {
        let market = market_with_pamm();
        let (mut worker, _shared_tiers) =
            admission_worker(market.clone(), Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        admit(&mut worker, &market, added_component_event(PAMM));
        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Withheld),
            "withheld while it was unbacked"
        );

        add_fallback_pool(&market);
        let event = admit(&mut worker, &market, added_component_event(FALLBACK_POOL));

        assert!(
            added_ids(&event).contains(&PAMM.to_string()),
            "the pAMM must join the graph with its fallback pool: {:?}",
            added_ids(&event)
        );
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Admitted));
    }

    /// A pAMM held out because nothing backs it can leave the market before the block that adds
    /// its fallback pool reaches the worker. A component the market has dropped is not backed
    /// again, and adding it would carry no tokens at all.
    #[test]
    fn test_apply_pamm_admission_keeps_a_missing_pamm_withheld() {
        let market = market_with_pamm();
        let (mut worker, _shared_tiers) =
            admission_worker(market.clone(), Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        admit(&mut worker, &market, added_component_event(PAMM));
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Withheld));

        add_fallback_pool(&market);
        // The market has advanced past the pool's arrival before the worker processes it.
        market
            .try_write()
            .expect("uncontended")
            .remove_components([&PAMM.to_string()]);
        let event = admit(&mut worker, &market, added_component_event(FALLBACK_POOL));

        assert_eq!(added_ids(&event), vec![FALLBACK_POOL.to_string()]);
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Withheld));
    }

    /// A pAMM and the Uniswap V3 pool it falls back to can arrive in one block. The pool index
    /// has to take the block before admission reads it, or the pAMM is judged against an index
    /// that does not hold its pool yet. Built against a market holding neither component, so no
    /// earlier index build can hide the ordering.
    #[tokio::test]
    async fn test_process_event_pamm_and_fallback_pool_in_one_block() {
        let (market, _) = setup_market_weighted(vec![]);
        let (mut worker, _shared_tiers) =
            admission_worker(market.clone(), Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        worker.initialize_graph().await;
        add_pamm(&market);
        add_fallback_pool(&market);
        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([
                (PAMM.to_string(), Vec::new()),
                (FALLBACK_POOL.to_string(), Vec::new()),
            ]),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        };

        worker.process_event(event).await;

        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Admitted),
            "the index must take the block before admission reads it"
        );
    }

    /// The first tier read is what lets a pAMM in, so the worker rebuilds when the tiers it
    /// filtered with stop matching the current ones.
    #[tokio::test]
    async fn test_process_event_after_the_first_fee_tier_read() {
        let market = market_with_pamm_and_fallback();
        let (mut worker, shared_tiers) = admission_worker(market.clone(), None);
        worker.initialize_graph().await;
        assert_eq!(
            worker
                .pamm_admission
                .count_in(PammState::Admitted),
            0,
            "no tiers, no pAMM"
        );

        shared_tiers.set(FeeTiers::new(FALLBACK_FEE_TIER));
        worker
            .process_event(added_component_event(FALLBACK_POOL))
            .await;

        assert_eq!(
            worker
                .pamm_admission
                .built_with_fee_tiers(),
            Some(&FeeTiers::new(FALLBACK_FEE_TIER))
        );
        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Admitted),
            "the rebuild admits the backed pAMM"
        );
    }

    /// A tier change moves which Uniswap V3 pool a pAMM falls back to, so the worker rebuilds and
    /// the pAMM leaves the graph when the new tier names no pool for its pair.
    #[tokio::test]
    async fn test_process_event_after_a_fee_tier_change() {
        let market = market_with_pamm_and_fallback();
        let (mut worker, shared_tiers) =
            admission_worker(market.clone(), Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        worker.initialize_graph().await;
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Admitted));

        shared_tiers.set(FeeTiers::new(FALLBACK_FEE_TIER + 1));
        worker
            .process_event(added_component_event(FALLBACK_POOL))
            .await;

        assert_eq!(
            worker
                .pamm_admission
                .built_with_fee_tiers(),
            Some(&FeeTiers::new(FALLBACK_FEE_TIER + 1)),
            "the tier change must rebuild the graph"
        );
        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Withheld),
            "no pool at the new tier, so the pAMM leaves"
        );
    }

    /// A pAMM falls back through the router, not through this worker's graph, so a Uniswap V3
    /// pool the worker excludes still backs it. The block that adds that pool names no pAMM, and
    /// the worker's own filter empties it, so the pAMM pass must read the event the market
    /// broadcast rather than the filtered one.
    #[tokio::test]
    async fn test_process_event_readmits_a_pamm_behind_an_excluded_fallback_pool() {
        let market = market_with_pamm();
        let (mut worker, _shared_tiers) =
            admission_worker(market.clone(), Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        worker.exclude_protocols = vec![FALLBACK_PROTOCOL_SYSTEM.to_string()];
        worker.initialize_graph().await;
        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Withheld),
            "no fallback pool yet"
        );

        add_fallback_pool(&market);
        worker
            .process_event(added_component_event(FALLBACK_POOL))
            .await;

        assert_eq!(
            worker.pamm_admission.state_of(PAMM),
            Some(PammState::Admitted),
            "the excluded pool still backs the pAMM on chain"
        );
    }

    /// A pAMM the worker's own `exclude_protocols` rule drops is out for a reason this rule does
    /// not own, so the build records it as neither admitted nor withheld. Were it withheld, the
    /// event path would put it back the moment its fallback pool arrived.
    #[tokio::test]
    async fn test_initialize_graph_leaves_an_excluded_pamm_off_the_record() {
        let market = market_with_pamm_and_fallback();
        let (mut worker, _shared_tiers) =
            admission_worker(market, Some(FeeTiers::new(FALLBACK_FEE_TIER)));
        worker.exclude_protocols = vec![PROPAMM_FALLBACK_PREFIX.to_string()];

        worker.initialize_graph().await;

        assert_eq!(worker.pamm_admission.state_of(PAMM), None);
    }

    /// Without `exclude_protocols` the worker drops nothing on protocol grounds — the liquidity
    /// scope stays the only reason to leave a component out.
    #[test]
    fn test_should_drop_component_without_exclusions() {
        let (market, _) = setup_market_weighted(vec![]);
        let worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        );

        let pamm =
            component_with_protocol("pamm-1", "propammfallback:fermiswap", &[token(0x01, "A")]);

        assert!(!should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, &pamm));
    }

    #[tokio::test]
    async fn wait_until_ready_succeeds_when_notified_and_ready() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Clone the notify handle and get a reference to the tracker
        let notify = worker.ready_notify.clone();

        // Spawn a task that will update tracker and notify
        let handle = tokio::spawn({
            // We need to update the tracker from outside, so we simulate
            // what the main loop does: update tracker then notify
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                notify.notify_waiters();
            }
        });

        // Manually update the tracker to simulate what would happen in the main loop
        // In real usage, the main loop updates tracker THEN notifies
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationComplete {
                computation_id: SpotPriceComputation::ID,
                block: 1,
                failed_items: vec![],
            });

        // Now wait - should succeed immediately since we're already ready
        let result = worker
            .wait_until_ready(Duration::from_millis(100))
            .await;

        handle.abort(); // Don't need to wait for the spawned task
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn notify_pattern_handles_multiple_waiters() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .allow_stale(TokenGasPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        let notify = worker.ready_notify.clone();

        // Spawn multiple waiting tasks
        let notify1 = notify.clone();
        let waiter1 = tokio::spawn(async move {
            notify1.notified().await;
            true
        });

        let notify2 = notify.clone();
        let waiter2 = tokio::spawn(async move {
            notify2.notified().await;
            true
        });

        // Give waiters time to register
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Update tracker and notify all waiters
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationComplete {
                computation_id: TokenGasPriceComputation::ID,
                block: 1,
                failed_items: vec![],
            });
        notify.notify_waiters();

        // Both waiters should complete
        let (r1, r2) = tokio::join!(waiter1, waiter2);
        assert!(r1.unwrap());
        assert!(r2.unwrap());
    }

    #[tokio::test]
    async fn wait_until_ready_returns_immediately_on_blocked_state() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Mark the current block and record a failure for spot_prices
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::NewBlock { block: 1 });
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationFailed {
                computation_id: SpotPriceComputation::ID,
                block: 1,
            });

        // Notify AFTER wait_until_ready starts waiting (must arrive after the
        // Notified future is registered, not before).
        let notify = worker.ready_notify.clone();
        let notifier = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            notify.notify_waiters();
        });

        // wait_until_ready is woken by the notification, then checks
        // is_blocked_for_current_block() → true → returns Err immediately.
        let result = worker
            .wait_until_ready(Duration::from_secs(5))
            .await;
        notifier.await.unwrap();

        match result {
            Err(SolveError::ComputationFailed(msg)) => {
                assert!(
                    msg.contains("required computation failed"),
                    "expected 'required computation failed' message, got: {msg}"
                );
            }
            other => panic!("Expected ComputationFailed error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn wait_until_ready_returns_blocked_when_failure_already_processed() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Mark the current block and record a failure for spot_prices
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::NewBlock { block: 1 });
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationFailed {
                computation_id: SpotPriceComputation::ID,
                block: 1,
            });

        // Do NOT spawn a notifier — the failure was already processed
        // before wait_until_ready starts. Without the is_blocked_for_current_block() check in the
        // loop body, this hangs for 1 second and returns NotReady.
        let result = worker
            .wait_until_ready(Duration::from_secs(1))
            .await;

        match result {
            Err(SolveError::ComputationFailed(msg)) => {
                assert!(
                    msg.contains("required computation failed"),
                    "expected 'required computation failed' message, got: {msg}"
                );
            }
            other => panic!("Expected ComputationFailed error, got {:?}", other),
        }
    }

    // ==================== Integration Tests with run() ====================

    #[tokio::test]
    async fn worker_updates_tracker_and_notifies_on_derived_event() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0, "test_pool".to_string());

        // Create channels
        let (_event_tx, event_rx) = broadcast::channel::<MarketEvent>(16);
        let (derived_tx, derived_rx) = broadcast::channel::<DerivedDataEvent>(16);
        let (_task_tx, task_rx) = async_channel::bounded::<crate::types::internal::SolveTask>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        // Spawn worker
        let handle = tokio::spawn(async move {
            worker
                .run(event_rx, derived_rx, task_rx, shutdown_rx)
                .await;
        });

        // Send a derived data event
        derived_tx
            .send(DerivedDataEvent::ComputationComplete {
                computation_id: SpotPriceComputation::ID,
                block: 1,
                failed_items: vec![],
            })
            .unwrap();

        // Give worker time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutdown
        let _ = shutdown_tx.send(());

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("worker should shutdown")
            .expect("worker task should not panic");
    }

    /// Captures log output for assertions, shared between the subscriber and the test.
    #[derive(Clone, Default)]
    struct SharedLogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogBuffer {
        type Writer = SharedLogBuffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    async fn worker_handles_derived_channel_close_once_without_spinning() {
        // recv() on a closed broadcast channel returns Closed instantly on every call: if the
        // worker keeps polling that arm, its select loop degenerates into a busy spin that pegs
        // a core and floods the log (seen live: millions of identical warns per minute, starved
        // solves, poisoned WebSocket reconnects). The closed channel must be handled exactly
        // once, and the worker must stay responsive afterwards.
        let logs = SharedLogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, MockAlgorithm::new(), 0, "test_pool".to_string());

        let (_event_tx, event_rx) = broadcast::channel::<MarketEvent>(16);
        let (derived_tx, derived_rx) = broadcast::channel::<DerivedDataEvent>(16);
        let (_task_tx, task_rx) = async_channel::bounded::<crate::types::internal::SolveTask>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        let handle = tokio::spawn(async move {
            worker
                .run(event_rx, derived_rx, task_rx, shutdown_rx)
                .await;
        });

        // Close the derived-data channel, then give a spinning loop ample time to spam.
        drop(derived_tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The worker must still be responsive.
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("worker should shutdown")
            .expect("worker task should not panic");

        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let closed_warns = output
            .matches("derived event receiver closed")
            .count();
        assert_eq!(
            closed_warns, 1,
            "closed channel must be handled once, not spun on ({closed_warns} warns)"
        );
    }

    #[test]
    fn no_route_found_with_reason_carries_reason() {
        use crate::algorithm::NoPathReason;
        let err = SolveError::no_route_found_with_reason(
            "order-1",
            NoPathReason::DestinationTokenNotInGraph,
        );
        match err {
            SolveError::NoRouteFound { order_id, reason } => {
                assert_eq!(order_id, "order-1");
                assert_eq!(reason, Some(NoPathReason::DestinationTokenNotInGraph));
            }
            other => panic!("expected NoRouteFound, got {other:?}"),
        }
    }

    #[test]
    fn no_route_found_defaults_to_no_reason() {
        match SolveError::no_route_found("order-1") {
            SolveError::NoRouteFound { reason, .. } => assert_eq!(reason, None),
            other => panic!("expected NoRouteFound, got {other:?}"),
        }
    }

    #[test]
    fn task_pickup_metrics_recorded() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_task_pickup_metrics("test_pool", std::time::Duration::from_millis(25), 3);
        });

        let mut wait_seen = false;
        let mut depth_seen = false;
        for (key, _unit, _description, value) in snapshotter.snapshot().into_vec() {
            let key = key.key();
            let pool_label = key
                .labels()
                .find(|label| label.key() == "pool")
                .map(|label| label.value().to_string());
            match key.name() {
                "worker_pool_queue_wait_seconds" => {
                    assert_eq!(pool_label.as_deref(), Some("test_pool"));
                    let DebugValue::Histogram(samples) = value else {
                        panic!("expected histogram, got {value:?}");
                    };
                    assert_eq!(samples.len(), 1);
                    assert!((samples[0].into_inner() - 0.025).abs() < 1e-9);
                    wait_seen = true;
                }
                "worker_pool_queue_depth" => {
                    assert_eq!(pool_label.as_deref(), Some("test_pool"));
                    let DebugValue::Gauge(depth) = value else {
                        panic!("expected gauge, got {value:?}");
                    };
                    assert!((depth.into_inner() - 3.0).abs() < f64::EPSILON);
                    depth_seen = true;
                }
                _ => {}
            }
        }
        assert!(wait_seen, "queue wait histogram not recorded");
        assert!(depth_seen, "queue depth gauge not recorded");
    }

    #[test]
    fn quote_duration_metric_recorded() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_quote_duration("test_pool", std::time::Duration::from_millis(120));
        });

        let mut quote_duration_seen = false;
        for (key, _unit, _description, value) in snapshotter.snapshot().into_vec() {
            let key = key.key();
            if key.name() != "worker_pool_solve_duration_seconds" {
                continue;
            }
            let pool_label = key
                .labels()
                .find(|label| label.key() == "pool")
                .map(|label| label.value().to_string());
            assert_eq!(pool_label.as_deref(), Some("test_pool"));
            let DebugValue::Histogram(samples) = value else {
                panic!("expected histogram, got {value:?}");
            };
            assert_eq!(samples.len(), 1);
            assert!((samples[0].into_inner() - 0.120).abs() < 1e-9);
            quote_duration_seen = true;
        }
        assert!(quote_duration_seen, "quote duration histogram not recorded");
    }

    #[test]
    fn test_price_impact_metrics_recorded() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_price_impact_metrics(
                "test_pool",
                std::time::Duration::from_micros(250),
                "unknown_token",
            );
        });

        let recorded = crate::tests::metrics::recorded_metrics(&snapshotter);
        let (_, labels, value) = recorded
            .iter()
            .find(|(name, _, _)| name == "worker_pool_price_impact_duration_seconds")
            .expect("price impact duration histogram not recorded");
        assert_eq!(labels, &vec!["pool=test_pool".to_string()]);
        let DebugValue::Histogram(samples) = value else {
            panic!("expected histogram, got {value:?}");
        };
        assert_eq!(samples.len(), 1);
        assert!((samples[0].into_inner() - 0.000_25).abs() < 1e-12);

        let (_, labels, value) = recorded
            .iter()
            .find(|(name, _, _)| name == "worker_pool_price_impact_calculations_total")
            .expect("price impact outcome counter not recorded");
        assert_eq!(
            labels,
            &vec!["pool=test_pool".to_string(), "outcome=unknown_token".to_string()]
        );
        let DebugValue::Counter(count) = value else {
            panic!("expected counter, got {value:?}");
        };
        assert_eq!(*count, 1);
    }

    #[test]
    fn test_algorithm_error_maps_data_not_found_to_missing_data() {
        let err = crate::AlgorithmError::DataNotFound { kind: "gas price", id: None };
        let mapped = solve_error_from_algorithm_error("o1", &num_bigint::BigUint::from(5u64), err);
        assert!(matches!(mapped, SolveError::MissingData(_)), "got {mapped:?}");
    }

    #[test]
    fn test_algorithm_error_maps_simulation_failed() {
        let err = crate::AlgorithmError::SimulationFailed {
            component_id: "pool-1".to_string(),
            error: "revert".to_string(),
        };
        let mapped = solve_error_from_algorithm_error("o1", &num_bigint::BigUint::from(5u64), err);
        assert!(matches!(mapped, SolveError::SimulationFailed(_)), "got {mapped:?}");
    }

    #[test]
    fn test_algorithm_error_maps_insufficient_liquidity() {
        let err = crate::AlgorithmError::InsufficientLiquidity;
        let mapped = solve_error_from_algorithm_error("o1", &num_bigint::BigUint::from(5u64), err);
        assert!(matches!(mapped, SolveError::InsufficientLiquidity { .. }), "got {mapped:?}");
    }

    #[test]
    fn test_algorithm_error_other_stays_algorithm_error() {
        let err = crate::AlgorithmError::Other("boom".to_string());
        let mapped = solve_error_from_algorithm_error("o1", &num_bigint::BigUint::from(5u64), err);
        assert!(matches!(mapped, SolveError::AlgorithmError(_)), "got {mapped:?}");
    }

    #[test]
    fn test_algorithm_error_timeout_stays_timeout() {
        let err = crate::AlgorithmError::Timeout { elapsed_ms: 7 };
        let mapped = solve_error_from_algorithm_error("o1", &num_bigint::BigUint::from(5u64), err);
        assert!(matches!(mapped, SolveError::Timeout { elapsed_ms: 7 }), "got {mapped:?}");
    }
}
