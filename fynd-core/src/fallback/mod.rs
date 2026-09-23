//! Picks the pool each pAMM leg falls back to, and prices the route through those fallbacks.
//!
//! A pAMM quote only reaches the chain in the block the maker quotes for, so a pAMM swap that
//! lands late reverts. Tycho's `TychoFallbackRouter` catches that revert and runs the leg on a
//! fallback pool instead, so the solver names the fallback pool in the swap data.
//! `select_fallback` picks it: the candidate pool with the best simulated amount out for the leg's
//! `amount_in`, skipping any the request excludes. A pAMM leg with no selectable candidate pool
//! rejects the route.
//!
//! Every fallback runs with `minAmountOut = 0`; the route-level `min_amount_out` is the only price
//! check. It keeps describing the pAMM quote and the slippage the user accepted, so a fallback
//! below that floor reverts the route anyway — and lowering the floor to fit would pay the user
//! less than they accepted. `price_through_fallbacks` prices the route through the chosen
//! fallbacks so `WorkerPoolRouter` can drop such a route candidate before ranking.

pub(crate) mod manager;
pub(crate) mod user_data;

use num_bigint::BigUint;
use rustc_hash::FxHashMap;
use tracing::debug;
use tycho_execution::encoding::evm::swap_encoder::FallbackProtocol;
use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    algorithm::sim_guard::GuardedProtocolSim,
    feed::{component_filter::protocol_matches, events::MarketEvent, market_data::MarketDataView},
    replay::replay_route,
    types::{ComponentId, FallbackLeg, Route, RouteExclusionFilter, RouteRejection, Swap},
};

/// Marks a component the `TychoFallbackRouter` executes, e.g. `fallback:fermiswap`.
///
/// tycho-simulation's price level stream labels every pAMM's components with this prefix; the
/// direct `pricelevelstream:` label is only for a stream built `without_fallback_router`. Fynd
/// requests the venue by its `pricelevelstream:{venue}` entry and the stream decides the
/// label.
pub const FALLBACK_PREFIX: &str = tycho_execution::encoding::evm::FALLBACK_PREFIX;

/// Whether `route` has a leg the `TychoFallbackRouter` executes (`fallback:` protocol family).
///
/// Gate `price_through_fallbacks` behind this: it lets a route without such a leg cost neither a
/// market read lock nor a replay.
pub(crate) fn has_fallback_leg(route: &Route) -> bool {
    route.swaps().iter().any(|swap| {
        swap.protocol()
            .starts_with(FALLBACK_PREFIX)
    })
}

/// Whether `component` is a pAMM: a proprietary AMM that publishes a quote ladder per block, and
/// that the `TychoFallbackRouter` executes with a fallback pool of the solver's choosing.
pub(crate) fn is_pamm(component: &ProtocolComponent) -> bool {
    component
        .protocol_system
        .starts_with(FALLBACK_PREFIX)
}

/// Whether a graph must leave `component` out because nothing can serve as its fallback.
///
/// A `fallback:` leg only reaches the chain through its fallback, so a pAMM no pair of
/// whose tokens has a candidate pool in this market can never produce a quotable route. The pairs
/// are the ones `FallbackPoolIndex::insert` files a candidate under, so a multi-token pAMM stays
/// in the graph while any one of its pairs is backed. Returns `false` for any other protocol
/// system.
pub(crate) fn must_withhold_pamm(component: &ProtocolComponent, index: &FallbackPoolIndex) -> bool {
    if !is_pamm(component) {
        return false;
    }
    !token_pairs(&component.tokens).any(|(token_a, token_b)| {
        !index
            .candidates_for(token_a, token_b)
            .is_empty()
    })
}

/// Stamps a fallback pool on every pAMM leg of `route` and returns what the route delivers
/// through them.
///
/// Every other leg is untouched. Stamping and pricing happen in one pass, so a route cannot reach
/// the replay unstamped, and both read `market`, so a labeled solve prices on the overlay it was
/// solved on.
///
/// Check `has_fallback_leg` first: a route with no such leg pays for a replay that returns its own
/// amount out.
///
/// # Errors
///
/// The first pAMM leg that gets no fallback stops the pass; see `FallbackError`.
pub(crate) fn price_through_fallbacks(
    route: &mut Route,
    market: &MarketDataView<'_>,
    index: &FallbackPoolIndex,
    filter: &RouteExclusionFilter,
    pool_exclusions: &[String],
) -> Result<BigUint, FallbackError> {
    for swap in route.swaps_mut() {
        if !swap
            .protocol()
            .starts_with(FALLBACK_PREFIX)
        {
            continue;
        }
        let fallback = select_fallback(swap, market, index, filter, pool_exclusions)?;
        swap.set_fallback(fallback);
    }
    let substituted = substitute_fallbacks(route);
    let ids: Vec<ComponentId> = substituted
        .iter()
        .map(|swap| swap.component_id().to_string())
        .collect();
    let subset = market.extract_subset_with_overlay(&ids.iter().collect());

    // `replay_route` resolves every pool and token from the subset, so the substituted swaps'
    // embedded states are along for the ride and the route needs no token map of its own.
    let substituted = Route::new(substituted, FxHashMap::default())
        .map_err(|error| FallbackError::ReplayFailed { reason: error.to_string() })?;
    replay_route(&substituted, &subset)
        .map(|replay| replay.amount_out)
        .map_err(|error| FallbackError::ReplayFailed { reason: error.to_string() })
}

