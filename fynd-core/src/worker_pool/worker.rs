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

use num_bigint::{BigInt, BigUint};
use rustc_hash::FxHashSet;
use tokio::sync::{broadcast, Notify};
use tracing::{debug, error, info, warn};
use tycho_simulation::{
    tycho_common::models::{protocol::ProtocolComponent, Address},
    tycho_core::Bytes,
};

use crate::{
    algorithm::{request::SolveRequest, Algorithm},
    derived::{
        computation::ComputationRequirements, events::DerivedDataEvent, tracker::ReadinessTracker,
        SharedDerivedDataRef,
    },
    exclusive_route::{pin_route_commitment, routes_through_exclusive},
    fallback::{has_fallback_leg, manager::PammManager, price_through_fallbacks},
    feed::{
        component_filter::{
            filter_event, is_excluded_protocol, protocol_matches, remove_components,
        },
        events::{MarketEvent, MarketEventHandler},
        exclusivity::is_exclusive,
        market_data::{MarketData, MarketDataView, StateLabel},
    },
    graph::{EdgeWeightUpdaterWithDerived, GraphManager},
    post_process::{PostProcessRequest, PostProcessors, ProcessedRoute, RoutePostProcessor},
    types::{internal::SolveTask, ResolvedRouteConstraints, Route, RouteConstraints, RouteResult},
    worker_pool_router::{ExclusiveAccess, LiquidityScope},
    BlockInfo, Order, OrderQuote, QuoteStatus, SingleOrderQuote, SolveError, SolveParams,
};

/// How long before the task deadline the worker stops waiting for post-processors. A worker sends
/// one answer per task, after it validates and prices the route, and the router reads no answer
/// after the deadline. Without this margin, a post-processor that runs to the deadline would also
/// lose the algorithm's route.
const POST_PROCESS_DEADLINE_MARGIN: Duration = Duration::from_millis(10);

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

