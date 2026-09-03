//! Picks the pool each pAMM leg falls back to, and prices the route through those fallbacks.
//!
//! A pAMM quote only reaches the chain in the block the maker quotes for, so a pAMM swap that
//! lands late reverts. Tycho's `TychoFallbackRouter` catches that revert and runs the leg on a
//! fallback pool instead — and it is the encoder, i.e. this solver, that names the fallback pool
//! in the swap data. `select_fallbacks` picks it: the candidate among `FALLBACK_VENUE_SYSTEMS`
//! with the best simulated amount out for the leg's `amount_in`. A pAMM leg with no candidate
//! rejects the route.
//!
//! Every fallback runs with `minAmountOut = 0`; the route-level `min_amount_out` is the only price
//! check. It keeps describing the pAMM quote and the slippage the user accepted, so a fallback
//! below that floor reverts the route anyway — and lowering the floor to fit would pay the user
//! less than they accepted. `fallback_amount_out` prices the route through the chosen fallbacks so
//! the router can drop such a candidate before ranking.

use num_bigint::BigUint;
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Address};

use crate::{
    feed::{events::MarketEvent, market_data::MarketDataView},
    types::{ComponentId, FallbackLeg, Route},
};

/// Must match `tycho-execution`'s `PROPAMM_FALLBACK_PREFIX`.
pub const PROPAMM_FALLBACK_PREFIX: &str = "propammfallback:";

/// Protocol systems a pAMM leg may fall back to.
///
/// Must match the venues `TychoFallbackRouter` supports: Uniswap V2, Uniswap V3, Uniswap V4, Curve
/// and Fluid V1. A component under any other protocol system — a Uniswap V2 fork included — has no
/// venue byte the router understands, so it is never a candidate.
pub const FALLBACK_VENUE_SYSTEMS: &[&str] =
    &["uniswap_v2", "uniswap_v3", "uniswap_v4", "vm:curve", "fluid_v1"];

/// Whether `route` has a leg the `TychoFallbackRouter` executes (`propammfallback:` protocol
/// family).
///
/// Gate `select_fallbacks` and `fallback_amount_out` behind this: it lets a route without a pAMM
/// leg cost neither a market read lock nor a replay.
pub fn has_pamm_leg(route: &Route) -> bool {
    route.swaps().iter().any(|swap| {
        swap.protocol()
            .starts_with(PROPAMM_FALLBACK_PREFIX)
    })
}

/// Whether `component` is a pAMM with no pool to fall back on.
///
/// A `propammfallback:` leg only reaches the chain through its fallback, so a component whose pair
/// has no candidate in this market can never produce a quotable route. Returns `false` for any
/// other protocol system, and for a component that does not name exactly two tokens: the fallback
/// resolves per leg from the swap's own pair, so a wider component is left to `select_fallbacks`.
pub fn lacks_fallback_pool(component: &ProtocolComponent, index: &FallbackPoolIndex) -> bool {
    // Return false unless the protocol system starts with `PROPAMM_FALLBACK_PREFIX` and the
    // component has exactly two tokens; then return whether `index.candidates_for` is empty.
    todo!("judge {} against {index:?}", component.id)
}

/// Picks a fallback pool for every pAMM leg of `route`.
///
/// One entry per swap, in route order: `None` for a leg that is not `propammfallback:`, else the
/// candidate for the leg's pair with the best `get_amount_out` for the leg's `amount_in`. States
/// are read from `market`, so a labeled solve selects on the overlay it was solved on.
///
/// # Errors
///
/// `NoFallbackPool` when a pAMM leg's pair has no candidate in `index`; `NotPriceable` when no
/// candidate could be simulated.
pub fn select_fallbacks(
    route: &Route,
    market: &MarketDataView<'_>,
    index: &FallbackPoolIndex,
) -> Result<Vec<Option<FallbackLeg>>, FallbackSelectionError> {
    let mut fallbacks = Vec::with_capacity(route.swaps().len());
    for swap in route.swaps() {
        if !swap
            .protocol()
            .starts_with(PROPAMM_FALLBACK_PREFIX)
        {
            fallbacks.push(None);
            continue;
        }
        let candidates = index.candidates_for(swap.token_in(), swap.token_out());
        // `NoFallbackPool` when `candidates` is empty. Otherwise read each candidate's component
        // and state from `market`, simulate `get_amount_out(swap.amount_in())`, and keep the
        // candidate paying the most; `NotPriceable` when every simulation fails. Push a
        // `FallbackLeg` built from the winner.
        todo!(
            "pick the best of {} candidates for pAMM leg {} on state {:?}",
            candidates.len(),
            swap.component_id(),
            market.state_label()
        )
    }
    Ok(fallbacks)
}

