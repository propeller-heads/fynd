//! Decomposition split-routing algorithm (`decomposition`).
//!
//! Port of the decomposition algorithm from defibot's order solver
//! (`defibot/solver/order_solver/decomposition`). `README.md` in this directory records the
//! structural change from defibot, the bugs found in the Python while porting, and every deliberate
//! divergence; read it before changing anything here.
//!
//! # What a solve does
//!
//! [`DecompositionAlgorithm::find_best_route`] is the port of `solve_order` and `_solve`
//! (`order_solver.py:105-310`). In order:
//!
//! 1. Reject exact-out. No algorithm in this crate supports it.
//! 2. Enumerate candidate paths off the routing graph — which is passed separately from the market
//!    and needs no lock — and collect the components they touch.
//! 3. Take the market read lock once, honouring `label` so per-request pool overrides apply, read
//!    the block gas price, snapshot those components with
//!    [`MarketDataView::extract_subset_with_overlay`](crate::feed::market_data::MarketDataView::extract_subset_with_overlay),
//!    and drop the lock. Every simulation from here on runs against the snapshot.
//! 4. Build and solve the reference route (`reference`): direct pools plus one path through a
//!    connector token. Its post-trade marginal price is the floor candidate paths must clear, and
//!    it is the answer if nothing else can be built. The connector is the configured connector
//!    token when set, otherwise the highest-degree token of the operator's
//!    [`AlgorithmConfig::connector_tokens`] allowlist, otherwise the highest-degree token in the
//!    graph — so a pool spawned from `worker_pools.toml`, which can set neither, still gets a
//!    reference route.
//! 5. Build the candidate subgraph (`graph_build`) and solve it (`solve`): splits, loop removal, a
//!    re-solve of each branch for what it was actually allocated, then a sequential re-sell against
//!    the branches' shared liquidity. If that buys nothing, fall back to
//!    `solve::solve_without_splits`.
//! 6. Choose between candidate and reference on output net of gas (`solve::choose_solution`),
//!    assemble the winner (`assemble`), and fall through to the loser if the winner will not
//!    assemble or validate.
//!
//! # Amounts
//!
//! The [`RouteResult`] returned here is re-derived from the assembled
//! [`Route`](crate::types::Route) rather than carried over from the solver's own totals.
//! `assemble`'s module docs explain why and bound the difference; the short version is that the
//! solver floors every split independently while the encoder spends the whole balance, so only the
//! assembled route can describe the transaction that will actually be sent.
//!
//! # `min_hops` is not honoured
//!
//! [`AlgorithmConfig::min_hops`] has no effect here, and that is deliberate rather than an
//! oversight. defibot has no lower bound on path length, and a lower bound is not expressible in
//! this algorithm's shape: the reference route is *defined* as direct pools plus one two-hop path
//! (`order_solver.py:344-380`), so a `min_hops` of 2 would either silently keep building direct
//! reference pools — making the setting a lie — or delete the reference and with it the safety
//! floor the whole comparison rests on. An operator who wants to forbid direct pools should say so
//! with `connector_tokens`, which the enumeration does honour.
//!
//! # Constraints any implementation in this crate must satisfy
//!
//! * Every pool simulation goes through `sim_guard::GuardedProtocolSim::get_amount_out_guarded`. A
//!   raw `get_amount_out` panic from third-party pool math permanently kills the worker thread.
//! * All work runs on one solve clock (`start: Instant` vs [`Algorithm::timeout`]). Every loop
//!   checks it and returns the best complete result so far rather than failing.
//! * Take the market read lock once, snapshot what is needed, and drop it before simulating — the
//!   lock is shared with the feed.
//! * A multi-path route must be assembled through `split_primitives::build_split_route`. It emits
//!   swaps in topological order, merges hops shared between paths, and applies the tycho-execution
//!   remainder-split convention. A route assembled any other way is not encodable on-chain.
//! * Every returned route must pass `Route::validate()`. An invalid route drops the whole worker
//!   pool's solution, so fall through to the next-best candidate instead of returning it.