/// Every leg of `route` as it would execute on chain: a stamped leg moves onto its fallback pool,
/// every other leg is unchanged.
///
/// The substituted leg keeps the original's tokens, input amount, gas and split, so the replay
/// sees the same route shape; only the pool it runs against differs.
fn substitute_fallbacks(route: &Route) -> Vec<Swap> {
    route
        .swaps()
        .iter()
        .map(|swap| {
            let Some(fallback) = swap.fallback() else { return swap.clone() };
            Swap::new(
                fallback.component_id().to_string(),
                fallback
                    .protocol_component()
                    .protocol_system
                    .clone(),
                swap.token_in().clone(),
                swap.token_out().clone(),
                swap.amount_in().clone(),
                fallback.amount_out().clone(),
                swap.gas_estimate().clone(),
                fallback.protocol_component().clone(),
                fallback.protocol_state().clone_box(),
            )
            .with_split(*swap.split())
        })
        .collect()
}

/// Picks the fallback pool for one pAMM leg.
///
/// The candidate pool for the leg's pair with the best `get_amount_out` for the leg's `amount_in`,
/// among those the request does not exclude by pool id or protocol system. States are read from
/// `market`, so a labeled solve selects on the overlay it was solved on.
///
/// # Errors
///
/// `NoFallbackPool` when the market holds no candidate pool for the pair; `AllPoolsExcluded` when
/// the request rules out every one; `SimulationFailed` when none of the rest could be priced, which
/// includes a pool whose component carries too little to encode.
fn select_fallback(
    swap: &Swap,
    market: &MarketDataView<'_>,
    index: &FallbackPoolIndex,
    filter: &RouteExclusionFilter,
    pool_exclusions: &[String],
) -> Result<FallbackLeg, FallbackError> {
    let leg = swap.component_id();
    let candidates = index.candidates_for(swap.token_in(), swap.token_out());
    if candidates.is_empty() {
        return Err(FallbackError::NoFallbackPool {
            component_id: leg.to_string(),
            token_in: swap.token_in().clone(),
            token_out: swap.token_out().clone(),
        });
    }

    let admitted: Vec<&ComponentId> = candidates
        .iter()
        .filter(|candidate| !is_excluded(candidate, market, filter, pool_exclusions))
        .collect();
    if admitted.is_empty() {
        return Err(FallbackError::AllPoolsExcluded { component_id: leg.to_string() });
    }

    let (Some(token_in), Some(token_out)) =
        (market.get_token(swap.token_in()), market.get_token(swap.token_out()))
    else {
        debug!(pamm_leg = %leg, "the market holds no token metadata for the leg's pair");
        return Err(FallbackError::SimulationFailed { component_id: leg.to_string() });
    };

    let mut best: Option<FallbackLeg> = None;
    for candidate in admitted {
        let (Some(component), Some(state)) =
            (market.get_component(candidate), market.get_simulation_state(candidate))
        else {
            debug!(pamm_leg = %leg, %candidate, "fallback candidate left the market");
            continue;
        };
        // A pool we cannot encode is no use however well it prices.
        if let Err(error) =
            user_data::fallback_protocol(component, state, swap.token_in(), swap.token_out())
        {
            debug!(pamm_leg = %leg, %candidate, %error, "skipping fallback candidate");
            continue;
        }
        match state.get_amount_out_guarded(swap.amount_in().clone(), token_in, token_out) {
            Ok(simulated) => {
                if best
                    .as_ref()
                    .is_none_or(|leg| simulated.amount > *leg.amount_out())
                {
                    best = Some(FallbackLeg::new(
                        component.clone(),
                        state.clone_box(),
                        simulated.amount,
                    ));
                }
            }
            Err(error) => {
                debug!(pamm_leg = %leg, %candidate, %error, "fallback candidate would not simulate");
            }
        }
    }

    best.ok_or_else(|| FallbackError::SimulationFailed { component_id: leg.to_string() })
}

/// Whether this candidate pool is ruled out, by the request's own filter or by the worker pool's
/// `exclude_protocols`.
///
/// The fallback executes as part of the route, so a pool told never to route through a protocol
/// must not settle its pAMM legs there either.
///
/// A candidate the market no longer holds is not excluded here: `select_fallback` drops it when it
/// fails to read its state, which keeps "the request said no" and "the pool is gone" as separate
/// outcomes.
fn is_excluded(
    candidate: &ComponentId,
    market: &MarketDataView<'_>,
    filter: &RouteExclusionFilter,
    pool_exclusions: &[String],
) -> bool {
    if filter
        .excluded_pools()
        .contains(candidate)
    {
        return true;
    }
    let Some(component) = market.get_component(candidate) else { return false };
    filter
        .excluded_protocols()
        .iter()
        .chain(pool_exclusions)
        .any(|entry| protocol_matches(entry, &component.protocol_system))
}

