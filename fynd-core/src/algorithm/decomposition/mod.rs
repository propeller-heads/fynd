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

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use num_bigint::BigUint;
use num_traits::Zero;
use rustc_hash::FxHashSet;
use tracing::{debug, instrument, warn};
use tycho_simulation::tycho_core::models::{token::Token, Address};

use super::{most_liquid::DepthAndPrice, Algorithm, AlgorithmConfig};
use crate::{
    algorithm::decomposition::{
        assemble::build_route_result,
        components::{DecompositionError, SolutionGraph},
        graph_build::{build_routes_subgraph, SubgraphParams},
        optimizers::{
            equal_start_v2::EqualStartV2, frank_wolfe::FrankWolfe, pair_comparison::PairComparison,
            GasPrices, SplitOptimizerT,
        },
        reference::{build_reference_solution, ReferenceParams},
        solve::{
            choose_solution, net_of_gas, solve_solution_graph, solve_without_splits, SolutionChoice,
        },
        token_graph::{path_component_ids, AllowedTokens, DirectPath, SearchBounds, TokenGraph},
    },
    derived::{
        computation::ComputationRequirements,
        types::{ComponentDepths, TokenGasPrices},
        SharedDerivedDataRef,
    },
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

/// A split optimizer: how an amount is divided between parallel alternatives.
///
/// The first two are defibot's `solver.order_solver.decomposition.optimizer`; the third is not in
/// defibot. Each one's module says what it does and what it measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitOptimizer {
    /// Pairwise line search (`optimizers/pair_comparison.rs`).
    PairComparison,
    /// Equal-start gradient walk (`optimizers/equal_start_v2.rs`).
    EqualStartV2,
    /// Frank-Wolfe line search (`optimizers/frank_wolfe.rs`).
    FrankWolfe,
}

/// Which optimizer runs at which level of the solve.
///
/// A solve splits twice. The outer split hands the order to the branches; the inner splits hand a
/// branch's share to its sequences, and a hop's share to its pools. They do not have to use the
/// same optimizer, and on the recorded fixture they should not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOptimizerConfig {
    /// Splits the order across the graph's branches.
    pub outer: SplitOptimizer,
    /// Splits inside a branch: its sequences, and the pools of a hop.
    pub inner: SplitOptimizer,
}

impl Default for SplitOptimizerConfig {
    /// Pairwise over the branches, Frank-Wolfe inside them.
    fn default() -> Self {
        Self { outer: SplitOptimizer::PairComparison, inner: SplitOptimizer::FrankWolfe }
    }
}

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
    connector_token: Option<Address>,
    max_enumerated_paths: usize,
    optimizers: SplitOptimizerConfig,
    ranking_metric: RankingMetric,
}

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

    /// Sets the token the reference route's two-hop leg goes through, overriding the derived
    /// default.
    ///
    /// Leaving it unset does **not** mean "no reference route" — it means the token is derived per
    /// solve. The precedence is:
    ///
    /// 1. This override, when set.
    /// 2. The highest-degree token in [`AlgorithmConfig::connector_tokens`], when the operator set
    ///    an allowlist — the deepest hub they allowed.
    /// 3. The highest-degree token in the routing graph.
    ///
    /// Degree is the pool-edge count, the same connectivity signal `fynd derive-connector-tokens`
    /// ranks by, so the derived choice stays correct on a chain with no hardcoded list. defibot
    /// hard-codes the wrapped native token (`constants.WRAPPED_TOKEN`, `order_solver.py:344`);
    /// Fynd runs on chains where that is neither the same address nor the deepest hub, and the
    /// reference's whole job is to almost always exist.
    ///
    /// Set this when the operator knows better than the degree count — a chain where the deepest
    /// hub by pool count is not the deepest by liquidity, for instance.
    #[cfg(test)]
    pub fn with_connector_token(mut self, connector_token: Address) -> Self {
        self.connector_token = Some(connector_token);
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

    /// Sets the price [`SplitOptimizerT::EqualStartV2`] ranks alternatives by.
    ///
    /// defibot's `optimizer_config.iteration_strategy`; ignored by
    /// [`SplitOptimizerT::PairComparison`], which ranks on realised output alone.
    pub fn with_ranking_metric(mut self, ranking_metric: RankingMetric) -> Self {
        self.ranking_metric = ranking_metric;
        self
    }
}