pub(crate) mod assemble;
pub(crate) mod components;
pub(crate) mod graph_build;
pub(crate) mod optimizers;
pub(crate) mod reference;
pub(crate) mod solve;
pub(crate) mod token_graph;

pub use optimizers::equal_start_v2::RankingMetric;

#[cfg(test)]
#[path = "tests/test_fixtures.rs"]
pub(crate) mod test_fixtures;

/// Route snapshots over the recorded market. Needs the fixture and `Solver::from_recording`, both
/// of which are `test-utils` only.
#[cfg(all(test, feature = "test-utils"))]
#[path = "tests/snapshot_tests.rs"]
mod snapshot_tests;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use models::{DirectPath, TokenPriceData};
use num_traits::Zero;
use optimizers::SplitOptimizerConfig;
use rustc_hash::FxHashSet;
use tracing::{debug, instrument, warn};
use tycho_simulation::tycho_core::models::Address;

use super::{most_liquid::DepthAndPrice, Algorithm, AlgorithmConfig};
use crate::{
    algorithm::decomposition::{
        assemble::cast_into_route,
        components::{DecompositionError, DecompositionGraph},
        graph_build::{build_decomposition_graph, SubgraphParams},
        reference::solve_reference_solution,
        solve::{
            choose_solution, net_of_gas, solve_solution_graph, solve_without_splits, SolutionChoice,
        },
        token_graph::{path_to_component_ids, AllowedTokens, SearchBounds, TokenGraph},
    },
    derived::{computation::ComputationRequirements, types::ComponentDepths, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, RouteResult},
    AlgorithmError, NoPathReason,
};

/// Default cap on parallel alternatives kept, per solution graph and per hop.
///
/// defibot's `solver.order_solver.decomposition.max_splits`: 30 in production
/// (`solver-config.yaml:195`), 50 in the base configuration
/// (`propeller-solver-core/core/defibot.yaml:629`). The base value is used here: on the recorded
/// fixture 30 cut 99 ranked branches down to 30 on the liquid pairs, and the branches are ranked on
/// unsolved estimates, so the cut is made on a rough ordering. Measured: 30 changed one row of 44
/// and made it worse (`USDC_to_WETH` at three hops, +192 → +64).
///
/// This is not a cap on token paths — those are bounded by
/// [`DecompositionConfig::max_enumerated_paths`] and the solve deadline. It caps what survives
/// *after* enumeration: the pools of one hop, the sequences of one group, and the branches of the
/// solution graph.
const DEFAULT_MAX_PARALLEL_ROUTES: usize = 50;

/// Default cap on enumerated candidate paths per solve.
///
/// Not a defibot quantity — defibot bounds discovery with a topology filter this port does not
/// have; see `graph_build`'s module docs. The value is set so the cap is never the binding
/// constraint on a two-hop search over a normal market, and only bites on a dense three-hop one,
/// where the deadline would otherwise be doing the work alone.
const DEFAULT_MAX_ENUMERATED_PATHS: usize = 20_000;

/// Tokens a candidate path may pass through, taken as the deepest hubs of the routing graph.
///
/// Not a defibot quantity: defibot names one intermediate token for the reference leg
/// (`order_solver.py:344`) and bounds the candidate search with a topology filter this port does
/// not have. The count is what bounds a multi-hop search here, since enumeration is exponential in
/// the branching factor at each intermediate.
const CONNECTOR_TOKEN_COUNT: usize = 20;