/// Why a route could not be priced through its pAMM legs' fallbacks. Drop the route.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FallbackError {
    /// The market holds no candidate pool for the leg's pair, so the fallback would revert too.
    #[error("pAMM leg {component_id} has no fallback pool for the {token_in}/{token_out} pair")]
    NoFallbackPool {
        /// The pAMM leg with no fallback pool.
        component_id: ComponentId,
        /// The leg's input token, so a log line names the pair rather than only the component.
        token_in: Address,
        /// The leg's output token.
        token_out: Address,
    },
    /// The market holds candidate pools for the pair, but the request excludes every one: its
    /// filter reaches the pool the leg settles through, not only the pAMM.
    #[error("the request excludes every fallback pool for pAMM leg {component_id}")]
    AllPoolsExcluded {
        /// The pAMM leg whose candidate pools are all excluded.
        component_id: ComponentId,
    },
    /// Candidate pools remain, but none could be run for the leg. Each one's reason is logged at
    /// debug as it is skipped.
    #[error("no fallback pool for pAMM leg {component_id} could be used")]
    SimulationFailed {
        /// The pAMM leg whose candidate pools all failed.
        component_id: ComponentId,
    },
    /// Every leg has a fallback, but the substituted route could not be replayed, so there is no
    /// amount to floor at.
    #[error("the route could not be replayed through its fallbacks: {reason}")]
    ReplayFailed {
        /// What stopped the replay.
        reason: String,
    },
    /// A pool's component does not carry what `TychoFallbackRouter` needs to run it. Selection
    /// skips such a pool, so this only escapes when every candidate for a leg is unencodable.
    #[error("fallback pool {component_id} cannot be encoded: {reason}")]
    MissingPoolData {
        /// The candidate pool that cannot be encoded.
        component_id: ComponentId,
        /// What it is missing.
        reason: String,
    },
}

impl FallbackError {
    /// The rejection the worker reports for this failure, so a new variant cannot reach the worker
    /// without one.
    pub fn rejection(&self) -> RouteRejection {
        match self {
            Self::NoFallbackPool { .. } => RouteRejection::FallbackPoolMissing,
            Self::AllPoolsExcluded { .. } => RouteRejection::FallbackExcluded,
            Self::SimulationFailed { .. } |
            Self::ReplayFailed { .. } |
            Self::MissingPoolData { .. } => RouteRejection::FallbackNotSimulatable,
        }
    }
}

/// Candidate fallback pools by token pair, so finding them is a lookup, not a market scan.
///
/// A pool qualifies on its component alone — protocol system, tokens and, for Uniswap V4, its
/// static attributes — so the index only changes when the market adds or removes a component, not
/// when a pool's state changes. `apply_event` keeps it current from `MarketEvent::MarketUpdated`,
/// which costs one lookup per added component instead of a full rebuild.
#[derive(Debug, Default, Clone)]
pub struct FallbackPoolIndex {
    /// Candidates per sorted token pair. A component with more than two tokens is filed under
    /// every pair it can serve.
    pools: FxHashMap<(Address, Address), Vec<ComponentId>>,
    /// The pairs each indexed component is filed under, so a removal is a lookup rather than a
    /// scan of `pools`.
    keys: FxHashMap<ComponentId, Vec<(Address, Address)>>,
}

impl FallbackPoolIndex {
    /// Indexes every qualifying component in the market. Call once, then keep current with
    /// `apply_event`.
    pub fn build(market: &MarketDataView<'_>) -> Self {
        let mut index = Self::default();
        for component_id in market.component_topology().into_keys() {
            index.insert(market, component_id);
        }
        index
    }

    /// Adds the components in `event` and drops the removed ones.
    ///
    /// `updated_components` are state changes, which cannot alter whether a component qualifies,
    /// so they are ignored.
    pub fn apply_event(&mut self, market: &MarketDataView<'_>, event: &MarketEvent) {
        let MarketEvent::MarketUpdated { added_components, removed_components, .. } = event;
        for component_id in removed_components {
            self.remove(component_id);
        }
        for component_id in added_components.keys() {
            self.insert(market, component_id.clone());
        }
    }

    /// Every candidate pool the market holds for this pair, in no particular order — `build`
    /// walks the market's components as they are stored. Empty for a pair with no fallback pool.
    pub fn candidates_for(&self, token_a: &Address, token_b: &Address) -> &[ComponentId] {
        self.pools
            .get(&sorted_pair(token_a, token_b))
            .map_or(&[], Vec::as_slice)
    }

