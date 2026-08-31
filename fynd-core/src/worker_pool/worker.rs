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
    algorithm::Algorithm,
    derived::{
        computation::ComputationRequirements, events::DerivedDataEvent, tracker::ReadinessTracker,
        SharedDerivedDataRef,
    },
    feed::{
        component_filter::{filter_event, is_excluded_protocol, remove_components},
        events::{MarketEvent, MarketEventHandler},
        exclusivity::is_exclusive,
        market_data::{MarketData, MarketDataView, StateLabel},
    },
    graph::{EdgeWeightUpdaterWithDerived, GraphManager},
    propamm_fallback::{
        fallback_amount_out, has_pamm_leg, lacks_fallback_pool, FallbackAmountOut,
        FallbackPoolIndex, FeeTiers, SharedFeeTiers,
    },
    types::internal::SolveTask,
    worker_pool_router::LiquidityScope,
    BlockInfo, Order, OrderQuote, QuoteStatus, SingleOrderQuote, SolveError, SolveParams,
};

/// Records per-worker-pool queue metrics at task pickup: how long the task waited in the
/// queue and the depth left behind it. Queue wait growing while solve time stays
/// flat is the leading indicator of worker saturation.
fn record_task_pickup_metrics(pool_name: &str, queue_wait: Duration, queue_depth: usize) {
    metrics::histogram!("worker_pool_queue_wait_seconds", "pool" => pool_name.to_string())
        .record(queue_wait.as_secs_f64());
    metrics::gauge!("worker_pool_queue_depth", "pool" => pool_name.to_string())
        .set(queue_depth as f64);
}