/// Tuning parameters specific to the decomposition loop.
///
/// Only knobs the solve actually reads. defibot's remaining decomposition settings are not
/// represented because nothing here consumes them: `optimizer_config.split_step` tunes a search
/// whose step schedule this port fixes (`optimizers/pair_comparison.rs`, `STEPS`);
/// `optimizer_config.iteration_strategy` is fixed at EqualStartV2's own default, which is what both
/// defibot configurations set it to; `max_depth` and `enable_topology_filter` are covered by
/// [`AlgorithmConfig::max_hops`] and by [`DecompositionConfig::with_max_enumerated_paths`]
/// respectively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompositionConfig {
    max_parallel_routes: usize,
    max_enumerated_paths: usize,
    optimizers: SplitOptimizerConfig,
}

type GraphSearchResult = (Vec<DirectPath>, Vec<DirectPath>, FxHashSet<ComponentId>);

impl DecompositionConfig {
    /// Sets the cap on parallel alternatives kept, applied both to the branches of a solution
    /// graph and to the pools of a single hop.
    ///
    /// This is defibot's `max_splits`, renamed. defibot uses that one name for two unrelated
    /// quantities — this route-count cap in the solver (`order_solver.py:497`, `:568`) and the
    /// EqualStartV2 optimizer's *iteration budget* (`optimizers/equal_start.py`) — and its own
    /// config comment describes it as neither ("the 60 splits with the highest inertia",
    /// `solver-config.yaml:193-194`). The name here says which one it is.
    ///
    /// [`AlgorithmConfig::max_routes`] overrides this when set, since an operator writing a cap in
    /// `worker_pools.toml` means it.
    pub fn with_max_parallel_routes(mut self, max_parallel_routes: usize) -> Self {
        self.max_parallel_routes = max_parallel_routes;
        self
    }

    /// Sets the hard cap on candidate paths the enumeration may find before it stops.
    ///
    /// The wall clock bounds the enumeration too; this bounds the *memory* and the cost of every
    /// stage after it, which the clock alone does not. See `graph_build`'s module docs for what
    /// truncation costs.
    pub fn with_max_enumerated_paths(mut self, max_enumerated_paths: usize) -> Self {
        self.max_enumerated_paths = max_enumerated_paths;
        self
    }

    /// Sets which optimizer runs at which level of the solve.
    pub fn with_optimizers(mut self, optimizers: SplitOptimizerConfig) -> Self {
        self.optimizers = optimizers;
        self
    }
}

impl Default for DecompositionConfig {
    fn default() -> Self {
        Self {
            max_parallel_routes: DEFAULT_MAX_PARALLEL_ROUTES,
            max_enumerated_paths: DEFAULT_MAX_ENUMERATED_PATHS,
            optimizers: SplitOptimizerConfig::default(),
        }
    }
}

/// Splits an order by decomposing it into sub-orders routed in parallel.
pub struct DecompositionAlgorithm {
    max_hops: usize,
    timeout: Duration,
    connector_tokens: Option<FxHashSet<Address>>,
    config: DecompositionConfig,
}