    /// Indexes `component_id` under every pair of its tokens the router could run, if
    /// `is_fallback_candidate` admits it. A pair with native ETH on either side is skipped.
    ///
    /// A component already indexed is re-filed from scratch, so an addition the market repeats
    /// cannot list it twice.
    fn insert(&mut self, market: &MarketDataView<'_>, component_id: ComponentId) {
        let Some(component) = market.get_component(&component_id) else { return };
        if !is_fallback_candidate(component) {
            return;
        }
        self.remove(&component_id);
        let token_count = component.tokens.len();
        let mut pairs = Vec::with_capacity(token_count * (token_count - 1) / 2);
        for (token_a, token_b) in token_pairs(&component.tokens) {
            // The rest of a pool's coins can still serve, but not a pair the router cannot run.
            if is_native_token(token_a) || is_native_token(token_b) {
                continue;
            }
            let pair = sorted_pair(token_a, token_b);
            self.pools
                .entry(pair.clone())
                .or_default()
                .push(component_id.clone());
            pairs.push(pair);
        }
        self.keys.insert(component_id, pairs);
    }

    /// Drops `component_id` from every pair it was filed under.
    fn remove(&mut self, component_id: &ComponentId) {
        let Some(pairs) = self.keys.remove(component_id) else { return };
        for pair in pairs {
            let Some(candidates) = self.pools.get_mut(&pair) else { continue };
            candidates.retain(|candidate| candidate != component_id);
            if candidates.is_empty() {
                self.pools.remove(&pair);
            }
        }
    }
}

/// Whether `component` can serve as a fallback pool.
///
/// Its protocol system must map to a fallback protocol that its chain's `TychoFallbackRouter`
/// runs, per tycho-execution's `FallbackProtocol`, and it must hold at least two tokens. A
/// Uniswap V4 pool must also have a zero or absent hook: hooked pools are not supported.
///
/// Native ETH is judged per pair rather than here: a pool holding it alongside ERC20 coins still
/// serves the pairs that do not touch it. See `FallbackPoolIndex::insert`.
fn is_fallback_candidate(component: &ProtocolComponent) -> bool {
    let Some(protocol) = FallbackProtocol::from_protocol_system(&component.protocol_system) else {
        return false;
    };
    if !protocol.supported_on(component.chain) || component.tokens.len() < 2 {
        return false;
    }
    if protocol != FallbackProtocol::UniswapV4 {
        return true;
    }
    !component
        .static_attributes
        .get("hooks")
        .is_some_and(|hooks| !is_zero(hooks.as_ref()))
}

/// Whether `token` is native ETH under either name a fallback protocol gives it.
///
/// Uniswap V4 uses the zero address; Curve and Fluid use the all-`0xee` sentinel.
/// `TychoFallbackRouter` moves a leg's tokens with ERC20 transfers and states that native ETH is
/// unsupported, so neither can be one side of a fallback swap.
fn is_native_token(token: &Address) -> bool {
    let bytes = token.as_ref();
    bytes.len() == 20 &&
        (bytes.iter().all(|byte| *byte == 0) || bytes.iter().all(|byte| *byte == 0xEE))
}

/// Every unordered pair of `tokens`, so a component is judged and indexed on the same pairs.
fn token_pairs(tokens: &[Address]) -> impl Iterator<Item = (&Address, &Address)> {
    tokens
        .iter()
        .enumerate()
        .flat_map(|(position, token_a)| {
            tokens[position + 1..]
                .iter()
                .map(move |token_b| (token_a, token_b))
        })
}

/// Whether every byte is zero: an absent hook, or the native-ETH currency.
fn is_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}