/// Records per-pool solve latency: one algorithm's own working time for one order, excluding
/// queue wait. Unlike `worker_router_solve_duration_seconds`, which times the router racing every
/// pool and so belongs to no single pool, this is attributable per pool.
///
/// Successful solves only — a pool that exhausts its timeout returns before this point and is
/// counted in `worker_router_solver_failures_total{error_type="timeout"}` instead.
fn record_solve_duration(pool_name: &str, solve_time: Duration) {
    metrics::histogram!("worker_pool_solve_duration_seconds", "pool" => pool_name.to_string())
        .record(solve_time.as_secs_f64());
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
    /// Whether the fee tiers were known when the graph was last built.
    ///
    /// `FeeTierFetcher` runs as its own task, so a worker usually builds its first graph before
    /// the tiers arrive and admits every pAMM. This records that, so the graph can be rebuilt once
    /// the tiers land and the pAMMs without a fallback pool can be left out.
    built_with_fee_tiers: bool,
    /// Worker identifier (for logging).
    worker_id: usize,
    /// Worker pool name (used as the `pool` metric label).
    pool_name: String,
    /// Which liquidity this worker ingests.
    liquidity_scope: LiquidityScope,
    /// Protocol systems this worker never routes through. Empty for every worker pool that does
    /// not set `exclude_protocols`.
    exclude_protocols: Vec<String>,
    /// Uniswap V3 pools the PropAMMRouter can fall back to, kept current from market events.
    fallback_pools: FallbackPoolIndex,
    /// Fee tiers the PropAMMRouter falls back on, read from chain by `FeeTierFetcher`.
    fallback_fee_tiers: SharedFeeTiers,
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
            built_with_fee_tiers: false,
            worker_id,
            pool_name,
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_pools: FallbackPoolIndex::default(),
            fallback_fee_tiers: SharedFeeTiers::default(),
        }
    }

    /// Sets the fee tiers used to locate a pAMM leg's Uniswap V3 fallback pool.
    pub(crate) fn with_fallback_fee_tiers(mut self, fallback_fee_tiers: SharedFeeTiers) -> Self {
        self.fallback_fee_tiers = fallback_fee_tiers;
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

    /// Whether this worker must keep `component` out of its graph: an exclusive component in a
    /// `PublicOnly` worker pool, or a component of an excluded protocol system.
    ///
    /// Holds for the life of the worker, which is what lets it filter state updates and removals
    /// as well as additions.
    fn drops_component(&self, component: &ProtocolComponent) -> bool {
        (self.liquidity_scope == LiquidityScope::PublicOnly && is_exclusive(component)) ||
            is_excluded_protocol(&self.exclude_protocols, component)
    }

    /// Whether this worker must leave `component` out when it builds its graph.
    ///
    /// [`drops_component`](Self::drops_component) plus a pAMM whose fallback pool this market does
    /// not hold. That leg can never produce a quotable route, so leaving it out lets the algorithm
    /// route around it instead of assembling a route the worker then discards whole.
    ///
    /// Only used when building the graph. The fee tiers and the fallback pools both change over
    /// time, so this answer expires; applying it to a state update or a removal would freeze a
    /// component's price or strand it in the graph. [`fallback_amount_out`] stays authoritative
    /// and catches every case this misses.
    ///
    /// `fee_tiers` is read once per build rather than per component: `snapshot` copies the map.
    fn drops_component_on_build(
        &self,
        component: &ProtocolComponent,
        fee_tiers: Option<&FeeTiers>,
    ) -> bool {
        self.drops_component(component) ||
            fee_tiers
                .is_some_and(|tiers| lacks_fallback_pool(component, tiers, &self.fallback_pools))
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

    /// Initializes the graph from MarketState.
    ///
    /// Call this on startup or to recreate the graph from the latest market topology.
    /// Gets the market topology from MarketState and uses it to build the graph.
    pub async fn initialize_graph(&mut self) {
        let topology = {
            // One read: the index and the topology must describe the same market, or a pAMM whose
            // fallback pool arrived between two reads is left out for the life of the worker.
            let market = self.market_data.read().await;
            self.fallback_pools = FallbackPoolIndex::build(&market);
            let topology = market.component_topology().clone(); // clone to avoid holding the lock
            let fee_tiers = self.fallback_fee_tiers.snapshot();
            remove_components(market.base_market_state(), topology, &|component| {
                self.drops_component_on_build(component, fee_tiers.as_ref())
            })
        };

        self.graph_manager
            .initialize_graph(&topology);
        self.built_with_fee_tiers = self
            .fallback_fee_tiers
            .snapshot()
            .is_some();
        self.initialized = true;
    }

    /// Processes a single market event.
    pub async fn process_event(&mut self, event: MarketEvent) {
        let event = {
            let market = self.market_data.read().await;
            filter_event(market.base_market_state(), event, &|component| {
                self.drops_component(component)
            })
        };
        match event {
            MarketEvent::MarketUpdated { .. } => {
                {
                    let market = self.market_data.read().await;
                    self.fallback_pools
                        .apply_event(&market, &event);
                }
                if !self.built_with_fee_tiers &&
                    self.fallback_fee_tiers
                        .snapshot()
                        .is_some()
                {
                    // The first graph was built without the tiers, so it admitted every pAMM.
                    // Rebuild once now that the router's tiers are known.
                    self.initialize_graph().await;
                }
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
        let (block_info, solved_against) = {
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
            (block_info, solved_against)
        };

        let result = self
            .algorithm
            .find_best_route(
                graph,
                self.market_data.clone(),
                params.state_label().cloned(),
                Some(self.derived_data.clone()),
                order,
            )
            .await;

        let order_quote = match result {
            Ok(result) => {
                // Extract scalar values before consuming result with into_route()
                let amount_out_net_gas = result
                    .net_amount_out()
                    .to_biguint()
                    .unwrap_or(BigUint::ZERO);
                let gas_price = result.gas_price().clone();
                let algo_price_impact = result.price_impact();
                let mut route = result.into_route();

                if let Err(err) = route.validate() {
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
                    let Some(fee_tiers) = self.fallback_fee_tiers.snapshot() else {
                        debug!(
                            order_id = %order.id(),
                            "dropping pAMM route: the router's fee tiers are not read yet"
                        );
                        return Err(SolveError::no_route_found(order.id()));
                    };
                    // The same view the algorithm solved against, so the fallback is priced on the
                    // requested overlay rather than the base state.
                    let market = self
                        .read_market(params.state_label())
                        .await?;
                    match fallback_amount_out(&route, &market, &fee_tiers, &self.fallback_pools) {
                        FallbackAmountOut::AmountOut(amount) => {
                            route.set_fallback_amount_out(amount)
                        }
                        FallbackAmountOut::NoFallbackPool { component_id, fee_tier } => {
                            debug!(
                                order_id = %order.id(),
                                %component_id,
                                fee_tier,
                                "dropping pAMM route: no Uniswap V3 pool at the router's fee tier"
                            );
                            return Err(SolveError::no_route_found(order.id()));
                        }
                        FallbackAmountOut::NotPriceable { reason } => {
                            debug!(
                                order_id = %order.id(),
                                %reason,
                                "dropping pAMM route: the Uniswap V3 fallback could not be simulated"
                            );
                            return Err(SolveError::no_route_found(order.id()));
                        }
                    }
                }

                // This is a first naive approach to getting the total gas of this quote
                // A finer estimation is done during encoding
                let gas_estimate = route.total_gas();
                let amount_in = if order.is_sell() {
                    order.amount().clone()
                } else {
                    route
                        .swaps()
                        .first()
                        .map(|s| s.amount_in().clone())
                        .ok_or_else(|| {
                            error!(
                                order_id = %order.id(),
                                "route missing first swap for buy order"
                            );
                            SolveError::no_route_found(order.id())
                        })?
                };
                let amount_out = if order.is_sell() {
                    let output_token = route.output_token().ok_or_else(|| {
                        error!(
                            order_id = %order.id(),
                            "route missing swaps for sell order"
                        );
                        SolveError::no_route_found(order.id())
                    })?;
                    route
                        .swaps()
                        .iter()
                        .filter(|s| *s.token_out() == output_token)
                        .map(|s| s.amount_out().clone())
                        .fold(BigUint::ZERO, |acc, x| acc + x)
                } else {
                    order.amount().clone()
                };

                let price_impact_bps = algo_price_impact
                    .or_else(|| {
                        super::price_impact::spot_price_impact(
                            &route,
                            &amount_in,
                            &amount_out,
                            &self.market_data,
                        )
                    })
                    .map(|f| (f * 10_000.0).round() as i32);

                let mut quote = OrderQuote::new(
                    order.id().to_string(),
                    QuoteStatus::Success,
                    amount_in,
                    amount_out,
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

        let solve_time = start_time.elapsed();
        record_solve_duration(&self.pool_name, solve_time);

        Ok(SingleOrderQuote::new(order_quote, solve_time.as_millis() as u64))
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

    use rustc_hash::FxHashMap;

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
        propamm_fallback::PROPAMM_FALLBACK_PREFIX,
        types::{OrderSide, Route, RouteResult, Swap},
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
            _graph: &Self::GraphType,
            _market: MarketData,
            _label: Option<crate::feed::market_data::StateLabel>,
            _derived: Option<SharedDerivedDataRef>,
            _order: &Order,
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
            _graph: &Self::GraphType,
            _market: MarketData,
            _label: Option<crate::feed::market_data::StateLabel>,
            _derived: Option<SharedDerivedDataRef>,
            _order: &Order,
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
            _graph: &Self::GraphType,
            _market: MarketData,
            _label: Option<crate::feed::market_data::StateLabel>,
            _derived: Option<SharedDerivedDataRef>,
            _order: &Order,
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

    /// Quotes the pAMM route above through a worker holding `fee_tiers`.
    async fn quote_pamm_route(fee_tiers: SharedFeeTiers) -> Result<SingleOrderQuote, SolveError> {
        let (market, _) = setup_market_weighted(vec![]);
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, PropAMMRouteAlgorithm, 0, "test_pool".to_string())
                .with_fallback_fee_tiers(fee_tiers);

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        worker
            .quote(&ord, SolveParams::default())
            .await
    }

    /// Without a Uniswap V3 pool at the router's fee tier the fallback reverts too, so there is no
    /// fallback amount to check `min_amount_out` against.
    #[tokio::test]
    async fn test_quote_pamm_route_without_fallback_pool() {
        let fee_tiers = SharedFeeTiers::default();
        fee_tiers.set(crate::propamm_fallback::FeeTiers::new(3000));

        let result = quote_pamm_route(fee_tiers).await;

        assert!(
            matches!(result, Err(SolveError::NoRouteFound { .. })),
            "expected the unbacked pAMM route to be dropped, got {result:?}"
        );
    }

    /// Before the fetcher reads the router's tiers there is no tier to price the fallback at, so
    /// the route is dropped rather than priced against a guessed one.
    #[tokio::test]
    async fn test_quote_pamm_route_without_fee_tiers() {
        let result = quote_pamm_route(SharedFeeTiers::default()).await;

        assert!(
            matches!(result, Err(SolveError::NoRouteFound { .. })),
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
    fn test_drops_component_by_excluded_protocol() {
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

        assert!(worker.drops_component(&pamm));
        assert!(!worker.drops_component(&public));
    }

    /// The pAMM rule applies when the graph is built and nowhere else. `drops_component` also
    /// filters state updates and removals, and a component dropped there would keep a frozen price
    /// and could never leave the graph.
    #[test]
    fn test_drops_component_on_build_pamm_without_fallback_pool() {
        let (market, _) = setup_market_weighted(vec![]);
        let worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        );
        let fee_tiers = FeeTiers::new(3000);
        let pamm = component_with_protocol(
            "pamm-1",
            "propammfallback:fermiswap",
            &[token(0x01, "A"), token(0x02, "B")],
        );

        assert!(worker.drops_component_on_build(&pamm, Some(&fee_tiers)));
        assert!(!worker.drops_component(&pamm));
    }

    /// The fee tiers arrive on their own task, so the first graph is usually built without them.
    /// Admitting every pAMM then is what leaves the post-assembly check to catch them.
    #[test]
    fn test_drops_component_on_build_pamm_before_fee_tiers() {
        let (market, _) = setup_market_weighted(vec![]);
        let worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        );

        let pamm = component_with_protocol(
            "pamm-1",
            "propammfallback:fermiswap",
            &[token(0x01, "A"), token(0x02, "B")],
        );
        assert!(!worker.drops_component_on_build(&pamm, None));
    }

    /// An excluded protocol stays excluded when the graph is built, not just when events arrive.
    #[test]
    fn test_drops_component_on_build_excluded_protocol() {
        let (market, _) = setup_market_weighted(vec![]);
        let worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        )
        .with_exclude_protocols(vec!["uniswap_v3".to_string()]);

        let excluded = component_with_protocol("uni-1", "uniswap_v3", &[token(0x01, "A")]);
        assert!(worker.drops_component_on_build(&excluded, None));
    }

    /// Without `exclude_protocols` the worker drops nothing on protocol grounds — the liquidity
    /// scope stays the only reason to leave a component out.
    #[test]
    fn test_drops_component_without_exclusions() {
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

        assert!(!worker.drops_component(&pamm));
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
    fn solve_duration_metric_recorded() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_solve_duration("test_pool", std::time::Duration::from_millis(120));
        });

        let mut solve_seen = false;
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
            solve_seen = true;
        }
        assert!(solve_seen, "solve duration histogram not recorded");
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