impl Default for DecompositionConfig {
    fn default() -> Self {
        Self {
            max_parallel_routes: DEFAULT_MAX_PARALLEL_ROUTES,
            connector_token: None,
            max_enumerated_paths: DEFAULT_MAX_ENUMERATED_PATHS,
            optimizers: SplitOptimizerConfig::default(),
            ranking_metric: RankingMetric::default(),
        }
    }
}

/// Splits an order by decomposing it into sub-orders routed in parallel.
pub struct DecompositionAlgorithm {
    min_hops: usize,
    max_hops: usize,
    timeout: Duration,
    max_routes: Option<usize>,
    connector_tokens: Option<FxHashSet<Address>>,
    config: DecompositionConfig,
}

/// Everything one solve works against, prepared under a single market read lock.
struct SolveInput<'a> {
    /// Snapshot of the components every candidate path and reference leg can reach.
    market: MarketState,
    token_graph: TokenGraph<'a>,
    /// Candidate paths between the order's endpoints, reused by the candidate build.
    paths: Vec<DirectPath>,
    gas_price_wei: BigUint,
    depths: Option<ComponentDepths>,
    sell_token: Token,
    buy_token: Token,
    /// Token the reference route's two-hop leg goes through.
    ///
    /// Resolved against the base market state, not the snapshot: the snapshot holds a smaller
    /// graph and would pick a different token, whose pools nobody snapshotted.
    connector_token: Option<Token>,
    /// Instant the whole solve stops at.
    deadline: Instant,
    gas_prices: GasPrices,
}

impl<'a> SolveInput<'a> {
    fn new(
        market: MarketState,
        token_graph: TokenGraph<'a>,
        paths: Vec<DirectPath>,
        gas_price_wei: BigUint,
        token_prices: Option<Arc<TokenGasPrices>>,
        depths: Option<ComponentDepths>,
        sell_token: Token,
        buy_token: Token,
        connector_token: Option<Token>,
        deadline: Instant,
    ) -> Self {
        let gas_prices = GasPrices::new(gas_price_wei.clone(), token_prices);
        Self {
            market,
            token_graph,
            paths,
            gas_price_wei,
            depths,
            sell_token,
            buy_token,
            connector_token,
            deadline,
            gas_prices,
        }
    }
}