impl DecompositionAlgorithm {
    /// Creates a `DecompositionAlgorithm` from an `AlgorithmConfig`.
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Self::new(config, DecompositionConfig::default())
    }

    /// Creates a `DecompositionAlgorithm` from both configuration halves.
    ///
    /// # Errors
    ///
    /// [`AlgorithmError::InvalidConfiguration`] when either cap is zero — a solve that may keep no
    /// route and enumerate no path has no answer to give.
    pub fn new(
        config: AlgorithmConfig,
        decomposition: DecompositionConfig,
    ) -> Result<Self, AlgorithmError> {
        if decomposition.max_parallel_routes == 0 {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: "max_parallel_routes must be at least 1".to_string(),
            });
        }
        if decomposition.max_enumerated_paths == 0 {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: "max_enumerated_paths must be at least 1".to_string(),
            });
        }
        Ok(Self {
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            connector_tokens: config.connector_tokens().cloned(),
            config: decomposition,
        })
    }

    async fn solve_order(
        &self,
        order: &Order,
        graph: &StableDiGraph<DepthAndPrice>,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
    ) -> Result<RouteResult, DecompositionError> {
        let start = Instant::now();
        let deadline = start + self.timeout;

        let input = self
            .validate_input(graph, derived.as_ref(), order)
            .await?;

        // ---list all paths and clone pools from market data---
        let (all_paths, reference_paths, component_ids) = self.search_graph(deadline, &input);

        let components_to_snapshot: FxHashSet<&ComponentId> = component_ids.iter().collect();
        let market = match label.as_ref() {
            Some(l) => market
                .read_labeled(l)
                .await
                .map_err(|error| DecompositionError::MarketRead { reason: error.to_string() })?,
            None => market.read().await,
        };
        let market_state = market.extract_subset_with_overlay(&components_to_snapshot);
        drop(market);

        // GET TOKEN PRICES AND GAS PRICE
        let token_prices = match derived.as_ref() {
            Some(derived) => derived
                .read()
                .await
                .token_prices_shared(),
            None => None,
        };
        let gas_price = market_state
            .gas_price()
            .ok_or_else(|| DecompositionError::MarketRead {
                reason: "the market has no gas price".to_string(),
            })?
            .clone()
            .effective_gas_price();
        let price_data = TokenPriceData::new(gas_price, token_prices.clone());

        // SOLVE REFERENCE

        let reference_solution =
            self.solve_reference_solution(&input, reference_paths, &market_state, &price_data);

        // CZ: defibot uses price after swap here (new marginal price) but I think this call is
        // expensive. we should be able to use executed price although the cut will be less strict
        let reference_price = match reference_solution.as_ref() {
            Some(r) => r.executed_price(),
            None => 0.0,
        };

        // SOLVE MAIN

        // The reference is small and fast; the candidate subgraph is neither. Skipping the
        // candidate once the clock is out is what makes a timeout return the reference — a
        // complete, validated route — instead of a partial one.
        let main_solution = if Instant::now() >= deadline {
            debug!(
                "decomposition out of time after the reference; skipping the candidate subgraph"
            );
            None
        } else {
            match self.solve_full_graph(
                &input,
                all_paths,
                &market_state,
                &price_data,
                reference_price,
            ) {
                Ok(solution) => Some(solution),
                // `order_solver.py:245-248`: an unbuildable subgraph is not an error while a
                // reference exists.
                Err(error) if reference_solution.is_some() => {
                    debug!(%error, "decomposition candidate subgraph unavailable; using the reference");
                    None
                }
                Err(error) => return Err(error),
            }
        };

        // COMPARE BOTH
        // CZ: Lets just refactor this to PICK one, instead of giving a vec... so dumb

        let ranked = rank_solutions(main_solution, reference_solution, &price_data);
        if ranked.is_empty() {
            return Err(DecompositionError::SolveError);
        }

        // BUILD ROUTE ON WINNER

        // An invalid route drops the whole worker pool's solution, so a winner that will not
        // assemble falls through to the runner-up rather than failing the order.
        let mut last_error = None;
        for (choice, solution) in &ranked {
            match cast_into_route(solution, &market_state, &input.order, &price_data) {
                Ok(result) => {
                    debug!(
                        ?choice,
                        elapsed_ms = start.elapsed().as_millis(),
                        "decomposition solved"
                    );
                    return Ok(result);
                }
                Err(error) => {
                    warn!(?choice, %error, "decomposition solution did not assemble; trying the next");
                    last_error = Some(DecompositionError::RouteBuildFailure { error });
                }
            }
        }
        // `ranked` is non-empty by the check above, so the loop always sets this; the fallback is
        // for the compiler, not for a reachable state.
        Err(last_error.unwrap_or(DecompositionError::SolveError))
    }

    fn search_graph(&self, deadline: Instant, input: &SolveRequest) -> GraphSearchResult {
        let connector_tokens: Vec<Address> = self
            .get_connector_tokens(input)
            .into_iter()
            .cloned()
            .collect();

        // -- CANDIDATE PATH LISTING --
        let full_search_bounds = SearchBounds {
            max_hops: self.max_hops,
            max_paths: self.config.max_enumerated_paths,
            deadline: Some(deadline),
            connector_tokens: None,
        };
        let all_paths = input.search_graph(&full_search_bounds);
        let mut component_ids = path_to_component_ids(&all_paths);

        // -- REFERENCE PATH LISTING --
        // Reference solution uses only direct 1hop pools or go through connector tokens
        let direct_search = SearchBounds {
            max_hops: 1,
            max_paths: self.config.max_enumerated_paths,
            deadline: Some(deadline),
            connector_tokens: Some(Vec::new()),
        };
        let direct_paths = input.search_graph(&direct_search);
        let connector_search = SearchBounds {
            max_hops: 2,
            max_paths: self.config.max_enumerated_paths,
            deadline: Some(deadline),
            connector_tokens: Some(connector_tokens),
        };
        let connector_paths = input.search_graph(&connector_search);
        let reference_paths = direct_paths
            .into_iter()
            .chain(connector_paths.into_iter())
            .collect::<Vec<_>>();

        component_ids.extend(path_to_component_ids(&reference_paths));
        (all_paths, reference_paths, component_ids)
    }

    /// Everything one solve works against: the derived data, the searched graph, and the market
    /// snapshot of the components its paths reach.
    async fn validate_input<'a>(
        &self,
        graph: &'a StableDiGraph<DepthAndPrice>,
        derived: Option<&SharedDerivedDataRef>,
        order: &'a Order,
    ) -> Result<SolveRequest<'a>, DecompositionError> {
        let (token_prices, depths) = match derived {
            Some(derived) => {
                let store = derived.read().await;
                (
                    store
                        .token_prices()
                        .cloned()
                        .map(Arc::new),
                    store.component_depths().cloned(),
                )
            }
            None => (None, None),
        };

        let token_graph = TokenGraph::new(
            graph,
            &AllowedTokens {
                connector_tokens: self.connector_tokens.as_ref(),
                prices: token_prices.as_deref(),
                endpoints: [order.token_in(), order.token_out()],
            },
        );

        // A token the routing graph has never seen has no route of any kind, reference included,
        // so this is reported before anything is searched or snapshotted.
        if !token_graph.contains_token(order.token_in()) {
            return Err(DecompositionError::InvalidInput {
                reason: format!("sell token {} is not in the routing graph", order.token_in()),
            });
        }

        if !token_graph.contains_token(order.token_out()) {
            return Err(DecompositionError::InvalidInput {
                reason: format!("buy token {} is not in the routing graph", order.token_out()),
            });
        }

        Ok(SolveRequest::new(order, token_graph, depths, self.config.optimizers))
    }

    /// Tokens the reference route's two-hop leg may go through for this solve.
    ///
    /// The deepest hubs the operator's allowlist admits, by pool count. defibot hard-codes the
    /// wrapped native token (`order_solver.py:344`); Fynd runs on chains where that is neither the
    /// same address nor the deepest hub, and the reference's whole job is to almost always exist.
    fn get_connector_tokens<'a>(&self, input: &SolveRequest<'a>) -> Vec<&'a Address> {
        input
            .token_graph
            .highest_degree_tokens(CONNECTOR_TOKEN_COUNT, |token| {
                token != input.order.token_in() &&
                    token != input.order.token_out() &&
                    self.connector_tokens
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(token))
            })
    }

    /// Builds and solves the reference route, or `None` when there is no safe route to build.
    ///
    /// A failure to build the reference is logged and swallowed: the reference is the fallback, and
    /// letting it fail the order would make the fallback the thing that breaks the solve.
    fn solve_reference_solution(
        &self,
        input: &SolveRequest,
        reference_paths: Vec<DirectPath>,
        market_state: &MarketState,
        price_data: &TokenPriceData,
    ) -> Option<DecompositionGraph> {
        let solution = solve_reference_solution(
            input,
            reference_paths,
            self.config.max_parallel_routes,
            market_state,
            price_data,
        );
        match solution {
            Ok(reference) => reference,
            Err(error) => {
                warn!(%error, "decomposition reference route failed; solving without one");
                None
            }
        }
    }

    /// Builds and solves the full candidate subgraph.
    ///
    /// `minimum_price` is the reference's post-trade marginal price, which candidate paths must
    /// clear (`order_solver.py:240-242`).
    fn solve_full_graph(
        &self,
        input: &SolveRequest,
        paths: Vec<DirectPath>,
        market_state: &MarketState,
        gas_prices: &TokenPriceData,
        minimum_price: f64,
    ) -> Result<DecompositionGraph, DecompositionError> {
        let params = SubgraphParams { max_routes: self.config.max_parallel_routes, minimum_price };
        let mut candidate =
            build_decomposition_graph(market_state, input.depths.as_ref(), &params, paths)?;
        // Sell limits are denominated through the derived mid-prices from here on; see
        // `types::convert_through_numeraire`. Without them every cast falls back to chained spot
        // prices.
        if let Some(prices) = gas_prices.token_prices.as_ref() {
            candidate.set_prices(Arc::clone(prices));
        }

        solve_solution_graph(
            &mut candidate,
            input.order.amount(),
            input.split_optimizers,
            gas_prices,
        )?;

        // `order_solver.py:137-141`: when the ordinary solve buys nothing the market is too thin to
        // split, so hand the order out greedily over whatever the branches can individually absorb.
        if candidate.buy_amount().is_zero() {
            debug!(
                "decomposition candidate bought nothing; falling back to solving without splits"
            );
            solve_without_splits(&mut candidate, input.order.amount(), gas_prices)?;
        }

        Ok(candidate)
    }

    #[cfg(test)]
    fn connector_tokens(&self) -> Option<&FxHashSet<Address>> {
        self.connector_tokens.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn max_hops(&self) -> usize {
        self.max_hops
    }
    #[cfg(test)]
    pub(crate) fn config(&self) -> &DecompositionConfig {
        &self.config
    }
}