/// Order-independent pair key.
fn sorted_pair(token_a: &Address, token_b: &Address) -> (Address, Address) {
    if token_a <= token_b {
        (token_a.clone(), token_b.clone())
    } else {
        (token_b.clone(), token_a.clone())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_simulation::{
        tycho_common::{models::Chain, Bytes},
        tycho_core::simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::algorithm::test_utils::{self as util, addr};

    /// A pAMM quoting 2x against fallback pools quoting 1x and 1.5x, so the result is visibly the
    /// fallback and the better fallback is visibly chosen.
    const PAMM_PRICE: f64 = 2.0;
    const WORSE_FALLBACK_PRICE: f64 = 1.0;
    const BETTER_FALLBACK_PRICE: f64 = 1.5;
    const WORSE_POOL: &str = "0x1111111111111111111111111111111111111111";
    const BETTER_POOL: &str = "0x2222222222222222222222222222222222222222";
    const PAMM_COMPONENT: &str = "0xpamm";
    const UNBACKED_PAMM: &str = "0xpamm-unbacked";
    const PAMM_PROTOCOL: &str = "fallback:fermiswap";

    /// Market holding two fallback pools for the (1, 2) pair: a `uniswap_v3` at
    /// `WORSE_FALLBACK_PRICE` and a `uniswap_v2` at `BETTER_FALLBACK_PRICE`.
    fn market_with_fallback_pools() -> crate::feed::market_data::MarketData {
        let (token_in, token_out) = (util::token(1, "WETH"), util::token(2, "USDC"));
        let worse = util::component_with_protocol(
            WORSE_POOL,
            "uniswap_v3",
            &[token_in.clone(), token_out.clone()],
        );
        let better = util::component_with_protocol(
            BETTER_POOL,
            "uniswap_v2",
            &[token_in.clone(), token_out.clone()],
        );

        let market = crate::feed::market_data::MarketData::new_shared();
        {
            let mut state = market.try_write().expect("uncontended");
            state.upsert_tokens([token_in, token_out]);
            state.upsert_components([worse, better]);
            state.update_states([
                (
                    WORSE_POOL.to_string(),
                    Box::new(util::MockProtocolSim::new(WORSE_FALLBACK_PRICE))
                        as Box<dyn ProtocolSim>,
                ),
                (
                    BETTER_POOL.to_string(),
                    Box::new(util::MockProtocolSim::new(BETTER_FALLBACK_PRICE))
                        as Box<dyn ProtocolSim>,
                ),
            ]);
        }
        market
    }

    /// The fixture market's (1, 2) candidates by id, sorted — the index promises no order.
    fn sorted_candidates(index: &FallbackPoolIndex) -> Vec<String> {
        let mut ids: Vec<String> = index
            .candidates_for(&addr(1), &addr(2))
            .to_vec();
        ids.sort();
        ids
    }

    /// A component id the index tests reuse; a real address, so selection could encode it.
    const POOL: &str = "0x3333333333333333333333333333333333333333";

    /// A market holding one component per entry, each `(id, protocol system, token ids)`, with no
    /// simulation state — enough for the index, which judges a component alone.
    fn market_with(components: &[(&str, &str, &[u8])]) -> crate::feed::market_data::MarketData {
        let market = crate::feed::market_data::MarketData::new_shared();
        let mut state = market.try_write().expect("uncontended");
        for (id, system, token_ids) in components {
            let tokens: Vec<_> = token_ids
                .iter()
                .map(|id| util::token(*id, "TOK"))
                .collect();
            state.upsert_tokens(tokens.clone());
            state.upsert_components([util::component_with_protocol(id, system, &tokens)]);
        }
        drop(state);
        market
    }

    fn pamm_swap() -> Swap {
        let (token_in, token_out) = (util::token(1, "WETH"), util::token(2, "USDC"));
        Swap::new(
            PAMM_COMPONENT.to_string(),
            PAMM_PROTOCOL.to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(1_000u32),
            BigUint::from(2_000u32),
            BigUint::from(100_000u32),
            util::component_with_protocol(PAMM_COMPONENT, PAMM_PROTOCOL, &[token_in, token_out]),
            Box::new(util::MockProtocolSim::new(PAMM_PRICE)),
        )
    }

    /// A second pAMM leg, on a pair no fallback pool in the fixture market serves.
    fn unbacked_pamm_swap() -> Swap {
        let (token_in, token_out) = (util::token(2, "USDC"), util::token(3, "DAI"));
        Swap::new(
            UNBACKED_PAMM.to_string(),
            PAMM_PROTOCOL.to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(1_000u32),
            BigUint::from(2_000u32),
            BigUint::from(100_000u32),
            util::component_with_protocol(UNBACKED_PAMM, PAMM_PROTOCOL, &[token_in, token_out]),
            Box::new(util::MockProtocolSim::new(PAMM_PRICE)),
        )
    }

    /// A leg on a fallback pool itself, so the route holds no pAMM.
    fn uniswap_swap() -> Swap {
        let (token_in, token_out) = (util::token(1, "WETH"), util::token(2, "USDC"));
        Swap::new(
            WORSE_POOL.to_string(),
            "uniswap_v3".to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(1_000u32),
            BigUint::from(1_000u32),
            BigUint::from(100_000u32),
            util::component_with_protocol(WORSE_POOL, "uniswap_v3", &[token_in, token_out]),
            Box::new(util::MockProtocolSim::new(WORSE_FALLBACK_PRICE)),
        )
    }

    /// Only a route with a `fallback:` leg pays for fallback selection.
    #[test]
    fn test_has_fallback_leg() {
        let non_pamm =
            Route::new(vec![uniswap_swap()], FxHashMap::default()).expect("non-empty route");
        assert!(!has_fallback_leg(&non_pamm));

        let pamm = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");
        assert!(has_fallback_leg(&pamm));
    }

    /// A system that maps to a protocol the chain's `TychoFallbackRouter` runs qualifies, forks
    /// included; a system with no protocol byte does not, nor one the chain's router lacks, nor
    /// anything on a chain with no router.
    #[test]
    fn test_is_fallback_candidate_protocol_systems() {
        let pair = [util::token(1, "WETH"), util::token(2, "USDC")];
        let supported = [
            (Chain::Ethereum, "uniswap_v2"),
            (Chain::Ethereum, "sushiswap_v2"),
            (Chain::Ethereum, "pancakeswap_v3"),
            (Chain::Ethereum, "uniswap_v4"),
            (Chain::Ethereum, "vm:curve"),
            (Chain::Ethereum, "fluid_v1"),
            (Chain::Base, "uniswap_v3"),
            (Chain::Base, "aerodrome_slipstreams"),
            (Chain::Base, "aerodrome_v1"),
        ];
        for (chain, system) in supported {
            let mut component = util::component_with_protocol("pool", system, &pair);
            component.chain = chain;
            assert!(is_fallback_candidate(&component), "{system} must qualify on {chain}");
        }

        let unsupported = util::component_with_protocol("pool", "vm:balancer_v2", &pair);
        assert!(!is_fallback_candidate(&unsupported));

        // Base's router deploys without Fluid; Plasma has no router.
        let mut fluid_on_base = util::component_with_protocol("pool", "fluid_v1", &pair);
        fluid_on_base.chain = Chain::Base;
        assert!(!is_fallback_candidate(&fluid_on_base));
        let mut on_plasma = util::component_with_protocol("pool", "uniswap_v3", &pair);
        on_plasma.chain = Chain::Plasma;
        assert!(!is_fallback_candidate(&on_plasma));
        let pamm = util::component_with_protocol("pool", PAMM_PROTOCOL, &pair);
        assert!(!is_fallback_candidate(&pamm));
        // A pool of one token serves no pair, so it would be filed under nothing.
        let single = util::component_with_protocol("pool", "uniswap_v2", &pair[..1]);
        assert!(!is_fallback_candidate(&single));
    }

    /// `TychoFallbackRouter` runs Uniswap V4 without hook data or value, so a hooked pool or one
    /// with a native-ETH currency is left out.
    #[test]
    fn test_is_fallback_candidate_hooked_or_native_v4() {
        let pair = [util::token(1, "WETH"), util::token(2, "USDC")];
        let plain = util::component_with_protocol("v4", "uniswap_v4", &pair);
        assert!(is_fallback_candidate(&plain));

        let mut zero_hooks = plain.clone();
        zero_hooks
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(vec![0u8; 20]));
        assert!(is_fallback_candidate(&zero_hooks));

        let mut hooked = plain.clone();
        hooked
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(vec![0x11u8; 20]));
        assert!(!is_fallback_candidate(&hooked));
    }

    /// Native ETH rules out the pair, not the pool: a V4 pool on the zero address and a Curve pool
    /// on the `0xee` sentinel are filed under their ERC20 pairs and nothing else.
    #[test]
    fn test_index_skips_native_pairs() {
        let native_v4 = Address::from(vec![0u8; 20]);
        let native_curve = Address::from(vec![0xEEu8; 20]);
        let market = crate::feed::market_data::MarketData::new_shared();
        {
            let mut state = market.try_write().expect("uncontended");
            let (usdc, weth) = (util::token(1, "USDC"), util::token(2, "WETH"));
            state.upsert_tokens([usdc.clone(), weth.clone()]);
            let mut v4 = util::component_with_protocol(
                WORSE_POOL,
                "uniswap_v4",
                &[usdc.clone(), weth.clone()],
            );
            v4.tokens[0] = native_v4.clone();
            let mut curve = util::component_with_protocol(POOL, "vm:curve", &[usdc, weth]);
            curve.tokens.push(native_curve.clone());
            state.upsert_components([v4, curve]);
        }
        let view = market
            .try_read_blocking()
            .expect("uncontended");

        let index = FallbackPoolIndex::build(&view);

        // The V4 pool's only pair was ETH/WETH, so it is filed under nothing.
        assert!(index
            .candidates_for(&native_v4, &addr(2))
            .is_empty());
        // The Curve pool keeps USDC/WETH and loses the two pairs that touch ETH.
        assert_eq!(index.candidates_for(&addr(1), &addr(2)), [POOL.to_string()]);
        assert!(index
            .candidates_for(&addr(1), &native_curve)
            .is_empty());
        assert!(index
            .candidates_for(&addr(2), &native_curve)
            .is_empty());
    }

    /// The pair key is order-independent, and a pair the market holds no pool for is empty.
    #[test]
    fn test_candidates_for_token_order() {
        let market = market_with(&[(POOL, "uniswap_v3", &[9, 3])]);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        assert_eq!(index.candidates_for(&addr(3), &addr(9)), [POOL.to_string()]);
        assert_eq!(index.candidates_for(&addr(9), &addr(3)), [POOL.to_string()]);
        assert!(index
            .candidates_for(&addr(3), &addr(4))
            .is_empty());
    }

    /// A state update cannot change whether a component qualifies, so the index ignores it.
    #[test]
    fn test_apply_event_ignores_state_updates() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let mut index = FallbackPoolIndex::build(&view);

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: Vec::new(),
                updated_components: vec![BETTER_POOL.to_string()],
            },
        );

        assert_eq!(sorted_candidates(&index), [WORSE_POOL, BETTER_POOL]);
    }

    /// An addition the market repeats re-files the component rather than listing it twice.
    #[test]
    fn test_apply_event_repeated_addition() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let mut index = FallbackPoolIndex::build(&view);
        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(BETTER_POOL.to_string(), Vec::new())]),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        };

        index.apply_event(&view, &event);
        index.apply_event(&view, &event);

        assert_eq!(sorted_candidates(&index), [WORSE_POOL, BETTER_POOL]);
    }

    /// A component with more than two tokens is a candidate for every pair it can serve, and
    /// leaves all of them at once.
    #[test]
    fn test_index_files_a_multi_token_component_under_every_pair() {
        let market = market_with(&[(POOL, "vm:curve", &[1, 2, 3])]);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let mut index = FallbackPoolIndex::build(&view);

        for (token_a, token_b) in [(1, 2), (1, 3), (2, 3)] {
            assert_eq!(
                index.candidates_for(&addr(token_a), &addr(token_b)),
                [POOL.to_string()],
                "({token_a}, {token_b}) must hold the pool"
            );
        }

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: vec![POOL.to_string()],
                updated_components: Vec::new(),
            },
        );

        for (token_a, token_b) in [(1, 2), (1, 3), (2, 3)] {
            assert!(
                index
                    .candidates_for(&addr(token_a), &addr(token_b))
                    .is_empty(),
                "({token_a}, {token_b}) must be empty"
            );
        }
    }

    /// The index tracks the component set, so an added pool becomes a candidate and a removed one
    /// stops being one without a rebuild.
    #[test]
    fn test_apply_event_added_and_removed_components() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let mut index = FallbackPoolIndex::build(&view);
        assert_eq!(
            index
                .candidates_for(&addr(1), &addr(2))
                .len(),
            2
        );

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: vec![BETTER_POOL.to_string()],
                updated_components: Vec::new(),
            },
        );
        assert_eq!(index.candidates_for(&addr(1), &addr(2)), [WORSE_POOL.to_string()]);

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(BETTER_POOL.to_string(), Vec::new())]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            },
        );
        assert_eq!(sorted_candidates(&index), [WORSE_POOL, BETTER_POOL]);
    }

    /// Two candidate pools at different prices: the leg falls back to the one paying more.
    #[test]
    fn test_select_fallback_best_amount_out() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        let fallback =
            select_fallback(&pamm_swap(), &view, &index, &RouteExclusionFilter::default(), &[])
                .expect("candidates exist");

        assert_eq!(fallback.component_id(), BETTER_POOL);
        // MockProtocolSim multiplies by spot_price for ascending token addresses: 1000 * 1.5.
        assert_eq!(*fallback.amount_out(), BigUint::from(1_500u32));
    }

    /// The better pool is excluded by pool id and by protocol system in turn, and the leg takes
    /// the one left either way.
    #[rstest]
    #[case::by_pool(RouteExclusionFilter::default().with_excluded_pools([BETTER_POOL.to_string()]))]
    #[case::by_protocol(
        RouteExclusionFilter::default().with_excluded_protocols(["uniswap_v2".to_string()])
    )]
    fn test_select_fallback_excluded_candidates(#[case] filter: RouteExclusionFilter) {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        let fallback =
            select_fallback(&pamm_swap(), &view, &index, &filter, &[]).expect("one left");

        assert_eq!(fallback.component_id(), WORSE_POOL);
    }

    /// The request's filter reaches the pool the leg settles through, so excluding every candidate
    /// pool leaves it no fallback at all.
    #[test]
    fn test_select_fallback_every_candidate_excluded() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let filter = RouteExclusionFilter::default()
            .with_excluded_protocols(["uniswap_v2".to_string(), "uniswap_v3".to_string()]);

        let error =
            select_fallback(&pamm_swap(), &view, &index, &filter, &[]).expect_err("none left");

        assert_eq!(
            error,
            FallbackError::AllPoolsExcluded { component_id: PAMM_COMPONENT.to_string() }
        );
    }

    /// A worker pool told never to route through a protocol must not settle its pAMM legs there
    /// either, so its own `exclude_protocols` rules candidates out alongside the request's filter.
    #[test]
    fn test_select_fallback_honours_the_worker_pools_exclusions() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let pool_exclusions = ["uniswap_v2".to_string()];

        let fallback = select_fallback(
            &pamm_swap(),
            &view,
            &index,
            &RouteExclusionFilter::default(),
            &pool_exclusions,
        )
        .expect("the worse pool is left");

        assert_eq!(fallback.component_id(), WORSE_POOL);
    }

    /// A labeled solve selects on its overlay, so a pool the overlay reprices is judged at the
    /// overlay's price rather than the base market's.
    #[tokio::test]
    async fn test_select_fallback_on_the_overlay() {
        let market = market_with_fallback_pools();
        let label = "overlay".to_string();
        market
            .register_labeled_state(
                label.clone(),
                FxHashMap::from_iter([(
                    BETTER_POOL.to_string(),
                    Box::new(util::MockProtocolSim::new(0.5)) as Box<dyn ProtocolSim>,
                )]),
                u64::MAX,
            )
            .await;
        let view = market
            .read_labeled(&label)
            .await
            .expect("label was just registered");
        let index = FallbackPoolIndex::build(&view);

        let fallback =
            select_fallback(&pamm_swap(), &view, &index, &RouteExclusionFilter::default(), &[])
                .expect("candidate pools exist");

        // The overlay halves the better pool, dropping it below the worse one's unchanged 1.0.
        assert_eq!(fallback.component_id(), WORSE_POOL);
        assert_eq!(*fallback.amount_out(), BigUint::from(1_000u32));
    }

    /// The pass stops at the first leg that gets no fallback, and the error names that leg rather
    /// than the one that succeeded before it.
    #[test]
    fn test_price_through_fallbacks_stops_at_the_first_unbacked_leg() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let mut route = Route::new(vec![pamm_swap(), unbacked_pamm_swap()], FxHashMap::default())
            .expect("non-empty route");

        let error = price_through_fallbacks(
            &mut route,
            &view,
            &index,
            &RouteExclusionFilter::default(),
            &[],
        )
        .expect_err("the second leg has no candidate pool");

        assert_eq!(
            error,
            FallbackError::NoFallbackPool {
                component_id: UNBACKED_PAMM.to_string(),
                token_in: addr(2),
                token_out: addr(3),
            }
        );
    }

    /// A pair with no candidate pool rejects the route, naming the leg and its pair.
    #[test]
    fn test_select_fallback_without_candidates() {
        let market = crate::feed::market_data::MarketData::new_shared();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        let error =
            select_fallback(&pamm_swap(), &view, &index, &RouteExclusionFilter::default(), &[])
                .expect_err("no candidates");

        assert_eq!(
            error,
            FallbackError::NoFallbackPool {
                component_id: PAMM_COMPONENT.to_string(),
                token_in: addr(1),
                token_out: addr(2),
            }
        );
    }

    /// Only the pAMM legs get a fallback; a leg on an ordinary pool is left as it is.
    #[test]
    fn test_price_through_fallbacks_non_pamm_legs() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let mut route = Route::new(vec![uniswap_swap(), pamm_swap()], FxHashMap::default())
            .expect("non-empty route");

        price_through_fallbacks(&mut route, &view, &index, &RouteExclusionFilter::default(), &[])
            .expect("candidate pools exist");

        let [plain, pamm] = route.swaps() else { panic!("two legs") };
        assert!(plain.fallback().is_none());
        assert_eq!(
            pamm.fallback()
                .map(FallbackLeg::component_id),
            Some(BETTER_POOL)
        );
    }

    /// A split route prices through `replay_route`, so both legs are counted against one input
    /// total and the second sees the pool state the first left behind.
    #[test]
    fn test_price_through_fallbacks_split_route() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        // 60% through the pAMM, the remainder through the same pAMM. Both legs fall back to the
        // better pool, which then sees the second leg deplete it.
        let mut route =
            Route::new(vec![pamm_swap().with_split(0.6), pamm_swap()], FxHashMap::default())
                .expect("non-empty route");

        let amount_out = price_through_fallbacks(
            &mut route,
            &view,
            &index,
            &RouteExclusionFilter::default(),
            &[],
        )
        .expect("candidate pools exist");

        // Both legs fall back to the better pool and split one 2000 input: 60% at its 1.5, then
        // the remaining 800 at the 2.5 the mock reports after the first swap. Pricing one leg
        // only, or re-reading the pool's pre-swap state, cannot reach this.
        assert_eq!(amount_out, BigUint::from(3_800u32));
    }

    /// Only `fallback:` components are judged — everything else routes on its own terms — and a
    /// pAMM the market cannot back is withheld.
    #[test]
    fn test_must_withhold_pamm() {
        let index = FallbackPoolIndex::default();
        let uniswap = util::component_with_protocol(
            "0xuni",
            "uniswap_v3",
            &[util::token(3, "DAI"), util::token(4, "WBTC")],
        );
        assert!(!must_withhold_pamm(&uniswap, &index));

        let pamm = util::component_with_protocol(
            PAMM_COMPONENT,
            PAMM_PROTOCOL,
            &[util::token(3, "DAI"), util::token(4, "WBTC")],
        );
        assert!(must_withhold_pamm(&pamm, &index));
    }

    /// A pAMM is judged on every pair of its tokens, so a wider one stays in the graph while one
    /// pair is backed and leaves when none is.
    #[test]
    fn test_must_withhold_pamm_multi_token_component() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        // Only the (1, 2) pair has candidate pools; (1, 3) and (2, 3) have none.
        let backed = util::component_with_protocol(
            PAMM_COMPONENT,
            PAMM_PROTOCOL,
            &[util::token(1, "WETH"), util::token(2, "USDC"), util::token(3, "DAI")],
        );
        assert!(!must_withhold_pamm(&backed, &index));

        let unbacked = util::component_with_protocol(
            PAMM_COMPONENT,
            PAMM_PROTOCOL,
            &[util::token(3, "DAI"), util::token(4, "WBTC"), util::token(5, "USDT")],
        );
        assert!(must_withhold_pamm(&unbacked, &index));
    }
}