impl DecompositionAlgorithm {
    /// Creates a `DecompositionAlgorithm` from an `AlgorithmConfig`.
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Self::with_configs(config, DecompositionConfig::default())
    }

    /// Creates a `DecompositionAlgorithm` from both configuration halves.
    ///
    /// # Errors
    ///
    /// [`AlgorithmError::InvalidConfiguration`] when either cap is zero — a solve that may keep no
    /// route and enumerate no path has no answer to give.
    pub fn with_configs(
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
            min_hops: config.min_hops(),
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            max_routes: config.max_routes(),
            connector_tokens: config.connector_tokens().cloned(),
            config: decomposition,
        })
    }

    /// Minimum number of hops a candidate path may have.
    ///
    /// Read back for configuration round-tripping only; see the module docs for why the solve does
    /// not apply it.
    pub fn min_hops(&self) -> usize {
        self.min_hops
    }

    /// Maximum number of hops a candidate path may have.
    pub fn max_hops(&self) -> usize {
        self.max_hops
    }

    /// Cap on candidate paths considered per order, or `None` for no cap.
    pub fn max_routes(&self) -> Option<usize> {
        self.max_routes
    }

    /// Hard allowlist of tokens permitted as intermediate hops, or `None` when unrestricted. The
    /// order's own `token_in`/`token_out` are always allowed.
    pub fn connector_tokens(&self) -> Option<&FxHashSet<Address>> {
        self.connector_tokens.as_ref()
    }

    /// Decomposition-specific tuning parameters.
    pub fn config(&self) -> &DecompositionConfig {
        &self.config
    }

    /// Parallel alternatives kept per solution graph and per hop.
    fn effective_max_routes(&self) -> usize {
        self.max_routes
            .unwrap_or(self.config.max_parallel_routes)
    }

    /// Everything one solve works against: the derived data, the searched graph, and the market
    /// snapshot of the components its paths reach.
    async fn prepare_solve_input<'a>(
        &self,
        graph: &'a StableDiGraph<DepthAndPrice>,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
        deadline: Instant,
    ) -> Result<SolveInput<'a>, AlgorithmError> {
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

        let view = match label.as_ref() {
            Some(label) => market
                .read_labeled(label)
                .await
                .map_err(|error| AlgorithmError::Other(error.to_string()))?,
            None => market.read().await,
        };

        let sell_token = view
            .get_token(order.token_in())
            .cloned()
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token",
                id: Some(order.token_in().to_string()),
            })?;
        let buy_token = view
            .get_token(order.token_out())
            .cloned()
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token",
                id: Some(order.token_out().to_string()),
            })?;
        let gas_price_wei = view
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();

        let connector_token = self
            .get_connector_token(&token_graph, view.base_market_state(), order)
            .cloned();
        // A token the routing graph has never seen has no route of any kind, reference included,
        // so this is reported before anything is searched or snapshotted.
        if !token_graph.contains_token(&sell_token.address) {
            return Err(no_route(&sell_token, &buy_token, NoPathReason::SourceTokenNotInGraph));
        }

        if !token_graph.contains_token(&buy_token.address) {
            return Err(no_route(&sell_token, &buy_token, NoPathReason::DestinationTokenNotInGraph));
        }

        let bounds = SearchBounds {
            max_hops: self.max_hops,
            max_paths: self.config.max_enumerated_paths,
            deadline: Some(deadline),
        };

        let candidate_paths =
            token_graph.paths_between(&sell_token.address, &buy_token.address, &bounds);
        let mut components = path_component_ids(&candidate_paths);

        // The reference route is built from the snapshot, so its pools have to be in it. From two
        // hops up the candidate search has already found them — `sell -> connector -> buy` is one
        // of the paths it walks — so the legs are only searched when it cannot have, which is a
        // one-hop search or one that stopped early. The stop is exact rather than guessed: the
        // search halts on the path cap or on the deadline, and both still read true here.
        let truncated =
            candidate_paths.len() >= self.config.max_enumerated_paths || Instant::now() >= deadline;
        if self.max_hops < 2 || truncated {
            components.extend(self.reference_pools(
                &token_graph,
                &sell_token,
                &buy_token,
                connector_token.as_ref(),
                deadline,
            ));
        }
        let borrowed_components: FxHashSet<&ComponentId> = components.iter().collect();
        let market = view.extract_subset_with_overlay(&borrowed_components);
        drop(view);

        Ok(SolveInput::new(
            market,
            token_graph,
            candidate_paths,
            gas_price_wei,
            token_prices,
            depths,
            sell_token,
            buy_token,
            connector_token,
            deadline,
        ))
    }

    /// Token the reference route's two-hop leg goes through for this solve.
    fn get_connector_token<'a>(
        &self,
        graph: &TokenGraph<'_>,
        market: &'a MarketState,
        order: &Order,
    ) -> Option<&'a Token> {
        if let Some(address) = self.config.connector_token.as_ref() {
            // An explicit override is an operator decision, so it is honoured verbatim — including
            // when it names an endpoint — rather than silently replaced by a derived hub.
            return market.get_token(address);
        }

        let address = graph.highest_degree_token(|token| {
            token != order.token_in() &&
                token != order.token_out() &&
                self.connector_tokens
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(token))
        })?;
        market.get_token(address)
    }

    /// Pools the reference route's three legs can use: `sell -> connector`, `connector -> buy`,
    /// and the direct pair.
    ///
    /// Only the ids are wanted. The reference builds its own subgraph later, from the snapshot, so
    /// all this does is make sure the snapshot holds the pools.
    ///
    /// With no connector token the reference is direct pools alone, and those are the only leg
    /// searched.
    fn reference_pools(
        &self,
        graph: &TokenGraph<'_>,
        sell_token: &Token,
        buy_token: &Token,
        connector: Option<&Token>,
        deadline: Instant,
    ) -> FxHashSet<ComponentId> {
        let bounds = SearchBounds {
            max_hops: 1,
            max_paths: self.config.max_enumerated_paths,
            deadline: Some(deadline),
        };
        let mut pools = path_component_ids(&graph.paths_between(
            &sell_token.address,
            &buy_token.address,
            &bounds,
        ));
        let Some(connector) = connector else {
            return pools;
        };
        pools.extend(path_component_ids(&graph.paths_between(
            &sell_token.address,
            &connector.address,
            &bounds,
        )));
        pools.extend(path_component_ids(&graph.paths_between(
            &connector.address,
            &buy_token.address,
            &bounds,
        )));
        pools
    }

    /// Builds and solves the reference route, or `None` when there is no safe route to build.
    ///
    /// A failure to build the reference is logged and swallowed: the reference is the fallback, and
    /// letting it fail the order would make the fallback the thing that breaks the solve.
    fn solve_reference_solution(&self, input: &SolveInput, order: &Order) -> Option<SolutionGraph> {
        input.connector_token.as_ref()?;

        // The reference is one or two branches, so only the outer choice is meaningful here.
        // Matched rather than boxed: `SplitOptimizer::optimize` is generic over the alternative it
        // splits, so the trait is not object-safe.
        let metric = self.config.ranking_metric;
        let solution = match self.config.optimizers.outer {
            SplitOptimizer::PairComparison => self.reference_with(input, order, &PairComparison),
            SplitOptimizer::EqualStartV2 => {
                self.reference_with(input, order, &EqualStartV2::new(metric))
            }
            SplitOptimizer::FrankWolfe => self.reference_with(input, order, &FrankWolfe),
        };
        match solution {
            Ok(reference) => reference,
            Err(error) => {
                warn!(%error, "decomposition reference route failed; solving without one");
                None
            }
        }
    }

    /// [`build_reference_solution`] with one chosen optimizer.
    fn reference_with<O: SplitOptimizerT>(
        &self,
        input: &SolveInput,
        order: &Order,
        optimizer: &O,
    ) -> Result<Option<SolutionGraph>, AlgorithmError> {
        let Some(connector) = input.connector_token.as_ref() else {
            return Ok(None);
        };
        let params = ReferenceParams {
            sell_token: &input.sell_token,
            buy_token: &input.buy_token,
            intermediate_token: connector,
            max_routes: self.effective_max_routes(),
            deadline: Some(input.deadline),
        };
        build_reference_solution(
            &input.token_graph,
            &input.market,
            input.depths.as_ref(),
            &params,
            order.amount(),
            optimizer,
            &input.gas_prices,
        )
    }

    /// Solves `candidate` with the configured pair of optimizers.
    ///
    /// Two nested matches rather than one boxed optimizer, for the same reason as above.
    fn solve_candidate_graph(
        &self,
        candidate: &mut SolutionGraph,
        sell_amount: &BigUint,
        gas_prices: &GasPrices,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        let metric = self.config.ranking_metric;
        match self.config.optimizers.outer {
            SplitOptimizer::PairComparison => {
                self.solve_inner(candidate, sell_amount, &PairComparison, gas_prices)
            }
            SplitOptimizer::EqualStartV2 => {
                self.solve_inner(candidate, sell_amount, &EqualStartV2::new(metric), gas_prices)
            }
            SplitOptimizer::FrankWolfe => {
                self.solve_inner(candidate, sell_amount, &FrankWolfe, gas_prices)
            }
        }
    }

    /// The inner half of [`DecompositionAlgorithm::solve_candidate_graph`], with the outer
    /// optimizer already chosen.
    fn solve_inner<B: SplitOptimizerT>(
        &self,
        candidate: &mut SolutionGraph,
        sell_amount: &BigUint,
        outer: &B,
        gas_prices: &GasPrices,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        let metric = self.config.ranking_metric;
        match self.config.optimizers.inner {
            SplitOptimizer::PairComparison => {
                solve_solution_graph(candidate, sell_amount, outer, &PairComparison, gas_prices)
            }
            SplitOptimizer::EqualStartV2 => solve_solution_graph(
                candidate,
                sell_amount,
                outer,
                &EqualStartV2::new(metric),
                gas_prices,
            ),
            SplitOptimizer::FrankWolfe => {
                solve_solution_graph(candidate, sell_amount, outer, &FrankWolfe, gas_prices)
            }
        }
    }

    /// Builds and solves the full candidate subgraph.
    ///
    /// `minimum_price` is the reference's post-trade marginal price, which candidate paths must
    /// clear (`order_solver.py:240-242`).
    fn candidate_solution(
        &self,
        input: &SolveInput,
        order: &Order,
        minimum_price: f64,
    ) -> Result<SolutionGraph, AlgorithmError> {
        let params = SubgraphParams { max_routes: self.effective_max_routes(), minimum_price };
        let Some(mut candidate) =
            build_routes_subgraph(&input.market, input.depths.as_ref(), &params, &input.paths)?
        else {
            return Err(no_route(&input.sell_token, &input.buy_token, NoPathReason::NoGraphPath));
        };
        // Sell limits are denominated through the derived mid-prices from here on; see
        // `types::convert_through_numeraire`. Without them every cast falls back to chained spot
        // prices.
        if let Some(prices) = input.gas_prices.token_prices() {
            candidate.set_prices(Arc::clone(prices));
        }

        self.solve_candidate_graph(&mut candidate, order.amount(), &input.gas_prices)
            .map_err(|error| {
                AlgorithmError::Other(format!("decomposition solve failed: {error}"))
            })?;

        // `order_solver.py:137-141`: when the ordinary solve buys nothing the market is too thin to
        // split, so hand the order out greedily over whatever the branches can individually absorb.
        if candidate.buy_amount().is_zero() {
            debug!(
                "decomposition candidate bought nothing; falling back to solving without splits"
            );
            solve_without_splits(&mut candidate, order.amount(), &input.gas_prices).map_err(
                |error| {
                    AlgorithmError::Other(format!("decomposition fallback solve failed: {error}"))
                },
            )?;
        }

        Ok(candidate)
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
        let start = Instant::now();
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }
        let deadline = start + self.timeout;
        let input = self
            .prepare_solve_input(graph, market, label, derived, order, deadline)
            .await?;

        let reference_solution = self.solve_reference_solution(&input, order);
        let reference_price = match reference_solution.as_ref() {
            Some(r) => r
                .new_marginal_price()
                .unwrap_or_default(),
            None => 0.0,
        };

        // The reference is small and fast; the candidate subgraph is neither. Skipping the
        // candidate once the clock is out is what makes a timeout return the reference — a
        // complete, validated route — instead of a partial one.
        let candidate = if Instant::now() >= deadline {
            debug!(
                "decomposition out of time after the reference; skipping the candidate subgraph"
            );
            None
        } else {
            match self.candidate_solution(&input, order, reference_price) {
                Ok(candidate) => Some(candidate),
                // `order_solver.py:245-248`: an unbuildable subgraph is not an error while a
                // reference exists.
                Err(error) if reference_solution.is_some() => {
                    debug!(%error, "decomposition candidate subgraph unavailable; using the reference");
                    None
                }
                Err(error) => return Err(error),
            }
        };

        let ranked = rank_solutions(candidate, reference_solution, &input.gas_prices);
        if ranked.is_empty() {
            return Err(AlgorithmError::InsufficientLiquidity);
        }

        // An invalid route drops the whole worker pool's solution, so a winner that will not
        // assemble falls through to the runner-up rather than failing the order.
        let mut last_error = None;
        for (choice, solution) in &ranked {
            match build_route_result(
                solution,
                &input.market,
                order,
                &input.gas_prices,
                &input.gas_price_wei,
            ) {
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
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or(AlgorithmError::InsufficientLiquidity))
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

/// The "no route" error for an order's endpoints.
fn no_route(sell_token: &Token, buy_token: &Token, reason: NoPathReason) -> AlgorithmError {
    AlgorithmError::NoPath {
        from: sell_token.address.clone(),
        to: buy_token.address.clone(),
        reason,
    }
}

fn rank_solutions(
    candidate: Option<SolutionGraph>,
    reference: Option<SolutionGraph>,
    gas_prices: &GasPrices,
) -> Vec<(SolutionChoice, SolutionGraph)> {
    match (candidate, reference) {
        (Some(candidate), Some(reference)) => {
            debug!(
                candidate_net = %net_of_gas(&candidate, gas_prices),
                candidate_branches = candidate.branches().len(),
                reference_net = %net_of_gas(&reference, gas_prices),
                reference_branches = reference.branches().len(),
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

#[cfg(test)]
#[path = "tests/algorithm_tests.rs"]
mod tests;