impl Algorithm for DecompositionAlgorithm {
    // Candidate ranking is `weight = inertia * (1 - fee) * price`, where `inertia` is the pool's
    // derived depth. That is the same pair of quantities `DepthAndPrice` carries, so the algorithm
    // shares the graph type — and therefore the graph manager and its update work — with
    // `bellman_ford` and `path_frank_wolfe` rather than adding another graph to maintain. It is one
    // edge per component, not the pair-keyed `TopologyGraph` that `most_liquid` and `water_fill`
    // moved to, because path enumeration here yields one path per *pool* combination and reads the
    // component off each edge directly. The weights on the edges are not read here: `weight` needs
    // the *directional* depth
    // of a specific hop, which the solve looks up in the derived store per hop, while an edge
    // weight is a single scalar per direction fixed at update time.
    type GraphType = StableDiGraph<DepthAndPrice>;
    type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        "decomposition"
    }

    #[instrument(level = "debug", skip_all, fields(order_id = %order.id()))]
    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }
        self.solve_order(order, graph, market, label, derived)
            .await
            .map_err(|error| match error {
                // Every other algorithm reports an unroutable pair as `NoPath`, and the router and
                // the RPC layer read that variant; flattening it to `Other` would make this
                // algorithm the only one that does not.
                DecompositionError::GraphBuildFailure | DecompositionError::SolveError => {
                    no_route(order, NoPathReason::NoGraphPath)
                }
                error => AlgorithmError::Other(format!("decomposition solve failed: {error}")),
            })
    }

    /// The solve reads two derived quantities and no others.
    ///
    /// * `pool_depths` is `inertia`, the depth term of `weight = inertia * (1 - fee) * price`
    ///   (`types::PoolRef::inertia`). Stale is allowed: a depth from a past block reorders
    ///   candidates by a little, and a pool with no entry at all already falls back to a constant
    ///   (`types::MISSING_DEPTH_INERTIA`), so a block-old value is strictly better than the handled
    ///   absence. `require_fresh` would gate the worker on the depth computation finishing every
    ///   block — paying availability for ranking precision that does not change which pools are
    ///   deep.
    /// * `token_prices` converts gas into buy-token units for the candidate-versus-reference
    ///   comparison and for the returned net amount. Stale is allowed for the same shape of reason:
    ///   with no price at all the solve ranks on gross output (`optimizers::GasPrices::new`), so a
    ///   stale price is again better than the handled absence, and gas is a small correction on any
    ///   order worth routing.
    ///
    /// Nothing is required fresh. The price half of `weight` comes from `spot_price` on the
    /// snapshotted simulation state, which is current by construction, so `spot_prices` is not a
    /// dependency of this algorithm even though the graph type carries it.
    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
            .allow_stale("pool_depths")
            .and_then(|requirements| requirements.allow_stale("token_prices"))
            .expect("decomposition requirements are stale-only and cannot conflict")
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Everything one solve works against, prepared under a single market read lock.
pub(crate) struct SolveRequest<'a> {
    /// Snapshot of the components every candidate path and reference leg can reach.
    order: &'a Order,
    token_graph: TokenGraph<'a>,
    depths: Option<ComponentDepths>,
    split_optimizers: SplitOptimizerConfig,
}