/// The constraints this request resolves to against `view`, resolved once and shared.
///
/// Every order of one request, in every pool it reaches, wants the same answer, and expanding a
/// protocol system copies that system's whole component set. The answer depends on the market's
/// components, its unsynced protocols and its unhealthy pools, so a cached one stands until the
/// market's exclusion generation changes.
///
/// Logs what a non-empty filter resolved to, on the one call per generation that resolves it.
fn resolve_exclusions(
    view: &MarketDataView<'_>,
    params: &SolveParams,
) -> Arc<ResolvedRouteConstraints> {
    let generation = view
        .base_market_state()
        .exclusion_generation();
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
            exclusion_generation = generation,
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

/// Checks that `route` is valid, passes the request's route filter, and uses no unsynced or
/// unhealthy pool.
fn check_route(
    route: &Route,
    order: &Order,
    filter: &RouteConstraints,
    exclusions: &ResolvedRouteConstraints,
) -> Result<(), String> {
    route
        .validate()
        .map_err(|err| err.to_string())?;
    validate_route_filter(route, order, filter)?;
    for swap in route.swaps() {
        if exclusions.is_unsynced_pool(swap.component_id()) {
            return Err(format!(
                "route uses pool {} of out-of-sync protocol {}",
                swap.component_id(),
                swap.protocol_component()
                    .protocol_system
            ));
        }
        if exclusions.is_unhealthy_pool(swap.component_id()) {
            return Err(format!(
                "route uses pool {} that fynd-nexus reports as unhealthy",
                swap.component_id()
            ));
        }
    }
    Ok(())
}

/// Check every leg, including split branches, against the original request filter. Reading
/// protocol systems from the returned components also covers pools added after exclusions were
/// resolved. The order's endpoints remain allowed by intermediate-token exclusions, and every
/// token path of the route must pass through every required token.
fn validate_route_filter(
    route: &Route,
    order: &Order,
    filter: &RouteConstraints,
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
    if route_bypasses_required_tokens(route, order, filter.required_tokens()) {
        return Err("route reaches the output token without passing through every required token"
            .to_string());
    }
    Ok(())
}

/// Whether `route` contains a path from the order's input token to its output token that skips
/// at least one required token.
///
/// A required input or output token is passed by every path and adds no constraint.
fn route_bypasses_required_tokens(
    route: &Route,
    order: &Order,
    required_tokens: &FxHashSet<Address>,
) -> bool {
    required_tokens
        .iter()
        .filter(|&token| token != order.token_in() && token != order.token_out())
        .any(|token| reaches_output_avoiding(route, order, token))
}

/// Whether the route's swaps lead from the order's input token to its output token without
/// passing through `avoided`.
///
/// Traverses the swaps as a graph, so a split branch that skips `avoided` is found.
fn reaches_output_avoiding(route: &Route, order: &Order, avoided: &Address) -> bool {
    let mut reached = FxHashSet::from_iter([order.token_in()]);
    let mut frontier = vec![order.token_in()];
    while let Some(token) = frontier.pop() {
        for swap in route.swaps() {
            let next = swap.token_out();
            if swap.token_in() == token && next != avoided && reached.insert(next) {
                frontier.push(next);
            }
        }
    }
    reached.contains(order.token_out())
}

/// Records per-worker-pool queue metrics at task pickup: how long the task waited in the
/// queue and the depth left behind it. Queue wait growing while quote duration stays
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
/// Successful quotes only: a pool that exhausts its timeout returns before this point, and a
/// task the router has already abandoned never reaches it. Those are in
/// `worker_pool_task_duration_seconds`, which also carries the readiness wait this excludes.
/// Read as a pair, the two separate a slow search from a long wait to start one; read alone,
/// this histogram shows a saturated pool as an idle one.
fn record_quote_duration(pool_name: &str, quote_duration: Duration) {
    // The metric keeps its established external name for dashboard compatibility.
    metrics::histogram!("worker_pool_solve_duration_seconds", "pool" => pool_name.to_string())
        .record(quote_duration.as_secs_f64());
}

/// Records how long a task held its worker, from pickup to response, under the outcome it
/// reached: `success`, or the [`SolveError::label`] of the failure.
///
/// Spans the whole task rather than the algorithm's solve alone, so it also covers the wait for
/// derived-data readiness. What this measures is worker occupancy, and a task blocked waiting to
/// become ready occupies its worker exactly as one that is solving.
///
/// Every outcome is recorded, which is what `worker_pool_solve_duration_seconds` beside it
/// cannot do: a pool that fails its work is absent from that histogram, so a saturated pool
/// reads there as an idle one.
///
/// `rate(worker_pool_task_duration_seconds_sum[..]) / workers` is therefore the share of worker
/// slots that are occupied, and not the share of a CPU that is busy. The two differ: a worker
/// waiting on the market lock is occupied, because it cannot take the next task, and burns no
/// CPU while it waits. Read this against `worker_pool_activity_duration_seconds` to see what
/// remaining time went to, and against container CPU to see how much of either was work.
fn record_task_duration(pool_name: &str, duration: Duration, outcome: &'static str) {
    metrics::histogram!(
        "worker_pool_task_duration_seconds",
        "pool" => pool_name.to_string(),
        "outcome" => outcome
    )
    .record(duration.as_secs_f64());
}

/// How long this task may run, or `None` when the router has already given up on it.
///
/// The router starts its deadline when the request arrives, before the task is queued, and
/// abandons every pool that has not answered by then. Two things follow, and this is both of
/// them. A task picked up after the deadline has no reader left, so solving it only takes a
/// worker away from a task that still has one. A task picked up before the deadline may run
/// until the earlier of its pool budget and what remains of the deadline.
///
/// Taking the smaller of the two is what lets the pool budget stay meaningful: a caller that
/// raises `timeout_ms` moves the deadline out and gets the deeper search it asked for, and a
/// caller that does not is never promised a budget the router will cut short.
fn task_budget(pool_timeout: Duration, deadline: Instant, now: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    if remaining.is_zero() {
        return None;
    }
    Some(pool_timeout.min(remaining))
}

/// Records time the worker spent on something other than a solve task.
///
/// The worker's `select!` is `biased` with market events and derived-data events ahead of the
/// task queue, so a task in the queue waits behind whatever this records. That wait is otherwise
/// unattributable: `worker_pool_queue_wait_seconds` shows that a task waited, and nothing shows
/// what it waited for.
///
/// Recorded at the call sites in the worker loop rather than inside the functions themselves,
/// because only the loop knows whether the work delays a task. [`Worker::initialize_graph`] also
/// runs at startup and, once, from inside [`Worker::quote`]: neither competes with task pickup,
/// and the second is already counted by `worker_pool_task_duration_seconds`.
///
/// `activity` partitions the time and the values are mutually exclusive, so they sum without
/// double-counting: `market_event` is a pass through [`Worker::process_event`], `graph_init` is
/// the full rebuild the lagged branch runs *instead of* it, and `derived_event` is the
/// derived-data arm.
///
/// Wall time, which is the right measure of what a task waits behind: each worker owns its
/// thread and a single-threaded runtime, so nothing else runs while the worker is here. It is
/// not a measure of work, because time blocked on the market or derived lock is counted in full
/// and spends no CPU.
fn record_worker_activity_duration(pool_name: &str, duration: Duration, activity: &'static str) {
    metrics::histogram!(
        "worker_pool_activity_duration_seconds",
        "pool" => pool_name.to_string(),
        "activity" => activity
    )
    .record(duration.as_secs_f64());
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

/// Counts a post-processed route that failed the checks, so the worker quoted the algorithm's
/// route instead.
fn record_post_processed_route_rejected(pool_name: &str) {
    metrics::counter!(
        "worker_pool_post_processed_route_rejected_total",
        "pool" => pool_name.to_string()
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
    /// Protocol systems only this worker's algorithm leaves out of its graph. Post-processors
    /// still see them. Empty for every worker pool that does not set
    /// `algorithm_exclude_protocols`.
    algorithm_exclude_protocols: Vec<String>,
    /// The protocol systems the algorithm's graph and route never use: `exclude_protocols` plus
    /// `algorithm_exclude_protocols`.
    algorithm_exclusions: Vec<String>,
    /// Post-processors run on the algorithm's route, in config order. Empty when the pool sets no
    /// `post_processors`.
    post_processors: PostProcessors,
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
            algorithm_exclude_protocols: Vec::new(),
            algorithm_exclusions: Vec::new(),
            post_processors: PostProcessors::default(),
        }
    }

    /// Sets which liquidity this worker ingests.
    pub(crate) fn with_liquidity_scope(mut self, scope: LiquidityScope) -> Self {
        self.liquidity_scope = scope;
        self
    }

    /// Sets the protocol systems this worker never routes through.
    pub(crate) fn with_exclude_protocols(mut self, exclude_protocols: Vec<String>) -> Self {
        self.exclude_protocols = exclude_protocols;
        self.merge_algorithm_exclusions();
        self
    }

    /// Sets the protocol systems only this worker's algorithm never routes through.
    pub(crate) fn with_algorithm_exclude_protocols(
        mut self,
        algorithm_exclude_protocols: Vec<String>,
    ) -> Self {
        self.algorithm_exclude_protocols = algorithm_exclude_protocols;
        self.merge_algorithm_exclusions();
        self
    }

    /// Builds `algorithm_exclusions` from the two exclusion lists.
    fn merge_algorithm_exclusions(&mut self) {
        self.algorithm_exclusions = self
            .exclude_protocols
            .iter()
            .chain(&self.algorithm_exclude_protocols)
            .cloned()
            .collect();
    }

    /// Sets the post-processors this worker runs on every route, in the order given, each with its
    /// registered name.
    pub(crate) fn with_post_processors(
        mut self,
        post_processors: Vec<(String, Box<dyn RoutePostProcessor>)>,
    ) -> Self {
        self.post_processors = PostProcessors::new(self.pool_name.clone(), post_processors);
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
    /// three things together, from one read: the graph, the fallback pool index, and which pAMMs
    /// the graph holds and which it left out. It then calls `initialize` on each post-processor.
    pub async fn initialize_graph(&mut self) {
        let topology = {
            // One read: the index and the topology must describe the same market, or a pAMM whose
            // fallback pool arrived between the two reads stays out until the next rebuild.
            let market = self.market_data.read().await;
            // Owned already, so the graph is built without holding the market lock.
            let topology = market.component_topology();
            let Self { pamm_admission, liquidity_scope, algorithm_exclusions, .. } = self;
            let caller_drops = |component: &ProtocolComponent| {
                should_drop_component(*liquidity_scope, algorithm_exclusions, component)
            };
            let withheld_pamms =
                pamm_admission.withhold_from_graph(&market, &topology, &caller_drops);
            remove_components(market.base_market_state(), topology, &|component| {
                caller_drops(component) || withheld_pamms.contains(&component.id)
            })
        };

        self.graph_manager
            .initialize_graph(&topology);
        self.post_processors
            .initialize(&self.market_data)
            .await;
        self.initialized = true;
    }

    /// Applies the pAMM admission rule to a market event before passing the event to the graph and
    /// the post-processors.
    ///
    /// The graph drops what the pool rule and `algorithm_exclude_protocols` drop. The
    /// post-processors drop only the protocols the pool excludes, so they still learn of a
    /// component only the algorithm leaves out, and of the exclusive components a public-only
    /// pool leaves out.
    pub async fn process_event(&mut self, event: MarketEvent) {
        let market_data = self.market_data.clone();
        let mut post_processor_event = None;
        let graph_event = {
            let market = market_data.read().await;
            let Self {
                pamm_admission,
                liquidity_scope,
                exclude_protocols,
                algorithm_exclude_protocols,
                algorithm_exclusions,
                post_processors,
                ..
            } = self;
            // Post-processors learn of exclusive components even on a public-only pool: whether a
            // request may use them depends on the caller, and the request rule decides.
            let post_processor_drops = |component: &ProtocolComponent| {
                should_drop_component(
                    LiquidityScope::IncludeExclusive,
                    exclude_protocols,
                    component,
                )
            };
            let graph_drops = |component: &ProtocolComponent| {
                should_drop_component(*liquidity_scope, algorithm_exclusions, component)
            };
            let mut event = event;
            pamm_admission.apply_pamm_admission(&market, &graph_drops, &mut event);
            if post_processors.is_empty() ||
                (*liquidity_scope == LiquidityScope::IncludeExclusive &&
                    algorithm_exclude_protocols.is_empty())
            {
                // One event serves both: the graph rule is the post-processor rule, or nobody
                // else reads it.
                filter_event(market.base_market_state(), event, &graph_drops)
            } else {
                // The graph rule drops everything the post-processor rule drops, so it can
                // filter the post-processors' event instead of the original.
                let for_post_processors =
                    filter_event(market.base_market_state(), event, &post_processor_drops);
                let for_graph = filter_event(
                    market.base_market_state(),
                    for_post_processors.clone(),
                    &graph_drops,
                );
                post_processor_event = Some(for_post_processors);
                for_graph
            }
        };
        let post_processor_event = post_processor_event
            .as_ref()
            .unwrap_or(&graph_event);

        match &graph_event {
            MarketEvent::MarketUpdated { .. } => {
                if let Err(e) = self
                    .graph_manager
                    .handle_event(&graph_event)
                    .await
                {
                    // Graph errors currently returned by handle_event are non-fatal, so we just log
                    // them.
                    warn!("Error handling market event: {:?}", e);
                }
                self.post_processors
                    .handle_event(post_processor_event)
                    .await;
            }
        }
    }

    /// Checks what the algorithm's own route cannot break but a post-processor can: the pool's
    /// component rules, the commitment, and `net_amount_out` against the route's amount out for a
    /// sell order. The router subtracts the two as unsigned numbers, so a net above the amount out
    /// would underflow there.
    fn check_processed_route(
        &self,
        order: &Order,
        processed: &RouteResult,
        scope: LiquidityScope,
    ) -> Result<(), String> {
        let route = processed.route();
        for swap in route.swaps() {
            if should_drop_component(scope, &self.exclude_protocols, swap.protocol_component()) {
                return Err(format!(
                    "route uses component {} that this pool leaves out of its graph",
                    swap.component_id()
                ));
            }
        }
        self.check_commitment(order, processed)?;
        if !order.is_sell() {
            return Ok(());
        }
        let Some(output_token) = route.output_token() else {
            return Ok(());
        };
        let amount_out = BigInt::from(route.amount_out(&output_token));
        let net_amount_out = processed.net_amount_out();
        if *net_amount_out > amount_out {
            return Err(format!(
                "net amount out {net_amount_out} is above the route amount out {amount_out}"
            ));
        }
        Ok(())
    }

    /// Checks that the commitment of a processed route can be pinned: a sell order, an exclusive
    /// leg to pin it on, and no more than the route pays. A public-only pool also needs it on every
    /// route with an exclusive leg, because the encoder signs only a leg with a committed amount.
    fn check_commitment(&self, order: &Order, processed: &RouteResult) -> Result<(), String> {
        let route = processed.route();
        let has_exclusive_leg = routes_through_exclusive(route.swaps());
        let Some(commitment) = processed.commitment() else {
            if has_exclusive_leg && self.liquidity_scope == LiquidityScope::PublicOnly {
                return Err("route has an exclusive leg but no commitment".to_string());
            }
            return Ok(());
        };
        if !order.is_sell() {
            return Err("commitment on an order that is not a sell order".to_string());
        }
        if !has_exclusive_leg {
            return Err("commitment on a route with no exclusive leg".to_string());
        }
        let amount_out = route
            .output_token()
            .map(|token| route.amount_out(&token));
        if amount_out.is_none_or(|amount_out| *commitment.committed_amount_out() > amount_out) {
            return Err(format!(
                "commitment {} is above the route amount out",
                commitment.committed_amount_out()
            ));
        }
        Ok(())
    }

    /// Prices the fallback legs of `route` and records the amount out they pay. Does nothing for a
    /// route with no fallback leg.
    ///
    /// A pAMM route is ranked on what its fallbacks pay against `min_amount_out`, so a leg with no
    /// fallback, or one that cannot be priced, makes the route unquotable: there is no floor to
    /// check.
    ///
    /// # Errors
    ///
    /// [`SolveError::RouteRejected`] when a leg cannot be priced, and [`SolveError::NotReady`]
    /// when the request names an overlay the market no longer holds.
    async fn price_fallback_legs(
        &self,
        order: &Order,
        params: &SolveParams,
        route: &mut Route,
    ) -> Result<(), SolveError> {
        if !has_fallback_leg(route) {
            return Ok(());
        }
        // The same view the algorithm solved against, so the fallback is selected and priced on
        // the requested overlay rather than the base state.
        let market = self
            .read_market(params.state_label())
            .await?;
        match price_through_fallbacks(
            route,
            &market,
            self.pamm_admission.fallback_pools(),
            params.route_filter(),
            &self.algorithm_exclusions,
        ) {
            Ok(amount) => {
                route.set_fallback_amount_out(amount);
                Ok(())
            }
            Err(error) => {
                let rejection = error.rejection();
                debug!(order_id = %order.id(), %error, "{rejection}");
                Err(SolveError::route_rejected(order.id(), rejection))
            }
        }
    }

    /// The access the post-processors get: [`ExclusiveAccess::Granted`] when the caller has
    /// exclusive access and the pool leaves exclusive liquidity out of its graph. A pool that
    /// includes exclusive liquidity already routes through it.
    fn post_process_exclusive_access(&self, params: &SolveParams) -> ExclusiveAccess {
        match self.liquidity_scope {
            LiquidityScope::PublicOnly => params.exclusive_access(),
            LiquidityScope::IncludeExclusive => ExclusiveAccess::Denied,
        }
    }

    /// Runs the post-processors on the algorithm's route, which the worker already checked.
    /// Returns the route to quote and the names of the post-processors that made it. The names are
    /// empty when no post-processor replaced the route. A processed route gets its fallback
    /// pricing here; the algorithm's route does not. When the processed route fails the checks,
    /// including that pricing, logs the reason and returns the algorithm's route with no names, so
    /// a post-processor never fails a quote.
    async fn post_process(
        &self,
        order: &Order,
        params: &SolveParams,
        exclusions: &ResolvedRouteConstraints,
        deadline: Instant,
        result: RouteResult,
    ) -> (RouteResult, Vec<String>) {
        if self.post_processors.is_empty() {
            return (result, Vec::new());
        }
        let exclusive_access = self.post_process_exclusive_access(params);
        let scope = match exclusive_access {
            ExclusiveAccess::Granted => LiquidityScope::IncludeExclusive,
            ExclusiveAccess::Denied => self.liquidity_scope,
        };
        let drops_component = |component: &ProtocolComponent| {
            should_drop_component(scope, &self.exclude_protocols, component)
        };
        let request = PostProcessRequest::new(
            order,
            &self.market_data,
            &self.derived_data,
            exclusions,
            deadline
                .checked_sub(POST_PROCESS_DEADLINE_MARGIN)
                .unwrap_or(deadline),
        )
        .with_label(params.state_label())
        .with_pool_drops(&drops_component)
        .with_exclusive_access(exclusive_access);
        let Some(processed) = self
            .post_processors
            .run(&request, &result)
            .await
        else {
            return (result, Vec::new());
        };
        let ProcessedRoute { route: mut processed_route, post_processed_by } = processed;
        let mut checked =
            check_route(processed_route.route(), order, params.route_filter(), exclusions)
                .and_then(|()| self.check_processed_route(order, &processed_route, scope));
        if checked.is_ok() {
            checked = self
                .price_fallback_legs(order, params, processed_route.route_mut())
                .await
                .map_err(|err| err.to_string());
        }
        match checked {
            Ok(()) => (processed_route, post_processed_by),
            Err(err) => {
                warn!(
                    order_id = %order.id(),
                    worker_pool = %self.pool_name,
                    algorithm = self.algorithm.name(),
                    post_processed_by = %post_processed_by.join("+"),
                    error = %err,
                    "post-processed route failed the checks; keeping the algorithm's route"
                );
                record_post_processed_route_rejected(&self.pool_name);
                (result, Vec::new())
            }
        }
    }

    /// Returns a quote for an order, optionally solved against a named state overlay.
    ///
    /// `deadline` is when the caller stops waiting. Post-processors stop
    /// [`POST_PROCESS_DEADLINE_MARGIN`] before it.
    pub async fn quote(
        &mut self,
        order: &Order,
        params: SolveParams,
        deadline: Instant,
    ) -> Result<SingleOrderQuote, SolveError> {
        if !params
            .route_filter()
            .required_tokens()
            .is_empty() &&
            !A::SUPPORTS_REQUIRED_TOKENS
        {
            debug!(order_id = %order.id(), "algorithm does not support required tokens");
            return Err(SolveError::RequiredTokensNotSupported);
        }

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
            .with_shared_exclusions(Arc::clone(&exclusions));
        if let Some(label) = params.state_label().cloned() {
            request = request.with_label(label);
        }
        let result = self
            .algorithm
            .find_best_route(request)
            .await;

        let order_quote = match result {
            Ok(result) => {
                if let Err(err) =
                    check_route(result.route(), order, params.route_filter(), &exclusions)
                {
                    error!(
                        order_id = %order.id(),
                        worker_pool = %self.pool_name,
                        algorithm = self.algorithm.name(),
                        error = %err,
                        "algorithm produced an invalid route"
                    );
                    return Err(SolveError::AlgorithmError(format!(
                        "{} produced an invalid route: {err}",
                        self.algorithm.name()
                    )));
                }

                let (result, post_processed_by) = self
                    .post_process(order, &params, &exclusions, deadline, result)
                    .await;
                // Extract scalar values before consuming result with into_route()
                let net_amount_out = result.net_amount_out().clone();
                let amount_out_net_gas = net_amount_out
                    .to_biguint()
                    .unwrap_or(BigUint::ZERO);
                let gas_price = result.gas_price().clone();
                let commitment = result.commitment().cloned();
                let mut route = result.into_route();

                // The worker priced a processed route in `post_process`.
                if post_processed_by.is_empty() {
                    self.price_fallback_legs(order, &params, &mut route)
                        .await?;
                }

                // This is a first naive approach to getting the total gas of this quote
                // A finer estimation is done during encoding
                let gas_estimate = route.total_gas();
                let amount_in_raw = if order.is_sell() {
                    order.amount().clone()
                } else {
                    route.input_amount().ok_or_else(|| {
                        error!(
                            order_id = %order.id(),
                            algorithm = self.algorithm.name(),
                            "route missing swaps for buy order"
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
                let price_impact_bps_result = super::price_impact::route_price_impact(
                    &route,
                    &amount_in_raw,
                    &amount_out_raw,
                )
                .and_then(super::price_impact::price_impact_to_basis_points);
                let price_impact_outcome = match &price_impact_bps_result {
                    Ok(_) => "computed",
                    Err(err) => err.outcome(),
                };
                record_price_impact_metrics(
                    &self.pool_name,
                    price_impact_started.elapsed(),
                    price_impact_outcome,
                );
                let price_impact_bps = match price_impact_bps_result {
                    Ok(basis_points) => Some(basis_points),
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
                quote.set_post_processed_by(post_processed_by);
                if let Some(commitment) = &commitment {
                    let token_prices = self
                        .derived_data
                        .read()
                        .await
                        .token_prices_shared();
                    quote = pin_route_commitment(&quote, commitment, token_prices.as_deref());
                }
                if let Some(bps) = price_impact_bps {
                    quote = quote.with_price_impact_bps(bps);
                }
                quote
            }
            Err(err) => {
                return Err(solve_error_from_algorithm_error(order.id(), order.amount(), err))
            }
        };

        // The solve alone. Occupancy is timed by the caller, which also sees the failing
        // outcomes this point never reaches.
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
                    // Timed here rather than inside the two calls: this is the point at which
                    // the work holds the worker away from the task queue.
                    let activity_started = Instant::now();
                    match event_result {
                        Ok(event) => {
                            self.process_event(event).await;
                            record_worker_activity_duration(
                                &self.pool_name,
                                activity_started.elapsed(),
                                "market_event",
                            );
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
                            record_worker_activity_duration(
                                &self.pool_name,
                                activity_started.elapsed(),
                                "graph_init",
                            );
                        }
                    }
                }

                // Process derived data events (component depths, token prices)
                derived_result = derived_event_rx.recv(), if !derived_closed => {
                    let activity_started = Instant::now();
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
                    record_worker_activity_duration(
                        &self.pool_name,
                        activity_started.elapsed(),
                        "derived_event",
                    );
                }

                // Get next solve task
                task = task_rx.recv() => {
                    match task.ok() {
                        Some(task) => {
                            let task_id = task.id();
                            // Occupancy starts at pickup and runs to the response, so it covers
                            // the readiness wait as well as the solve.
                            let started = Instant::now();
                            record_task_pickup_metrics(
                                &self.pool_name,
                                task.wait_time(),
                                task_rx.len(),
                            );

                            // The router gives up at its own deadline and reads no answer after
                            // it. Solving anyway takes this worker away from a task that still
                            // has a reader, which lengthens the queue and pushes the next task
                            // past its deadline too.
                            let Some(budget) =
                                task_budget(self.algorithm.timeout(), task.deadline(), started)
                            else {
                                debug!(
                                    self.worker_id,
                                    task_id = %task_id,
                                    waited_ms = task.wait_time().as_millis() as u64,
                                    "task passed its deadline in the queue; not solving"
                                );
                                record_task_duration(
                                    &self.pool_name,
                                    started.elapsed(),
                                    "abandoned",
                                );
                                let waited_ms = task.wait_time().as_millis() as u64;
                                task.respond(Err(SolveError::timeout(waited_ms)));
                                continue;
                            };

                            // Wait for derived data readiness before solving, bounded by what
                            // remains of the deadline as well as by the pool's own budget.
                            if let Err(e) = self.wait_until_ready(budget).await {
                                warn!(
                                    self.worker_id,
                                    task_id = %task_id,
                                    error = %e,
                                    "not ready to solve"
                                );
                                record_task_duration(&self.pool_name, started.elapsed(), e.label());
                                task.respond(Err(e));
                                continue;
                            }

                            // Process the task
                            let result = {
                                let params = task.params().clone();
                                let order = task.order();
                                self.quote(order, params, task.deadline()).await
                            };

                            let outcome = match &result {
                                Ok(_) => "success",
                                Err(e) => e.label(),
                            };
                            record_task_duration(&self.pool_name, started.elapsed(), outcome);

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
    use std::{sync::atomic::Ordering, time::Duration};

    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::{
        tycho_client::feed::{BlockHeader, SynchronizerState},
        tycho_core::{models::token::Token, simulation::protocol_sim::ProtocolSim},
    };

    use super::*;
    use crate::{
        algorithm::{
            most_liquid::DepthAndPrice,
            test_utils::{
                component, component_with_directions, component_with_protocol, order,
                setup_market_weighted, token, MockProtocolSim,
            },
        },
        derived::{
            computation::DerivedComputation,
            computations::{SpotPriceComputation, TokenGasPriceComputation},
            DerivedData,
        },
        fallback::{manager::PammState, FALLBACK_PREFIX},
        graph::petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
        post_process::test_utils::{direct_swap, swap_through, MockPostProcessor},
        tests::metrics::recorded_metrics,
        types::{
            ComponentId, ComponentTopology, OrderSide, Route, RouteConstraints, RouteRejection,
            RouteResult, Swap,
        },
        AlgorithmError,
    };

    /// A deadline far enough away that no test reaches it.
    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

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
            .quote(&ord, SolveParams::default(), far_deadline())
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

        const SUPPORTS_REQUIRED_TOKENS: bool = true;

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

    fn a_to_c_swap(
        id: &str,
        token_in: &tycho_simulation::tycho_common::models::token::Token,
        token_out: &tycho_simulation::tycho_common::models::token::Token,
        amount_in: u64,
        amount_out: u64,
    ) -> Swap {
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
    }

    /// A route from A to C through B. When `split` is true, half goes directly to C and half goes
    /// through B.
    fn a_to_c_route(split: bool) -> Route {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let swaps = if split {
            vec![
                a_to_c_swap("direct", &token_a, &token_c, 50, 40).with_split(0.5),
                a_to_c_swap("p1", &token_a, &token_b, 50, 45),
                a_to_c_swap("p2", &token_b, &token_c, 45, 40),
            ]
        } else {
            vec![
                a_to_c_swap("p1", &token_a, &token_b, 100, 90),
                a_to_c_swap("p2", &token_b, &token_c, 90, 80),
            ]
        };
        let route = Route::new(swaps, FxHashMap::default()).unwrap();
        route.validate().unwrap();
        route
    }

    /// A route from A to C through D only.
    fn a_to_c_route_through_d() -> Route {
        let token_a = token(0x01, "A");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");
        let swaps = vec![
            a_to_c_swap("a_to_d", &token_a, &token_d, 100, 90),
            a_to_c_swap("d_to_c", &token_d, &token_c, 90, 80),
        ];
        let route = Route::new(swaps, FxHashMap::default()).unwrap();
        route.validate().unwrap();
        route
    }

    /// A route from A to C through B and then D.
    fn a_to_c_route_through_b_then_d() -> Route {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");
        let swaps = vec![
            a_to_c_swap("a_to_b", &token_a, &token_b, 100, 90),
            a_to_c_swap("b_to_d", &token_b, &token_d, 90, 80),
            a_to_c_swap("d_to_c", &token_d, &token_c, 80, 70),
        ];
        let route = Route::new(swaps, FxHashMap::default()).unwrap();
        route.validate().unwrap();
        route
    }

    /// A route from A to C that splits in half between a branch through B and one through D.
    fn a_to_c_route_through_b_and_d() -> Route {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");
        let swaps = vec![
            a_to_c_swap("via_b_in", &token_a, &token_b, 50, 45).with_split(0.5),
            a_to_c_swap("via_d_in", &token_a, &token_d, 50, 45),
            a_to_c_swap("via_b_out", &token_b, &token_c, 45, 40),
            a_to_c_swap("via_d_out", &token_d, &token_c, 45, 40),
        ];
        let route = Route::new(swaps, FxHashMap::default()).unwrap();
        route.validate().unwrap();
        route
    }

    #[rstest]
    #[case::pool(RouteConstraints::default().with_excluded_pools(["p2".to_string()]), Some("excluded pool p2"))]
    #[case::protocol(RouteConstraints::default().with_excluded_protocols(["uniswap_v2".to_string()]), Some("excluded protocol uniswap_v2"))]
    #[case::protocol_prefix(RouteConstraints::default().with_excluded_protocols(["uniswap:".to_string()]), None)]
    #[case::intermediate(RouteConstraints::default().with_excluded_tokens([token(0x02, "B").address]), Some("excluded intermediate token"))]
    #[case::empty(RouteConstraints::default(), None)]
    #[case::unrelated(RouteConstraints::default().with_excluded_pools(["other".to_string()]).with_excluded_protocols(["other".to_string()]).with_excluded_tokens([token(0x04, "D").address]), None)]
    #[case::endpoints(RouteConstraints::default().with_excluded_tokens([token(0x01, "A").address, token(0x03, "C").address]), None)]
    #[tokio::test]
    async fn test_quote_enforces_route_filter(
        #[case] filter: RouteConstraints,
        #[case] rejection: Option<&str>,
        #[values(false, true)] split: bool,
    ) {
        let token_a = token(0x01, "A");
        let token_c = token(0x03, "C");
        let route = a_to_c_route(split);
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
            .quote(&ord, SolveParams::default().with_route_filter(filter), far_deadline())
            .await;
        match (rejection, result) {
            (Some(expected), Err(SolveError::AlgorithmError(msg))) => {
                assert!(msg.contains(expected), "unexpected message: {msg}");
            }
            (None, Ok(quote)) => assert_eq!(quote.order().status(), QuoteStatus::Success),
            (expected, result) => panic!("expected rejection {expected:?}, got {result:?}"),
        }
    }

    /// Every branch of the route must pass through every required token. The split route has one
    /// branch straight from A to C; the two-branch route splits between B and D.
    #[rstest]
    #[case::through_required(a_to_c_route(false), &[0x02], None)]
    #[case::split_branch_skips_required(a_to_c_route(true), &[0x02], Some("required token"))]
    #[case::required_unvisited(a_to_c_route(false), &[0x04], Some("required token"))]
    #[case::one_of_two_visited(a_to_c_route(false), &[0x04, 0x02], Some("required token"))]
    #[case::other_of_two_visited(a_to_c_route_through_d(), &[0x04, 0x02], Some("required token"))]
    #[case::both_on_one_path(a_to_c_route_through_b_then_d(), &[0x02, 0x04], None)]
    #[case::endpoint_and_intermediate(a_to_c_route(false), &[0x01, 0x02], None)]
    #[case::endpoint_and_intermediate_skipped(a_to_c_route(true), &[0x01, 0x02], Some("required token"))]
    #[case::input_required(a_to_c_route(true), &[0x01], None)]
    #[case::output_required(a_to_c_route(true), &[0x03], None)]
    #[case::each_branch_has_one_of_two(
        a_to_c_route_through_b_and_d(),
        &[0x02, 0x04],
        Some("required token")
    )]
    #[case::one_branch_skips_required(
        a_to_c_route_through_b_and_d(),
        &[0x02],
        Some("required token")
    )]
    #[tokio::test]
    async fn test_quote_enforces_required_tokens(
        #[case] route: Route,
        #[case] required: &[u8],
        #[case] rejection: Option<&str>,
    ) {
        let token_a = token(0x01, "A");
        let token_c = token(0x03, "C");
        let filter = RouteConstraints::default().with_required_tokens(
            required
                .iter()
                .map(|&byte| token(byte, "R").address),
        );
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
            .quote(&ord, SolveParams::default().with_route_filter(filter), far_deadline())
            .await;

        match (rejection, result) {
            (Some(expected), Err(SolveError::AlgorithmError(msg))) => {
                assert!(msg.contains(expected), "unexpected message: {msg}");
            }
            (None, Ok(quote)) => assert_eq!(quote.order().status(), QuoteStatus::Success),
            (expected, result) => panic!("expected rejection {expected:?}, got {result:?}"),
        }
    }

    /// The worker rejects required tokens before calling an unsupported algorithm; calling
    /// `InvalidRouteAlgorithm` would instead produce an invalid-route error.
    #[tokio::test]
    async fn test_quote_required_tokens_with_unsupported_algorithm() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![]);
        let mut worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            InvalidRouteAlgorithm,
            0,
            "test_pool".to_string(),
        );
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);
        let filter = RouteConstraints::default().with_required_tokens([token_b.address]);

        let result = worker
            .quote(&ord, SolveParams::default().with_route_filter(filter), far_deadline())
            .await;

        assert!(matches!(result, Err(SolveError::RequiredTokensNotSupported)), "{result:?}");
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
    struct SplitRouteAlgorithm {
        reported_spot_price: f64,
    }

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
            // Both pools report a spot price of 2.0. The spot reference is 120 for 60 A through p1,
            // which pays 114, and 80 for the remaining 40 A through p2, which pays 78: 192 out
            // of a total spot reference of 200, a 4% impact.
            let swap_p1 = Swap::new(
                "p1".to_string(),
                "mock".to_string(),
                token_a.address.clone(),
                token_b.address.clone(),
                BigUint::from(60u64),
                BigUint::from(114u64),
                BigUint::from(1u64),
                component("p1", &[token_a.clone(), token_b.clone()]),
                Box::new(MockProtocolSim::new(self.reported_spot_price)),
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
                Box::new(MockProtocolSim::new(self.reported_spot_price)),
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
            RouteConstraints::default().with_excluded_protocols(["uniswap_v2".to_string()]),
        );

        let result = worker
            .quote(&ord, params, far_deadline())
            .await;

        match result {
            Err(SolveError::AlgorithmError(msg)) => {
                assert!(msg.contains("excluded: p1,p2"), "unexpected message: {msg}");
            }
            other => panic!("expected the algorithm's report, got {other:?}"),
        }
    }

    /// Sets the sync status of `uniswap_v2`, the protocol of every test pool, with a 15 s lag.
    async fn set_uniswap_v2_status(market: &MarketData, status: SynchronizerState) {
        market
            .write()
            .await
            .update_sync_status([("uniswap_v2".to_string(), status)], Duration::from_secs(15));
    }

    /// One request's cached exclusions follow the protocol going `Stale` and then `Ready` again.
    #[tokio::test]
    async fn test_quote_unsynced_protocol() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![
            ("p1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("p2", &token_a, &token_b, MockProtocolSim::new(2.0)),
        ]);
        let mut worker = SolverWorker::new(
            market.clone(),
            DerivedData::new_shared(),
            ReportExclusionsAlgorithm,
            0,
            "test_pool".to_string(),
        );
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);
        let params = SolveParams::default();

        let mut reported = Vec::new();
        for status in [
            SynchronizerState::Ready(BlockHeader { number: 1, ..Default::default() }),
            SynchronizerState::Stale(BlockHeader::default()),
            SynchronizerState::Ready(BlockHeader { number: 1, ..Default::default() }),
        ] {
            set_uniswap_v2_status(&market, status).await;
            match worker
                .quote(&ord, params.clone(), far_deadline())
                .await
            {
                Err(SolveError::AlgorithmError(msg)) => reported.push(msg),
                other => panic!("expected the algorithm's report, got {other:?}"),
            }
        }

        assert!(reported[0].ends_with("excluded: "), "unexpected message: {}", reported[0]);
        assert!(reported[1].contains("excluded: p1,p2"), "unexpected message: {}", reported[1]);
        assert!(reported[2].ends_with("excluded: "), "unexpected message: {}", reported[2]);
    }

    #[tokio::test]
    async fn test_quote_route_through_unsynced_protocol() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) =
            setup_market_weighted(vec![("p1", &token_a, &token_b, MockProtocolSim::new(2.0))]);
        set_uniswap_v2_status(&market, SynchronizerState::Stale(BlockHeader::default())).await;
        let swap = Swap::new(
            "p1".to_string(),
            "uniswap_v2".to_string(),
            token_a.address.clone(),
            token_b.address.clone(),
            BigUint::from(100u64),
            BigUint::from(90u64),
            BigUint::from(1u64),
            component("p1", &[token_a.clone(), token_b.clone()]),
            Box::new(MockProtocolSim::new(2.0)),
        );
        let route = Route::new(vec![swap], FxHashMap::default()).expect("non-empty route");
        let mut worker = SolverWorker::new(
            market,
            DerivedData::new_shared(),
            FilterIgnoringAlgorithm(route),
            0,
            "test_pool".to_string(),
        );

        let result = worker
            .quote(
                &order(&token_a, &token_b, 100, OrderSide::Sell),
                SolveParams::default(),
                far_deadline(),
            )
            .await;

        match result {
            Err(SolveError::AlgorithmError(msg)) => {
                assert!(
                    msg.contains("pool p1 of out-of-sync protocol uniswap_v2"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected the route to be rejected, got {other:?}"),
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
        let mut worker = SolverWorker::new(
            market,
            derived,
            SplitRouteAlgorithm { reported_spot_price: 2.0 },
            0,
            "test_pool".to_string(),
        );
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let quote = worker
            .quote(&ord, SolveParams::default(), far_deadline())
            .await
            .expect("split route must quote");

        assert_eq!(quote.order().price_impact_bps(), Some(400));
    }

    #[tokio::test]
    async fn test_quote_omits_unrepresentable_price_impact() {
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
            SplitRouteAlgorithm { reported_spot_price: 1e-300 },
            0,
            "test_pool".to_string(),
        );
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let quote = worker
            .quote(&ord, SolveParams::default(), far_deadline())
            .await
            .expect("an unrepresentable price impact must not fail the quote");

        assert_eq!(quote.order().price_impact_bps(), None);
        assert_eq!(quote.order().amount_out(), &BigUint::from(192u64));
    }

    /// Mock algorithm that returns a route without a token map, which the price-impact
    /// calculation cannot price.
    struct UnpricedRouteAlgorithm;

    impl Algorithm for UnpricedRouteAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "unpriced_route_mock"
        }

        async fn find_best_route(
            &self,
            _request: SolveRequest<'_, Self::GraphType>,
        ) -> Result<RouteResult, AlgorithmError> {
            let token_a = token(0x01, "A");
            let token_b = token(0x02, "B");
            let swap = Swap::new(
                "p1".to_string(),
                "mock".to_string(),
                token_a.address.clone(),
                token_b.address.clone(),
                BigUint::from(100u64),
                BigUint::from(190u64),
                BigUint::from(1u64),
                component("p1", &[token_a.clone(), token_b.clone()]),
                Box::new(MockProtocolSim::new(2.0)),
            );
            let route = Route::new(vec![swap], FxHashMap::default()).expect("non-empty route");
            Ok(RouteResult::new(route, num_bigint::BigInt::from(190), BigUint::from(1u64)))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    #[tokio::test]
    async fn test_quote_omits_price_impact_when_route_cannot_be_priced() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) =
            setup_market_weighted(vec![("p1", &token_a, &token_b, MockProtocolSim::new(2.0))]);
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, UnpricedRouteAlgorithm, 0, "test_pool".to_string());
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let quote = worker
            .quote(&ord, SolveParams::default(), far_deadline())
            .await
            .expect("a route the calculation cannot price still quotes");

        assert_eq!(quote.order().price_impact_bps(), None);
        assert_eq!(quote.order().amount_out(), &BigUint::from(190u64));
    }

    /// Mock algorithm returning one `fallback:` leg for the (A, B) pair; the market each
    /// test builds decides whether the leg gets a fallback.
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
                format!("{FALLBACK_PREFIX}fermiswap"),
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

    /// Quotes the pAMM route above against `market`, under the request filter `filter`.
    ///
    /// The worker indexes the market's fallback pools on `initialize_graph`, so the market has to
    /// be in place before the quote.
    async fn quote_pamm_route_against(
        market: MarketData,
        filter: RouteConstraints,
    ) -> Result<SingleOrderQuote, SolveError> {
        let derived = DerivedData::new_shared();
        let mut worker =
            SolverWorker::new(market, derived, PropAMMRouteAlgorithm, 0, "test_pool".to_string());
        worker.initialize_graph().await;

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        worker
            .quote(&ord, SolveParams::default().with_route_filter(filter), far_deadline())
            .await
    }

    /// A market holding one Uniswap V3 pool for the route's pair, but with too little liquidity to
    /// price the swap the fallback would make.
    fn market_with_unpriceable_fallback_pool() -> MarketData {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let fallback = component_with_protocol(
            FALLBACK_POOL,
            "uniswap_v3",
            &[token_a.clone(), token_b.clone()],
        );

        // Built on the shared setup so the market carries the block info and gas price a quote
        // needs; only the fallback pool is added here.
        let (market, _) = setup_market_weighted(vec![]);
        {
            let mut state = market.try_write().expect("uncontended");
            state.upsert_tokens([token_a, token_b]);
            state.upsert_components([fallback]);
            state.update_states([(
                FALLBACK_POOL.to_string(),
                Box::new(MockProtocolSim::new(2.0).with_liquidity(1)) as Box<dyn ProtocolSim>,
            )]);
        }
        market
    }

    /// The request's filter reaches the pool a pAMM leg settles through, so excluding every
    /// candidate, by pool or by protocol, leaves the leg no fallback.
    #[rstest]
    #[case::pool(RouteConstraints::default().with_excluded_pools([FALLBACK_POOL.to_string()]))]
    #[case::protocol(RouteConstraints::default().with_excluded_protocols(["uniswap_v3".to_string()]))]
    #[tokio::test]
    async fn test_quote_pamm_route_with_excluded_fallback(#[case] filter: RouteConstraints) {
        let result =
            quote_pamm_route_against(market_with_unpriceable_fallback_pool(), filter).await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected { reason: RouteRejection::FallbackExcluded, .. })
            ),
            "expected rejection before fallback simulation, got {result:?}"
        );
    }

    /// The algorithm's fallback pricing leaves out `algorithm_exclude_protocols`, as it does the
    /// pool's `exclude_protocols`. The control is the test below, where the pool prices.
    #[tokio::test]
    async fn test_quote_pamm_route_with_algorithm_excluded_fallback() {
        let mut worker = SolverWorker::new(
            market_with_unpriceable_fallback_pool(),
            DerivedData::new_shared(),
            PropAMMRouteAlgorithm,
            0,
            "test_pool".to_string(),
        )
        .with_algorithm_exclude_protocols(vec!["uniswap_v3".to_string()]);
        worker.initialize_graph().await;
        let ord = order(&token(0x01, "A"), &token(0x02, "B"), 100, OrderSide::Sell);

        let result = worker
            .quote(&ord, SolveParams::default(), far_deadline())
            .await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected { reason: RouteRejection::FallbackExcluded, .. })
            ),
            "expected the excluded fallback pool to leave the leg no fallback, got {result:?}"
        );
    }

    /// Without a fallback pool for the pair the fallback reverts too, so there is no fallback
    /// amount to check `min_amount_out` against.
    #[tokio::test]
    async fn test_quote_pamm_route_without_fallback_pool() {
        let (market, _) = setup_market_weighted(vec![]);

        let result = quote_pamm_route_against(market, RouteConstraints::default()).await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected { reason: RouteRejection::FallbackPoolMissing, .. })
            ),
            "expected the unbacked pAMM route to be dropped, got {result:?}"
        );
    }

    /// A fallback pool that cannot price the swap leaves no amount to check `min_amount_out`
    /// against, so the route is dropped rather than ranked on the pAMM's own amount.
    #[tokio::test]
    async fn test_quote_pamm_route_with_unpriceable_fallback() {
        let result = quote_pamm_route_against(
            market_with_unpriceable_fallback_pool(),
            RouteConstraints::default(),
        )
        .await;

        assert!(
            matches!(
                result,
                Err(SolveError::RouteRejected {
                    reason: RouteRejection::FallbackNotSimulatable,
                    ..
                })
            ),
            "expected the unpriceable fallback to drop the route, got {result:?}"
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
        .with_exclude_protocols(vec![FALLBACK_PREFIX.to_string()]);

        let pamm = component_with_protocol("pamm-1", "fallback:fermiswap", &[token(0x01, "A")]);
        let public = component("uni-1", &[token(0x01, "A")]);

        assert!(should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, &pamm));
        assert!(!should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, &public));
    }

    /// The pAMM every admission test decides.
    const PAMM: &str = "pamm-1";

    /// The pool the pAMM falls back to. A real address, because selection refuses a candidate
    /// whose component id it cannot encode as a pool for the router.
    const FALLBACK_POOL: &str = "0x3333333333333333333333333333333333333333";

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
            "fallback:fermiswap",
            &[token_a.clone(), token_b.clone()],
        );
        let mut state = market.try_write().expect("uncontended");
        state.upsert_tokens([token_a, token_b]);
        state.upsert_components([pamm]);
    }

    /// A second candidate pool on the same pair, so the pAMM keeps a fallback when one leaves.
    const SECOND_FALLBACK_POOL: &str = "0x4444444444444444444444444444444444444444";

    /// Adds a `uniswap_v2` pool serving the same pair as `FALLBACK_POOL`.
    fn add_second_fallback_pool(market: &MarketData) {
        let second = component_with_protocol(
            SECOND_FALLBACK_POOL,
            "uniswap_v2",
            &[token(0x01, "A"), token(0x02, "B")],
        );
        let mut state = market.try_write().expect("uncontended");
        state.upsert_components([second]);
    }

    /// Adds the Uniswap V3 pool the pAMM falls back to.
    fn add_fallback_pool(market: &MarketData) {
        let fallback = component_with_protocol(
            FALLBACK_POOL,
            "uniswap_v3",
            &[token(0x01, "A"), token(0x02, "B")],
        );
        let mut state = market.try_write().expect("uncontended");
        state.upsert_components([fallback]);
    }

    /// A worker whose fallback pool index describes `market`.
    fn admission_worker(market: MarketData) -> SolverWorker<MockAlgorithm> {
        let mut worker = SolverWorker::new(
            market.clone(),
            DerivedData::new_shared(),
            MockAlgorithm::new(),
            0,
            "test_pool".to_string(),
        );
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        worker
            .pamm_admission
            .rebuild_pools(&view);
        worker
    }

    /// A `MarketUpdated` event that adds one component, by id.
    fn added_component_event(component_id: &str) -> MarketEvent {
        MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(
                component_id.to_string(),
                ComponentTopology::default(),
            )]),
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
        let caller_drops = |component: &ProtocolComponent| {
            should_drop_component(worker.liquidity_scope, &worker.exclude_protocols, component)
        };
        worker
            .pamm_admission
            .apply_pamm_admission(&view, &caller_drops, &mut event);
        filter_event(view.base_market_state(), event, &caller_drops)
    }

    /// A pAMM only reaches the chain through a fallback pool, so the worker admits one exactly
    /// when this market holds a candidate for its pair.
    #[rstest]
    #[case::without_fallback_pool(market_with_pamm(), false)]
    #[case::with_fallback_pool(market_with_pamm_and_fallback(), true)]
    fn test_apply_pamm_admission(#[case] market: MarketData, #[case] expect_admitted: bool) {
        let mut worker = admission_worker(market.clone());

        let event = admit(&mut worker, &market, added_component_event(PAMM));

        let admitted = added_ids(&event) == vec![PAMM.to_string()];
        assert_eq!(admitted, expect_admitted, "additions were {:?}", added_ids(&event));
    }

    /// The event that removes a Uniswap V3 pool does not name the pAMMs that fell back to it, so
    /// the pAMM would otherwise stay in the graph and produce routes the worker cannot price.
    #[test]
    fn test_apply_pamm_admission_after_the_fallback_pool_leaves() {
        let market = market_with_pamm_and_fallback();
        let mut worker = admission_worker(market.clone());
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

    /// A pAMM is evicted when its last candidate pool leaves, not when any one does — the whole
    /// point of choosing a pool per leg is that one pair can have several.
    #[test]
    fn test_apply_pamm_admission_keeps_a_pamm_behind_a_second_fallback_pool() {
        let market = market_with_pamm_and_fallback();
        add_second_fallback_pool(&market);
        let mut worker = admission_worker(market.clone());
        admit(&mut worker, &market, added_component_event(PAMM));

        let event = admit(&mut worker, &market, removed_component_event(FALLBACK_POOL));

        assert!(
            !removed_ids(&event).contains(&PAMM.to_string()),
            "the pAMM still has a fallback pool: {:?}",
            removed_ids(&event)
        );
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Admitted));
    }

    /// A pAMM re-sent with other swap directions can turn unbacked while the graph holds it. The
    /// event then has to take it out of the graph.
    #[test]
    fn test_apply_pamm_admission_evicts_a_resent_pamm_that_turns_unbacked() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let listing_pamm = |directions: &[(&Token, &Token)]| {
            let mut pamm =
                component_with_directions(PAMM, &[a.clone(), b.clone(), c.clone()], directions);
            pamm.protocol_system = "fallback:fermiswap".to_string();
            pamm
        };
        let market = market_with_pamm_and_fallback();
        market
            .try_write()
            .expect("uncontended")
            .upsert_components([listing_pamm(&[(&a, &b)])]);
        let mut worker = admission_worker(market.clone());
        admit(&mut worker, &market, added_component_event(PAMM));
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Admitted));
        market
            .try_write()
            .expect("uncontended")
            .upsert_components([listing_pamm(&[(&a, &c)])]);

        let event = admit(&mut worker, &market, added_component_event(PAMM));

        assert_eq!(removed_ids(&event), vec![PAMM.to_string()]);
        assert!(added_ids(&event).is_empty());
        assert_eq!(worker.pamm_admission.state_of(PAMM), Some(PammState::Withheld));
    }

    /// The admitted counter counts a pAMM when it enters the graph, not each time the market
    /// sends it again.
    #[test]
    fn test_apply_pamm_admission_counts_an_admitted_pamm_once() {
        let market = market_with_pamm_and_fallback();
        let mut worker = admission_worker(market.clone());
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            admit(&mut worker, &market, added_component_event(PAMM));
            admit(&mut worker, &market, added_component_event(PAMM));
        });

        let admitted: Vec<_> = recorded_metrics(&snapshotter)
            .into_iter()
            .filter(|(name, labels, _)| {
                name == "propamm_admissions_total" &&
                    labels.contains(&"outcome=admitted".to_string())
            })
            .map(|(_, _, value)| value)
            .collect();
        assert_eq!(admitted, vec![metrics_util::debugging::DebugValue::Counter(1)]);
    }

    /// The market names a component in `added_components` once, so the event that adds the
    /// fallback pool does not name the pAMM. The withheld set is what lets the pAMM back in
    /// before the next rebuild.
    #[test]
    fn test_apply_pamm_admission_after_the_fallback_pool_arrives() {
        let market = market_with_pamm();
        let mut worker = admission_worker(market.clone());
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
        let mut worker = admission_worker(market.clone());
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
        let mut worker = admission_worker(market.clone());
        worker.initialize_graph().await;
        add_pamm(&market);
        add_fallback_pool(&market);
        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([
                (PAMM.to_string(), ComponentTopology::default()),
                (FALLBACK_POOL.to_string(), ComponentTopology::default()),
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

    /// A pAMM falls back through the router, not through this worker's graph, so a Uniswap V3
    /// pool the worker excludes still backs it. The block that adds that pool names no pAMM, and
    /// the worker's own filter empties it, so the pAMM pass must read the event the market
    /// broadcast rather than the filtered one.
    #[tokio::test]
    async fn test_process_event_readmits_a_pamm_behind_an_excluded_fallback_pool() {
        let market = market_with_pamm();
        let mut worker =
            admission_worker(market.clone()).with_exclude_protocols(vec!["uniswap_v3".to_string()]);
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
        let mut worker =
            admission_worker(market).with_exclude_protocols(vec![FALLBACK_PREFIX.to_string()]);

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

        let pamm = component_with_protocol("pamm-1", "fallback:fermiswap", &[token(0x01, "A")]);

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

    /// One sample of `name` in `recorded`, as (labels, seconds).
    fn one_histogram_sample(
        recorded: &[crate::tests::metrics::Recorded],
        name: &str,
    ) -> (Vec<String>, f64) {
        use metrics_util::debugging::DebugValue;

        let (_, labels, value) = recorded
            .iter()
            .find(|(recorded_name, _, _)| recorded_name == name)
            .unwrap_or_else(|| panic!("{name} not recorded"));
        let DebugValue::Histogram(samples) = value else {
            panic!("expected histogram, got {value:?}");
        };
        assert_eq!(samples.len(), 1, "{name} recorded {} samples", samples.len());
        (labels.clone(), samples[0].into_inner())
    }

    #[test]
    fn test_quote_duration_metric_recorded() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_quote_duration("test_pool", std::time::Duration::from_millis(120));
        });

        let recorded = crate::tests::metrics::recorded_metrics(&snapshotter);
        let (labels, seconds) =
            one_histogram_sample(&recorded, "worker_pool_solve_duration_seconds");
        assert_eq!(labels, vec!["pool=test_pool".to_string()]);
        assert!((seconds - 0.120).abs() < 1e-9);
    }

    #[rstest]
    #[case("success")]
    #[case("timeout")]
    #[case("not_ready")]
    fn test_task_duration_recorded_for_every_outcome(#[case] outcome: &'static str) {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_task_duration("test_pool", std::time::Duration::from_millis(120), outcome);
        });

        let recorded = crate::tests::metrics::recorded_metrics(&snapshotter);
        let (labels, seconds) =
            one_histogram_sample(&recorded, "worker_pool_task_duration_seconds");
        assert_eq!(labels, vec!["pool=test_pool".to_string(), format!("outcome={outcome}")]);
        assert!((seconds - 0.120).abs() < 1e-9);
    }

    #[test]
    fn test_expired_task_has_no_budget() {
        let now = Instant::now();
        let deadline = now - Duration::from_millis(1);
        assert_eq!(task_budget(Duration::from_millis(1000), deadline, now), None);
    }

    /// The router gives up at its own deadline, so a budget past it buys nothing: the pool would
    /// keep searching for an answer no caller is still waiting for.
    #[test]
    fn test_budget_is_capped_by_the_router_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(100);
        assert_eq!(
            task_budget(Duration::from_millis(1000), deadline, now),
            Some(Duration::from_millis(100))
        );
    }

    /// A caller that raises its own timeout gets the deeper search it asked for, bounded by the
    /// pool's configured budget rather than by the router default.
    #[test]
    fn test_budget_keeps_the_pool_limit_when_the_deadline_is_further_out() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(5000);
        assert_eq!(
            task_budget(Duration::from_millis(1000), deadline, now),
            Some(Duration::from_millis(1000))
        );
    }

    /// The three activities partition the worker's non-solving time, so each must carry its own
    /// label and none may be recorded under another's name.
    #[rstest]
    #[case("market_event")]
    #[case("graph_init")]
    #[case("derived_event")]
    fn test_worker_activity_duration_recorded(#[case] activity: &'static str) {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_worker_activity_duration(
                "test_pool",
                std::time::Duration::from_micros(250),
                activity,
            );
        });

        let recorded = crate::tests::metrics::recorded_metrics(&snapshotter);
        let (labels, seconds) =
            one_histogram_sample(&recorded, "worker_pool_activity_duration_seconds");
        assert_eq!(labels, vec!["pool=test_pool".to_string(), format!("activity={activity}")]);
        assert!((seconds - 0.000_25).abs() < 1e-12);
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

    fn replace_route(result: RouteResult, swaps: Vec<Swap>, net_amount_out: u64) -> RouteResult {
        let route = Route::new(swaps, result.route().tokens().clone()).expect("non-empty swaps");
        RouteResult::new(route, BigInt::from(net_amount_out), result.gas_price().clone())
    }

    fn rfq_route(result: RouteResult) -> RouteResult {
        replace_route(result, vec![direct_swap("rfq:bebop", 120)], 118)
    }

    fn worker_with_post_processor(
        post_processor: MockPostProcessor,
    ) -> SolverWorker<SplitRouteAlgorithm> {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![
            ("p1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("p2", &token_a, &token_b, MockProtocolSim::new(2.0)),
        ]);
        SolverWorker::new(
            market,
            DerivedData::new_shared(),
            SplitRouteAlgorithm { reported_spot_price: 2.0 },
            0,
            "test_pool".to_string(),
        )
        .with_post_processors(vec![("rfq".to_string(), Box::new(post_processor))])
    }

    fn a_to_b_order() -> Order {
        order(&token(0x01, "A"), &token(0x02, "B"), 100, OrderSide::Sell)
    }

    #[tokio::test]
    async fn test_quote_post_processed_route() {
        let mut worker = worker_with_post_processor(MockPostProcessor::new(rfq_route));

        let quote = worker
            .quote(&a_to_b_order(), SolveParams::default(), far_deadline())
            .await
            .expect("the processed route is valid");

        let route = quote
            .order()
            .route()
            .expect("the quote has a route");
        assert_eq!(route.swaps().len(), 1);
        assert_eq!(
            route.swaps()[0]
                .protocol_component()
                .protocol_system,
            "rfq:bebop"
        );
        assert_eq!(quote.order().amount_out(), &BigUint::from(120u64));
        assert_eq!(quote.order().amount_out_net_gas(), &BigUint::from(118u64));
        assert_eq!(quote.order().post_processed_by(), ["rfq".to_string()]);
    }

    #[tokio::test]
    async fn test_quote_post_processor_request() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let recorder = Arc::clone(&seen);
        let recording = MockPostProcessor::with_request(move |request, result| {
            *recorder.lock().unwrap() = Some((
                request.label().cloned(),
                request
                    .exclusions()
                    .excludes_pool("unused"),
            ));
            rfq_route(result)
        });
        let mut worker = worker_with_post_processor(recording);
        worker
            .market_data
            .register_labeled_state("overlay".to_string(), FxHashMap::default(), u64::MAX)
            .await;
        let params = SolveParams::default()
            .with_state_label("overlay".to_string())
            .with_route_filter(
                RouteConstraints::default().with_excluded_pools(["unused".to_string()]),
            );

        worker
            .quote(&a_to_b_order(), params, far_deadline())
            .await
            .expect("the routes avoid the excluded pool");

        assert_eq!(*seen.lock().unwrap(), Some((Some("overlay".to_string()), true)));
    }

    /// The worker stops waiting for post-processors before the task deadline, so the route it had
    /// still reaches the router in time.
    #[tokio::test]
    async fn test_quote_post_processor_deadline() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let recorder = Arc::clone(&seen);
        let recording = MockPostProcessor::with_request(move |request, result| {
            *recorder.lock().unwrap() = Some(request.deadline());
            result
        });
        let mut worker = worker_with_post_processor(recording);
        let deadline = far_deadline();

        worker
            .quote(&a_to_b_order(), SolveParams::default(), deadline)
            .await
            .expect("the algorithm's route is valid");

        assert_eq!(*seen.lock().unwrap(), Some(deadline - POST_PROCESS_DEADLINE_MARGIN));
    }

    fn disconnected_route(result: RouteResult) -> RouteResult {
        let (token_c, token_d) = (token(0x03, "C"), token(0x04, "D"));
        let leg = swap_through(component("p3", &[token_c, token_d]), 80);
        replace_route(result, vec![direct_swap("rfq:bebop", 90), leg], 80)
    }

    fn exclusive_route(result: RouteResult) -> RouteResult {
        let tokens = [token(0x01, "A"), token(0x02, "B")];
        let mut leg = component("exclusive-leg", &tokens);
        crate::feed::exclusivity::mark_exclusive(&mut leg);
        replace_route(result, vec![swap_through(leg, 120)], 118)
    }

    fn inflated_route(result: RouteResult) -> RouteResult {
        replace_route(result, vec![direct_swap("rfq:bebop", 120)], 121)
    }

    #[rstest]
    #[case::disconnected_swaps(disconnected_route, &[], &[])]
    #[case::request_excluded_protocol(rfq_route, &[], &["rfq:bebop"])]
    #[case::pool_excluded_protocol(rfq_route, &["rfq:"], &[])]
    #[case::exclusive_component(exclusive_route, &[], &[])]
    #[case::net_above_amount_out(inflated_route, &[], &[])]
    #[tokio::test]
    async fn test_quote_with_rejected_post_processed_route(
        #[case] rewrite: fn(RouteResult) -> RouteResult,
        #[case] pool_excluded: &[&str],
        #[case] request_excluded: &[&str],
    ) {
        let mut worker = worker_with_post_processor(MockPostProcessor::new(rewrite))
            .with_exclude_protocols(
                pool_excluded
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            );
        let params = SolveParams::default().with_route_filter(
            RouteConstraints::default().with_excluded_protocols(
                request_excluded
                    .iter()
                    .map(ToString::to_string),
            ),
        );

        let quote = worker
            .quote(&a_to_b_order(), params, far_deadline())
            .await
            .expect("a failed post-processor does not fail the quote");

        let route = quote
            .order()
            .route()
            .expect("the quote has a route");
        let components: Vec<&str> = route
            .swaps()
            .iter()
            .map(|swap| swap.component_id())
            .collect();
        assert_eq!(components, ["p1", "p2"]);
        assert_eq!(quote.order().amount_out_net_gas(), &BigUint::from(192u64));
        assert!(quote
            .order()
            .post_processed_by()
            .is_empty());
    }

    /// The post-processors get access only on a public-only pool: a pool that includes exclusive
    /// liquidity already routes through it.
    #[rstest]
    #[case::public_only_granted(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Granted,
        ExclusiveAccess::Granted
    )]
    #[case::public_only_denied(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Denied,
        ExclusiveAccess::Denied
    )]
    #[case::include_exclusive_granted(
        LiquidityScope::IncludeExclusive,
        ExclusiveAccess::Granted,
        ExclusiveAccess::Denied
    )]
    #[tokio::test]
    async fn test_quote_post_processor_exclusive_access(
        #[case] scope: LiquidityScope,
        #[case] access: ExclusiveAccess,
        #[case] expected: ExclusiveAccess,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let recorder = Arc::clone(&seen);
        let recording = MockPostProcessor::with_request(move |request, result| {
            *recorder.lock().unwrap() = Some(request.exclusive_access());
            result
        });
        let mut worker = worker_with_post_processor(recording).with_liquidity_scope(scope);
        let params = SolveParams::default().with_exclusive_access(access);

        worker
            .quote(&a_to_b_order(), params, far_deadline())
            .await
            .expect("the algorithm's route is valid");

        assert_eq!(*seen.lock().unwrap(), Some(expected));
    }

    fn committed_exclusive_route(committed: u64) -> impl Fn(RouteResult) -> RouteResult {
        move |result| {
            exclusive_route(result).with_commitment(BigUint::from(committed), BigUint::from(100u64))
        }
    }

    fn committed_public_route(result: RouteResult) -> RouteResult {
        rfq_route(result).with_commitment(BigUint::from(110u64), BigUint::from(100u64))
    }

    #[rstest]
    #[case::pinned(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Granted,
        Box::new(committed_exclusive_route(110)),
        true
    )]
    #[case::access_denied(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Denied,
        Box::new(committed_exclusive_route(110)),
        false
    )]
    #[case::no_commitment(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Granted,
        Box::new(exclusive_route),
        false
    )]
    #[case::commitment_above_amount_out(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Granted,
        Box::new(committed_exclusive_route(121)),
        false
    )]
    #[case::commitment_without_exclusive_leg(
        LiquidityScope::PublicOnly,
        ExclusiveAccess::Granted,
        Box::new(committed_public_route),
        false
    )]
    #[tokio::test]
    async fn test_quote_with_committed_route(
        #[case] scope: LiquidityScope,
        #[case] access: ExclusiveAccess,
        #[case] rewrite: Box<dyn Fn(RouteResult) -> RouteResult + Send + Sync>,
        #[case] pinned: bool,
    ) {
        let mut worker =
            worker_with_post_processor(MockPostProcessor::new(rewrite)).with_liquidity_scope(scope);
        let params = SolveParams::default().with_exclusive_access(access);

        let quote = worker
            .quote(&a_to_b_order(), params, far_deadline())
            .await
            .expect("a rejected post-processed route does not fail the quote");

        let order = quote.order();
        let components: Vec<&str> = order
            .route()
            .expect("the quote has a route")
            .swaps()
            .iter()
            .map(|swap| swap.component_id())
            .collect();
        if !pinned {
            assert_eq!(components, ["p1", "p2"]);
            assert_eq!(order.amount_out_net_gas(), &BigUint::from(192u64));
            assert!(order.surplus_amount().is_none());
            return;
        }
        assert_eq!(components, ["exclusive-leg"]);
        assert_eq!(order.post_processed_by(), ["rfq".to_string()]);
        assert_eq!(order.amount_out(), &BigUint::from(110u64));
        assert_eq!(order.amount_out_net_gas(), &BigUint::from(108u64));
        assert_eq!(order.surplus_amount(), Some(&BigUint::from(10u64)));
        assert_eq!(order.committed_amount_out(), Some(&BigUint::from(110u64)));
        let leg = &order.route().unwrap().swaps()[0];
        assert_eq!(leg.committed_amount_out(), Some(&BigUint::from(110u64)));
    }

    /// The value of `worker_pool_post_processed_route_rejected_total` for the test pool.
    fn rejected_route_count(snapshotter: &metrics_util::debugging::Snapshotter) -> u64 {
        use metrics_util::debugging::DebugValue;

        let recorded = crate::tests::metrics::recorded_metrics(snapshotter);
        let (_, labels, value) = recorded
            .iter()
            .find(|(name, _, _)| name == "worker_pool_post_processed_route_rejected_total")
            .expect("the rejection counter is recorded");
        assert_eq!(labels, &vec!["pool=test_pool".to_string()]);
        let DebugValue::Counter(count) = value else {
            panic!("expected counter, got {value:?}");
        };
        *count
    }

    fn fallback_rfq_route(result: RouteResult) -> RouteResult {
        replace_route(result, vec![direct_swap("fallback:rfq:bebop", 120)], 118)
    }

    /// A processed route whose fallback leg has no fallback pool is rejected like any other
    /// failed check: the quote uses the algorithm's route.
    #[tokio::test]
    async fn test_quote_post_processed_route_with_unbacked_fallback_leg() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let mut worker = worker_with_post_processor(MockPostProcessor::new(fallback_rfq_route));

        let quote = worker
            .quote(&a_to_b_order(), SolveParams::default(), far_deadline())
            .await
            .expect("a failed post-processor does not fail the quote");

        let components: Vec<&str> = quote
            .order()
            .route()
            .expect("the quote has a route")
            .swaps()
            .iter()
            .map(|swap| swap.component_id())
            .collect();
        assert_eq!(components, ["p1", "p2"]);
        assert!(quote
            .order()
            .post_processed_by()
            .is_empty());
        assert_eq!(rejected_route_count(&snapshotter), 1);
    }

    /// The fallback amount of a processed route reaches the quoted route.
    #[tokio::test]
    async fn test_quote_post_processed_route_prices_fallback_leg() {
        let mut worker = worker_with_post_processor(MockPostProcessor::new(fallback_rfq_route));
        add_fallback_pool(&worker.market_data);
        worker
            .market_data
            .try_write()
            .expect("uncontended")
            .update_states([(
                FALLBACK_POOL.to_string(),
                Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
            )]);

        let quote = worker
            .quote(&a_to_b_order(), SolveParams::default(), far_deadline())
            .await
            .expect("the fallback leg has a pool");

        let route = quote
            .order()
            .route()
            .expect("the quote has a route");
        assert_eq!(route.swaps()[0].component_id(), "rfq-leg");
        assert!(route.fallback_amount_out().is_some());
        assert_eq!(quote.order().post_processed_by(), ["rfq".to_string()]);
    }

    #[tokio::test]
    async fn test_quote_rejected_post_processed_route_metric() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let mut worker = worker_with_post_processor(MockPostProcessor::new(inflated_route));

        worker
            .quote(&a_to_b_order(), SolveParams::default(), far_deadline())
            .await
            .expect("a failed post-processor does not fail the quote");

        assert_eq!(rejected_route_count(&snapshotter), 1);
    }

    #[tokio::test]
    async fn test_quote_post_processor_pool_drops() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let recording = MockPostProcessor::with_request(move |request, result| {
            let rfq = direct_swap("rfq:bebop", 120);
            let plain = direct_swap("uniswap_v2", 120);
            recorder.lock().unwrap().extend([
                request.drops_component(rfq.protocol_component()),
                request.drops_component(plain.protocol_component()),
            ]);
            result
        });
        let mut worker =
            worker_with_post_processor(recording).with_exclude_protocols(vec!["rfq:".to_string()]);

        worker
            .quote(&a_to_b_order(), SolveParams::default(), far_deadline())
            .await
            .expect("the route is valid");

        assert_eq!(*seen.lock().unwrap(), [true, false]);
    }

    /// Adds the `rfq:bebop` component `rfq-leg` and its tokens to `market`.
    fn add_bebop_component(market: &MarketData) -> ProtocolComponent {
        let tokens = [token(0x01, "A"), token(0x02, "B")];
        let rfq = component_with_protocol("rfq-leg", "rfq:bebop", &tokens);
        let mut state = market.try_write().expect("uncontended");
        state.upsert_tokens(tokens);
        state.upsert_components([rfq.clone()]);
        rfq
    }

    fn has_graph_edge(worker: &SolverWorker<SplitRouteAlgorithm>, component_id: &str) -> bool {
        worker
            .graph_manager
            .graph()
            .edge_weights()
            .any(|edge| edge.component_id == component_id)
    }

    fn worker_on_market(
        market: MarketData,
        excluded: &[&str],
    ) -> SolverWorker<SplitRouteAlgorithm> {
        SolverWorker::new(
            market,
            DerivedData::new_shared(),
            SplitRouteAlgorithm { reported_spot_price: 2.0 },
            0,
            "test_pool".to_string(),
        )
        .with_algorithm_exclude_protocols(
            excluded
                .iter()
                .map(ToString::to_string)
                .collect(),
        )
    }

    /// A component in the market at startup is out of the algorithm's graph when
    /// `algorithm_exclude_protocols` names it, and in the graph without the key.
    #[rstest]
    #[case::excluded(&["rfq:bebop"], false)]
    #[case::not_excluded(&[], true)]
    #[tokio::test]
    async fn test_initialize_graph_algorithm_exclude_protocols(
        #[case] excluded: &[&str],
        #[case] in_graph: bool,
    ) {
        let (market, _) = setup_market_weighted(vec![]);
        add_bebop_component(&market);
        let post_processor = MockPostProcessor::unchanged();
        let initialize_calls = Arc::clone(&post_processor.initialize_calls);
        let mut worker = worker_on_market(market, excluded)
            .with_post_processors(vec![("rfq".to_string(), Box::new(post_processor))]);

        worker.initialize_graph().await;

        assert_eq!(has_graph_edge(&worker, "rfq-leg"), in_graph);
        assert_eq!(initialize_calls.load(Ordering::SeqCst), 1);
    }

    /// A component the algorithm leaves out is out of its graph, but the post-processors still
    /// receive its market event.
    #[rstest]
    #[case::excluded(&["rfq:bebop"], false)]
    #[case::not_excluded(&[], true)]
    #[tokio::test]
    async fn test_process_event_algorithm_exclude_protocols(
        #[case] excluded: &[&str],
        #[case] in_graph: bool,
    ) {
        let (market, _) = setup_market_weighted(vec![]);
        let post_processor = MockPostProcessor::unchanged();
        let added_ids = Arc::clone(&post_processor.added_component_ids);
        let mut worker = worker_on_market(market, excluded)
            .with_post_processors(vec![("rfq".to_string(), Box::new(post_processor))]);
        worker.initialize_graph().await;
        let rfq = add_bebop_component(&worker.market_data);
        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([("rfq-leg".to_string(), rfq.tokens.into())]),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        };

        worker.process_event(event).await;

        assert_eq!(has_graph_edge(&worker, "rfq-leg"), in_graph);
        assert_eq!(*added_ids.lock().unwrap(), ["rfq-leg".to_string()]);
    }

    /// A post-processor receives the market event of an exclusive component on a public-only pool
    /// too, though that pool keeps the component out of its graph.
    #[rstest]
    #[case::public_only(LiquidityScope::PublicOnly, false)]
    #[case::include_exclusive(LiquidityScope::IncludeExclusive, true)]
    #[tokio::test]
    async fn test_process_event_exclusive_component(
        #[case] scope: LiquidityScope,
        #[case] in_graph: bool,
    ) {
        let (market, _) = setup_market_weighted(vec![]);
        let post_processor = MockPostProcessor::unchanged();
        let added_ids = Arc::clone(&post_processor.added_component_ids);
        let mut worker = worker_on_market(market, &[])
            .with_liquidity_scope(scope)
            .with_post_processors(vec![("exclusive".to_string(), Box::new(post_processor))]);
        worker.initialize_graph().await;
        let tokens = [token(0x01, "A"), token(0x02, "B")];
        let mut exclusive = component_with_protocol("exclusive-leg", "exclusive:ekubo_v3", &tokens);
        crate::feed::exclusivity::mark_exclusive(&mut exclusive);
        {
            let mut state = worker
                .market_data
                .try_write()
                .expect("uncontended");
            state.upsert_tokens(tokens);
            state.upsert_components([exclusive.clone()]);
        }
        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(
                "exclusive-leg".to_string(),
                exclusive.tokens.into(),
            )]),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        };

        worker.process_event(event).await;

        assert_eq!(has_graph_edge(&worker, "exclusive-leg"), in_graph);
        assert_eq!(*added_ids.lock().unwrap(), ["exclusive-leg".to_string()]);
    }

    /// The post-processor may put a component into the final quote that the algorithm cannot use.
    #[tokio::test]
    async fn test_quote_post_processor_uses_algorithm_excluded_protocol() {
        let mut worker = worker_with_post_processor(MockPostProcessor::new(rfq_route))
            .with_algorithm_exclude_protocols(vec!["rfq:bebop".to_string()]);

        let quote = worker
            .quote(&a_to_b_order(), SolveParams::default(), far_deadline())
            .await
            .expect("the processed route is valid");

        let route = quote
            .order()
            .route()
            .expect("the quote has a route");
        assert_eq!(
            route.swaps()[0]
                .protocol_component()
                .protocol_system,
            "rfq:bebop"
        );
        assert_eq!(quote.order().post_processed_by(), ["rfq".to_string()]);
    }
}