/// Why a pAMM leg got no fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackSelectionError {
    /// The market holds no candidate pool for the leg's pair, so the fallback would revert too.
    /// Drop the route.
    NoFallbackPool {
        /// The pAMM leg with no fallback pool.
        component_id: ComponentId,
        /// The leg's input token, so a log line names the pair rather than only the component.
        token_in: Address,
        /// The leg's output token.
        token_out: Address,
    },
    /// Candidates exist, but none could be simulated for the leg's `amount_in`. Drop the route.
    NotPriceable {
        /// The pAMM leg whose candidates all failed.
        component_id: ComponentId,
        /// What stopped the last simulation.
        reason: String,
    },
}

/// Replays `route` with every pAMM leg pointing at the fallback stamped on it.
///
/// Every `propammfallback:` swap must already carry a `FallbackLeg` (see `Swap::fallback`), which
/// `select_fallbacks` provides. Uses `replay_route`, so split fractions and shared-pool depletion
/// behave exactly as they do for any other route: a substituted leg's smaller output feeds the
/// next one, and two legs on one pool see it deplete. The replay runs against `market`'s
/// overlay-aware states, so a labeled solve prices the fallback on the same state the route was
/// solved on.
///
/// A route without a pAMM leg substitutes nothing, so the result is its plain replayed amount out.
/// Check `has_pamm_leg` first to skip that pointless replay.
pub fn fallback_amount_out(route: &Route, market: &MarketDataView<'_>) -> FallbackAmountOut {
    // Build a substituted swap list: a non-pAMM swap is cloned; a pAMM swap becomes a `Swap` on
    // its `FallbackLeg`'s component and state (same tokens, amounts, gas and split), or
    // `MissingFallback` when none is stamped. Then `market.extract_subset_with_overlay` over the
    // substituted component ids, `Route::new`, and `replay_route`; map its errors to
    // `NotPriceable`.
    todo!(
        "replay {} swaps through their fallbacks on state {:?}",
        route.swaps().len(),
        market.state_label()
    )
}

/// The amount out a route delivers when its pAMM legs fall back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackAmountOut {
    /// The fallback fill. Quote the route only when this amount clears `min_amount_out`.
    AmountOut(BigUint),
    /// A pAMM leg carries no `FallbackLeg`, so there is nothing to replay it through. Drop the
    /// route.
    MissingFallback {
        /// The pAMM leg without a fallback.
        component_id: ComponentId,
    },
    /// The substituted route could not be simulated, so there is no amount to floor at. Drop the
    /// route.
    NotPriceable {
        /// What stopped the simulation.
        reason: String,
    },
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

    /// Every candidate the market holds for this pair, in insertion order. Empty for a pair with
    /// no fallback pool.
    pub fn candidates_for(&self, token_a: &Address, token_b: &Address) -> &[ComponentId] {
        let pair = sorted_pair(token_a, token_b);
        // Look `pair` up in `pools`; an absent key is an empty slice.
        todo!("look up {pair:?} in {} indexed pairs", self.pools.len())
    }

    /// Indexes `component_id` under every pair of its tokens if `qualifies` admits it.
    fn insert(&mut self, market: &MarketDataView<'_>, component_id: ComponentId) {
        let Some(component) = market.get_component(&component_id) else { return };
        if !qualifies(component) {
            return;
        }
        // For each unordered pair of `component.tokens`, push `component_id` onto
        // `pools[sorted_pair]` and record the pair in `keys[component_id]`.
        todo!("file {component_id} under each of its {} tokens' pairs", component.tokens.len())
    }

    /// Drops `component_id` from every pair it was filed under.
    fn remove(&mut self, component_id: &ComponentId) {
        // Take `keys[component_id]`; for each pair, remove `component_id` from `pools[pair]` and
        // drop the pair's entry once it is empty.
        todo!("remove {component_id} from {} indexed components", self.keys.len())
    }
}