impl<'a> SolveRequest<'a> {
    fn new(
        order: &'a Order,
        token_graph: TokenGraph<'a>,
        depths: Option<ComponentDepths>,
        split_optimizers: SplitOptimizerConfig,
    ) -> Self {
        Self { order, token_graph, depths, split_optimizers }
    }

    fn search_graph(&self, search_bounds: &SearchBounds) -> Vec<DirectPath> {
        self.token_graph
            .paths_between(self.order.token_in(), self.order.token_out(), search_bounds)
    }
}

/// Reports an order as unroutable in the shape the other algorithms use.
fn no_route(order: &Order, reason: NoPathReason) -> AlgorithmError {
    AlgorithmError::NoPath { from: order.token_in().clone(), to: order.token_out().clone(), reason }
}

fn rank_solutions(
    candidate: Option<DecompositionGraph>,
    reference: Option<DecompositionGraph>,
    gas_prices: &TokenPriceData,
) -> Vec<(SolutionChoice, DecompositionGraph)> {
    match (candidate, reference) {
        (Some(candidate), Some(reference)) => {
            debug!(
                candidate_net = %net_of_gas(&candidate, gas_prices),
                candidate_sequences = candidate.sequences.len(),
                reference_net = %net_of_gas(&reference, gas_prices),
                reference_sequences = reference.sequences.len(),
                "decomposition ranking candidate against reference"
            );
            match choose_solution(&candidate, Some(&reference), gas_prices) {
                SolutionChoice::Candidate => vec![
                    (SolutionChoice::Candidate, candidate),
                    (SolutionChoice::Reference, reference),
                ],
                SolutionChoice::Reference => vec![
                    (SolutionChoice::Reference, reference),
                    (SolutionChoice::Candidate, candidate),
                ],
            }
        }
        (Some(candidate), None) => {
            debug!(candidate_net = %net_of_gas(&candidate, gas_prices), "decomposition has no reference to rank against");
            vec![(SolutionChoice::Candidate, candidate)]
        }
        (None, Some(reference)) => {
            debug!(reference_net = %net_of_gas(&reference, gas_prices), "decomposition has no candidate; returning the reference");
            vec![(SolutionChoice::Reference, reference)]
        }
        (None, None) => {
            debug!("decomposition produced neither a candidate nor a reference");
            Vec::new()
        }
    }
}

mod models;
#[cfg(test)]
#[path = "tests/algorithm_tests.rs"]
mod tests;