/// Whether `component` can serve as a fallback pool.
///
/// Its protocol system must be one of `FALLBACK_VENUE_SYSTEMS` and it must hold at least two
/// tokens. A `uniswap_v4` pool must also have a zero or absent `hooks` static attribute and no
/// native-ETH (zero address) currency: `TychoFallbackRouter` runs V4 without hook data and
/// without value.
fn qualifies(component: &ProtocolComponent) -> bool {
    // Check `FALLBACK_VENUE_SYSTEMS.contains(protocol_system)` and `tokens.len() >= 2`; for
    // `uniswap_v4`, reject a non-zero `static_attributes["hooks"]` and any token equal to
    // `Address::zero()`.
    todo!("judge whether {} qualifies", component.id)
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
    use tycho_simulation::{
        tycho_common::Bytes, tycho_core::simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{self as util, addr},
        types::Swap,
    };

    /// A pAMM quoting 2x against fallback pools quoting 1x and 1.5x, so the result is visibly the
    /// fallback and the better fallback is visibly chosen.
    const PAMM_PRICE: f64 = 2.0;
    const WORSE_FALLBACK_PRICE: f64 = 1.0;
    const BETTER_FALLBACK_PRICE: f64 = 1.5;
    const WORSE_POOL: &str = "0xworse";
    const BETTER_POOL: &str = "0xbetter";
    const PAMM_COMPONENT: &str = "0xpamm";
    const PAMM_PROTOCOL: &str = "propammfallback:fermiswap";

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

    /// Only a route with a `propammfallback:` leg pays for fallback selection.
    #[test]
    fn test_has_pamm_leg() {
        let non_pamm =
            Route::new(vec![uniswap_swap()], FxHashMap::default()).expect("non-empty route");
        assert!(!has_pamm_leg(&non_pamm));

        let pamm = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");
        assert!(has_pamm_leg(&pamm));
    }

    /// Every venue the `TychoFallbackRouter` supports qualifies; a V2 fork under another protocol
    /// system does not, however similar its pools.
    #[test]
    #[ignore = "scaffold: qualifies is not implemented"]
    fn test_qualifies_venue_systems() {
        let pair = [util::token(1, "WETH"), util::token(2, "USDC")];
        for system in FALLBACK_VENUE_SYSTEMS {
            let component = util::component_with_protocol("pool", system, &pair);
            assert!(qualifies(&component), "{system} must qualify");
        }

        let fork = util::component_with_protocol("pool", "sushiswap_v2", &pair);
        assert!(!qualifies(&fork));
        let pamm = util::component_with_protocol("pool", PAMM_PROTOCOL, &pair);
        assert!(!qualifies(&pamm));
    }

    /// The router runs Uniswap V4 without hook data or value, so a hooked pool or one with a
    /// native-ETH currency is left out.
    #[test]
    #[ignore = "scaffold: qualifies is not implemented"]
    fn test_qualifies_rejects_hooked_or_native_v4() {
        let pair = [util::token(1, "WETH"), util::token(2, "USDC")];
        let plain = util::component_with_protocol("v4", "uniswap_v4", &pair);
        assert!(qualifies(&plain));

        let mut zero_hooks = plain.clone();
        zero_hooks
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(vec![0u8; 20]));
        assert!(qualifies(&zero_hooks));

        let mut hooked = plain.clone();
        hooked
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(vec![0x11u8; 20]));
        assert!(!qualifies(&hooked));

        let mut native = plain;
        native.tokens[0] = Address::from([0u8; 20]);
        assert!(!qualifies(&native));
    }

    /// The pair key is order-independent, and a pair the market holds no pool for is empty.
    #[test]
    #[ignore = "scaffold: candidates_for is not implemented"]
    fn test_candidates_for_token_order() {
        let mut index = FallbackPoolIndex::default();
        let pair = sorted_pair(&addr(9), &addr(3));
        index
            .pools
            .insert(pair.clone(), vec!["pool".to_string()]);
        index
            .keys
            .insert("pool".to_string(), vec![pair]);

        assert_eq!(index.candidates_for(&addr(3), &addr(9)), ["pool".to_string()]);
        assert_eq!(index.candidates_for(&addr(9), &addr(3)), ["pool".to_string()]);
        assert!(index
            .candidates_for(&addr(3), &addr(4))
            .is_empty());
    }

    /// The index tracks the component set, so an added pool becomes a candidate and a removed one
    /// stops being one without a rebuild.
    #[test]
    #[ignore = "scaffold: FallbackPoolIndex::insert/remove are not implemented"]
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
        assert_eq!(
            index
                .candidates_for(&addr(1), &addr(2))
                .len(),
            2
        );
    }

    /// Two candidates at different prices: the leg falls back to the one paying more.
    #[test]
    #[ignore = "scaffold: select_fallbacks is not implemented"]
    fn test_select_fallbacks_picks_best_amount_out() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let route = Route::new(vec![uniswap_swap(), pamm_swap()], FxHashMap::default())
            .expect("non-empty route");

        let fallbacks = select_fallbacks(&route, &view, &index).expect("candidates exist");

        assert_eq!(fallbacks.len(), 2);
        assert!(fallbacks[0].is_none(), "a non-pAMM leg gets no fallback");
        let fallback = fallbacks[1]
            .as_ref()
            .expect("the pAMM leg gets a fallback");
        assert_eq!(fallback.component_id(), BETTER_POOL);
        // MockProtocolSim multiplies by spot_price for ascending token addresses: 1000 * 1.5.
        assert_eq!(*fallback.amount_out(), BigUint::from(1_500u32));
    }

    /// A pair with no candidate rejects the route, naming the leg and its pair.
    #[test]
    #[ignore = "scaffold: select_fallbacks is not implemented"]
    fn test_select_fallbacks_without_candidates() {
        let market = crate::feed::market_data::MarketData::new_shared();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let route = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");

        let error = select_fallbacks(&route, &view, &index).expect_err("no candidates");

        assert_eq!(
            error,
            FallbackSelectionError::NoFallbackPool {
                component_id: PAMM_COMPONENT.to_string(),
                token_in: addr(1),
                token_out: addr(2),
            }
        );
    }

    /// A split route prices through `replay_route`, so both legs of the split are counted and a
    /// shared fallback pool depletes across them.
    #[test]
    #[ignore = "scaffold: select_fallbacks and fallback_amount_out are not implemented"]
    fn test_fallback_amount_out_split_route() {
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
        let fallbacks = select_fallbacks(&route, &view, &index).expect("candidates exist");
        for (swap, fallback) in route
            .swaps_mut()
            .iter_mut()
            .zip(fallbacks)
        {
            swap.set_fallback(fallback.expect("every leg is a pAMM"));
        }

        let amount_out = fallback_amount_out(&route, &view);

        // The whole 1000 of input is routed either way at the better pool's 1.5, against the pAMM
        // legs' recorded 2x.
        assert_eq!(amount_out, FallbackAmountOut::AmountOut(BigUint::from(1_500u32)));
    }

    /// A pAMM leg the worker did not stamp cannot be replayed, so the route is not priced.
    #[test]
    #[ignore = "scaffold: fallback_amount_out is not implemented"]
    fn test_fallback_amount_out_without_stamped_fallback() {
        let market = market_with_fallback_pools();
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let route = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");

        assert_eq!(
            fallback_amount_out(&route, &view),
            FallbackAmountOut::MissingFallback { component_id: PAMM_COMPONENT.to_string() }
        );
    }

    /// Only `propammfallback:` components are judged. Everything else routes on its own terms.
    #[test]
    #[ignore = "scaffold: lacks_fallback_pool is not implemented"]
    fn test_lacks_fallback_pool_ignores_other_protocols() {
        let index = FallbackPoolIndex::default();
        let uniswap = util::component_with_protocol(
            "0xuni",
            "uniswap_v3",
            &[util::token(3, "DAI"), util::token(4, "WBTC")],
        );
        assert!(!lacks_fallback_pool(&uniswap, &index));

        let pamm = util::component_with_protocol(
            PAMM_COMPONENT,
            PAMM_PROTOCOL,
            &[util::token(3, "DAI"), util::token(4, "WBTC")],
        );
        assert!(lacks_fallback_pool(&pamm, &index));
    }
}
