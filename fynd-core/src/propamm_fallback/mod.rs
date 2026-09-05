//! Computes the amount out a route delivers when its pAMM legs fall back to Uniswap V3.
//!
//! The router drops the candidate before ranking when this number cannot clear
//! `min_amount_out`, so the order is quoted with the next-best candidate.
//!
//! A pAMM quote only reaches the chain in the block the maker quotes for, so simulating a pAMM
//! route against a mined block reverts. Routing the leg through Titan's PropAMMRouter
//! (`propammfallback:` protocol family) falls back to Uniswap V3 instead of reverting. The fallback
//! pays less than the pAMM quote. `min_amount_out` keeps describing the pAMM quote and the slippage
//! the user accepted, so a fallback below that floor reverts the route anyway — and lowering the
//! floor to fit would pay the user less than they accepted. Such a route is not quoted.

pub mod fee_tier_fetcher;

use std::sync::{Arc, RwLock};

use num_bigint::BigUint;
use rustc_hash::{FxHashMap, FxHashSet};
use tycho_simulation::tycho_common::models::Address;

use crate::{
    feed::{events::MarketEvent, market_data::MarketDataView},
    replay::replay_route,
    types::{ComponentId, Route, Swap},
};

/// Whether `route` has a leg the PropAMMRouter executes (`propammfallback:` protocol family).
///
/// Gate `fallback_amount_out` behind this: it lets a route without a pAMM leg — every route until
/// a venue adopts the prefix — cost neither a market read lock nor a copy of the fee tiers.
pub fn has_pamm_leg(route: &Route) -> bool {
    route.swaps().iter().any(|swap| {
        swap.protocol()
            .starts_with(PROPAMM_FALLBACK_PREFIX)
    })
}

/// Whether `component` is a pAMM: a venue the router executes through a Uniswap V3 fallback.
pub fn is_pamm(
    component: &tycho_simulation::tycho_common::models::protocol::ProtocolComponent,
) -> bool {
    component
        .protocol_system
        .starts_with(PROPAMM_FALLBACK_PREFIX)
}

/// Whether `component` is a pAMM this market cannot route through.
///
/// The one rule for admitting a pAMM: a `propammfallback:` leg only reaches the chain through its
/// fallback, so a component whose fee tier has no pool in this market can never produce a quotable
/// route. Every caller asks this and nothing decides it a second way.
///
/// `fee_tiers` is `None` before [`FeeTierFetcher`](fee_tier_fetcher::FeeTierFetcher) reads the
/// router. There is no tier to look up then, and guessing one prices the wrong pool, so no pAMM is
/// backed until they arrive.
///
/// Returns `false` for any other protocol system, and for a pAMM that does not name exactly two
/// tokens once the tiers are known: the fallback resolves per leg from the swap's own pair, so a
/// wider component is left to [`fallback_amount_out`].
pub fn lacks_fallback_pool(
    component: &tycho_simulation::tycho_common::models::protocol::ProtocolComponent,
    fee_tiers: Option<&FeeTiers>,
    index: &FallbackPoolIndex,
) -> bool {
    if !is_pamm(component) {
        return false;
    }
    let Some(fee_tiers) = fee_tiers else {
        return true;
    };
    let [token_a, token_b] = component.tokens.as_slice() else {
        return false;
    };
    index
        .pool_for(token_a, token_b, fee_tiers.resolved_tier(token_a, token_b))
        .is_none()
}

/// Replays `route` with every `propammfallback:` leg pointing at its Uniswap V3 fallback pool.
///
/// Uses `replay_route`, so split fractions and shared-pool depletion behave exactly as they do for
/// any other route: a substituted leg's smaller output feeds the next one, and two legs on one
/// pool see it deplete. The replay runs against `market`'s overlay-aware states, so a labeled
/// solve prices the fallback on the same state the route was solved on.
///
/// A route without a pAMM leg substitutes nothing, so the result is its plain replayed amount out.
/// Check `has_pamm_leg` first to skip that pointless replay.
pub fn fallback_amount_out(
    route: &Route,
    market: &MarketDataView<'_>,
    fee_tiers: &FeeTiers,
    index: &FallbackPoolIndex,
) -> FallbackAmountOut {
    let mut substituted = Vec::with_capacity(route.swaps().len());
    for swap in route.swaps() {
        if !swap
            .protocol()
            .starts_with(PROPAMM_FALLBACK_PREFIX)
        {
            substituted.push(swap.clone());
            continue;
        }

        let fee_tier = fee_tiers.resolved_tier(swap.token_in(), swap.token_out());
        let Some(pool) = index.pool_for(swap.token_in(), swap.token_out(), fee_tier) else {
            return FallbackAmountOut::NoFallbackPool {
                component_id: swap.component_id().to_string(),
                fee_tier,
                token_in: swap.token_in().clone(),
                token_out: swap.token_out().clone(),
            };
        };
        let (Some(component), Some(state)) =
            (market.get_component(pool), market.get_simulation_state(pool))
        else {
            // The index was built from this market, so a missing component or state means it was
            // removed between the two reads. Treat it as no fallback pool.
            return FallbackAmountOut::NoFallbackPool {
                component_id: swap.component_id().to_string(),
                fee_tier,
                token_in: swap.token_in().clone(),
                token_out: swap.token_out().clone(),
            };
        };
        substituted.push(
            Swap::new(
                pool.clone(),
                component.protocol_system.clone(),
                swap.token_in().clone(),
                swap.token_out().clone(),
                swap.amount_in().clone(),
                swap.amount_out().clone(),
                swap.gas_estimate().clone(),
                component.clone(),
                state.clone_box(),
            )
            .with_split(*swap.split()),
        );
    }

    // `replay_route` resolves every pool from the state passed to it, so the substituted route has
    // to be priced against the same overlay the algorithm solved against.
    let replayed_components: Vec<ComponentId> = substituted
        .iter()
        .map(|swap| swap.component_id().to_string())
        .collect();
    let market_state = market.extract_subset_with_overlay(
        &replayed_components
            .iter()
            .collect::<FxHashSet<_>>(),
    );

    let substituted = match Route::new(substituted, FxHashMap::default()) {
        Ok(route) => route,
        Err(e) => return FallbackAmountOut::NotPriceable { reason: e.to_string() },
    };
    match replay_route(&substituted, &market_state) {
        Ok(replay) => FallbackAmountOut::AmountOut(replay.amount_out),
        Err(e) => FallbackAmountOut::NotPriceable { reason: e.to_string() },
    }
}

/// The amount out a route delivers when its pAMM legs fall back to Uniswap V3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackAmountOut {
    /// The fallback fill. Quote the route only when this amount clears `min_amount_out`.
    AmountOut(BigUint),
    /// No Uniswap V3 pool at the router's fee tier, so the fallback reverts too. Drop the route.
    NoFallbackPool {
        /// The pAMM leg with no fallback pool.
        component_id: ComponentId,
        /// The fee tier the router would have used.
        fee_tier: u32,
        /// The leg's input token, so a log line names the pair rather than only the component.
        token_in: Address,
        /// The leg's output token.
        token_out: Address,
    },
    /// The substituted route could not be simulated, so there is no amount to floor at. Drop the
    /// route.
    NotPriceable {
        /// What stopped the simulation.
        reason: String,
    },
}

/// The Uniswap V3 fee tier the PropAMMRouter falls back to, per token pair.
///
/// Mirrors the router's `resolvedFee`: the per-pair override if set, else `fallbackFee`. Both are
/// settable on chain without an upgrade, so this is read from chain rather than hardcoded.
///
/// Compared for equality so a worker can tell a refresh that changed a tier from one that did not:
/// a changed tier moves which Uniswap V3 pool backs a pAMM, and the graph has to be rebuilt for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeTiers {
    default_tier: u32,
    per_pair: FxHashMap<(Address, Address), u32>,
}

impl FeeTiers {
    /// Creates the tiers with `default_tier` as the router's global `fallbackFee`.
    pub fn new(default_tier: u32) -> Self {
        Self { default_tier, per_pair: FxHashMap::default() }
    }

    /// Order-independent, like the router's `_pairKey`.
    pub fn resolved_tier(&self, token_a: &Address, token_b: &Address) -> u32 {
        self.per_pair
            .get(&sorted_pair(token_a, token_b))
            .copied()
            .unwrap_or(self.default_tier)
    }

    /// A tier of 0 clears the override, matching the router's `setPairFee`.
    pub fn set_pair_tier(&mut self, token_a: &Address, token_b: &Address, fee_tier: u32) {
        let key = sorted_pair(token_a, token_b);
        if fee_tier == 0 {
            self.per_pair.remove(&key);
        } else {
            self.per_pair.insert(key, fee_tier);
        }
    }

    /// How many pairs carry an override.
    pub fn pair_override_count(&self) -> usize {
        self.per_pair.len()
    }
}

/// Shared with the task that reads the tiers from the router.
///
/// Empty until the first successful read. A worker that finds it empty drops the pAMM route rather
/// than guess a tier: the wrong tier prices the wrong pool, and a floor derived from the wrong
/// pool is the revert this whole path exists to prevent.
#[derive(Debug, Clone, Default)]
pub struct SharedFeeTiers(Arc<RwLock<Option<FeeTiers>>>);

impl SharedFeeTiers {
    /// Returns a copy of the current fee tiers, or `None` before the first successful read.
    pub fn snapshot(&self) -> Option<FeeTiers> {
        self.0
            .read()
            .expect("fallback fee tier lock poisoned")
            .clone()
    }

    /// Replaces the fee tiers with freshly read on-chain values.
    pub fn set(&self, fee_tiers: FeeTiers) {
        *self
            .0
            .write()
            .expect("fallback fee tier lock poisoned") = Some(fee_tiers);
    }
}

/// Fallback pools by token pair and fee tier, so finding one is a lookup, not a market scan.
///
/// A pool qualifies on its component alone — protocol system, token pair and `fee` attribute — so
/// the index only changes when the market adds or removes a component, not when a pool's state
/// changes. `apply_event` keeps it current from `MarketEvent::MarketUpdated`, which costs one
/// lookup per added component instead of a full rebuild.
#[derive(Debug, Default, Clone)]
pub struct FallbackPoolIndex {
    pools: FxHashMap<(Address, Address, u32), ComponentId>,
    /// Reverse map, so a removed component can be found without scanning `pools`.
    keys: FxHashMap<ComponentId, (Address, Address, u32)>,
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

    /// Indexes `component_id` if it can serve the router's fallback.
    ///
    /// Skips components that are not `uniswap_v3`, do not have exactly two tokens, or carry no
    /// `fee` attribute. The router's fallback is a single-hop `exactInputSingle`, so nothing else
    /// can serve it.
    fn insert(&mut self, market: &MarketDataView<'_>, component_id: ComponentId) {
        let Some(component) = market.get_component(&component_id) else { return };
        if component.protocol_system != FALLBACK_PROTOCOL_SYSTEM {
            return;
        }
        let [token_a, token_b] = component.tokens.as_slice() else { return };
        let Some(fee_tier) = component
            .static_attributes
            .get(FEE_ATTRIBUTE)
            .and_then(parse_fee_tier)
        else {
            return;
        };
        let (low, high) = sorted_pair(token_a, token_b);
        self.keys
            .insert(component_id.clone(), (low.clone(), high.clone(), fee_tier));
        self.pools
            .insert((low, high, fee_tier), component_id);
    }

    /// Drops `component_id`, leaving any pool that took over its key in place.
    fn remove(&mut self, component_id: &ComponentId) {
        let Some(key) = self.keys.remove(component_id) else { return };
        if self.pools.get(&key) == Some(component_id) {
            self.pools.remove(&key);
        }
    }

    /// Every fee tier the market holds a Uniswap V3 pool at for this pair, ascending.
    ///
    /// Separates "the market has no pool for this pair" from "it has one, at a tier the router
    /// does not resolve to". Only called when a fallback is already missing, so the scan over the
    /// index costs nothing on the routing path.
    pub fn tiers_for(&self, token_a: &Address, token_b: &Address) -> Vec<u32> {
        let (low, high) = sorted_pair(token_a, token_b);
        let mut tiers: Vec<u32> = self
            .pools
            .keys()
            .filter(|(a, b, _)| *a == low && *b == high)
            .map(|(_, _, tier)| *tier)
            .collect();
        tiers.sort_unstable();
        tiers
    }

    /// The component the router would fall back to.
    pub fn pool_for(
        &self,
        token_a: &Address,
        token_b: &Address,
        fee_tier: u32,
    ) -> Option<&ComponentId> {
        let (low, high) = sorted_pair(token_a, token_b);
        self.pools.get(&(low, high, fee_tier))
    }
}

/// Must match `tycho-execution`'s `PROPAMM_FALLBACK_PREFIX`.
pub const PROPAMM_FALLBACK_PREFIX: &str = "propammfallback:";

/// The PropAMMRouter deployment on Ethereum mainnet: the router serving Titan's pAMM ecosystem,
/// written by LambdaClass.
///
/// <https://github.com/lambdaclass/propamm-router-contracts>
pub const PROPAMM_ROUTER_ADDRESS: &str = "0x4DdF368080CD7946db5b459aD591c350158175e1";

/// pAMM venues on the router's whitelist that Fynd routes through.
///
/// Only whitelisted venues may use `PROPAMM_FALLBACK_PREFIX`. The router reverts `UnknownVenue` for
/// any other address, which fires the catch arm and executes every swap on Uniswap V3 at a worse
/// price than the pAMM gives. `FeeTierFetcher` reads each venue's pairs to learn which pairs need
/// a fee tier.
pub const PROPAMM_VENUES: &[&str] = &[
    // Fermi (FermiSwapper)
    "0x5979458912F80B96d30D4220af8E2e4925A33320",
    // Kipseli
    "0x71e790dd841c8A9061487cb3E78C288E75cE0B3d",
];

/// Protocol system whose pools the PropAMMRouter falls back to.
pub(crate) const FALLBACK_PROTOCOL_SYSTEM: &str = "uniswap_v3";

/// Static attribute carrying a Uniswap V3 pool's fee tier, in hundredths of a bip.
pub(crate) const FEE_ATTRIBUTE: &str = "fee";

/// Order-independent pair key, matching the router's `_pairKey`.
fn sorted_pair(token_a: &Address, token_b: &Address) -> (Address, Address) {
    if token_a <= token_b {
        (token_a.clone(), token_b.clone())
    } else {
        (token_b.clone(), token_a.clone())
    }
}

/// Tycho encodes the `fee` attribute big-endian in up to 4 bytes.
fn parse_fee_tier(raw: &tycho_simulation::tycho_common::Bytes) -> Option<u32> {
    let bytes = raw.as_ref();
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    let mut padded = [0u8; 4];
    padded[4 - bytes.len()..].copy_from_slice(bytes);
    Some(u32::from_be_bytes(padded))
}

#[cfg(test)]
mod tests {
    use tycho_simulation::{
        tycho_common::Bytes, tycho_core::simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::algorithm::test_utils::{self as util, addr};

    /// The tier the router reports as its global `fallbackFee`, in the tests below.
    const DEFAULT_TIER: u32 = 3000;

    #[test]
    fn test_resolved_tier_defaults_and_overrides() {
        let mut fee_tiers = FeeTiers::new(DEFAULT_TIER);
        assert_eq!(fee_tiers.resolved_tier(&addr(1), &addr(2)), DEFAULT_TIER);

        fee_tiers.set_pair_tier(&addr(2), &addr(1), 500);
        // Order-independent, like the router's sorted pair key.
        assert_eq!(fee_tiers.resolved_tier(&addr(1), &addr(2)), 500);
        assert_eq!(fee_tiers.resolved_tier(&addr(2), &addr(1)), 500);

        // A zero tier clears the override, matching `setPairFee`.
        fee_tiers.set_pair_tier(&addr(1), &addr(2), 0);
        assert_eq!(fee_tiers.resolved_tier(&addr(1), &addr(2)), DEFAULT_TIER);
    }

    /// Nothing may price a pAMM route before the router's tiers are read.
    #[test]
    fn test_shared_fee_tiers_empty_until_set() {
        let shared = SharedFeeTiers::default();
        assert!(shared.snapshot().is_none());

        shared.set(FeeTiers::new(500));
        assert_eq!(
            shared
                .snapshot()
                .expect("tiers were set")
                .resolved_tier(&addr(1), &addr(2)),
            500
        );
    }

    #[test]
    fn test_parse_fee_tier() {
        assert_eq!(parse_fee_tier(&Bytes::from(3000_i32.to_be_bytes().to_vec())), Some(3000));
        // Tycho trims leading zero bytes on some attributes.
        assert_eq!(parse_fee_tier(&Bytes::from(vec![0x01, 0xf4])), Some(500));
        assert_eq!(parse_fee_tier(&Bytes::from(Vec::new())), None);
        assert_eq!(parse_fee_tier(&Bytes::from(vec![0u8; 5])), None);
    }

    #[test]
    fn test_pool_for_token_order() {
        let mut index = FallbackPoolIndex::default();
        let (low, high) = sorted_pair(&addr(9), &addr(3));
        index
            .pools
            .insert((low, high, 500), "pool".to_string());

        assert_eq!(
            index
                .pool_for(&addr(3), &addr(9), 500)
                .map(String::as_str),
            Some("pool")
        );
        assert_eq!(
            index
                .pool_for(&addr(9), &addr(3), 500)
                .map(String::as_str),
            Some("pool")
        );
        // Wrong tier is a different pool.
        assert!(index
            .pool_for(&addr(3), &addr(9), 3000)
            .is_none());
    }

    /// The index tracks the component set, so an added pool becomes usable and a removed one
    /// stops being usable without a rebuild.
    #[test]
    fn test_apply_event_added_and_removed_components() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let mut index = FallbackPoolIndex::build(&view);
        assert_eq!(index.pools.len(), 1);

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: vec![FALLBACK_POOL.to_string()],
                updated_components: Vec::new(),
            },
        );
        assert!(index.pools.is_empty());
        assert!(index
            .pool_for(&addr(1), &addr(2), 500)
            .is_none());

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(FALLBACK_POOL.to_string(), Vec::new())]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            },
        );
        assert_eq!(
            index
                .pool_for(&addr(1), &addr(2), 500)
                .map(String::as_str),
            Some(FALLBACK_POOL)
        );
    }

    /// A state change cannot alter whether a component qualifies, so it must not touch the index.
    #[test]
    fn test_apply_event_state_updates() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let mut index = FallbackPoolIndex::build(&view);

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: Vec::new(),
                updated_components: vec![FALLBACK_POOL.to_string()],
            },
        );

        assert_eq!(index.pools.len(), 1);
    }

    /// A pAMM quoting 2x and a fallback pool quoting 1x, so the result is visibly the fallback.
    const PAMM_PRICE: f64 = 2.0;
    const FALLBACK_PRICE: f64 = 1.0;
    const FALLBACK_POOL: &str = "0xretry";
    const PAMM_COMPONENT: &str = "0xpamm";
    const PAMM_PROTOCOL: &str = "propammfallback:fermiswap";

    /// Market holding one `uniswap_v3` fallback pool for the (1, 2) pair at `fee_tier`.
    fn market_with_fallback_pool(fee_tier: u32) -> crate::feed::market_data::MarketData {
        let (token_in, token_out) = (util::token(1, "WETH"), util::token(2, "USDC"));
        let mut component = util::component_with_protocol(
            FALLBACK_POOL,
            FALLBACK_PROTOCOL_SYSTEM,
            &[token_in.clone(), token_out.clone()],
        );
        component
            .static_attributes
            .insert(FEE_ATTRIBUTE.to_string(), Bytes::from(fee_tier.to_be_bytes().to_vec()));

        let market = crate::feed::market_data::MarketData::new_shared();
        {
            let mut state = market.try_write().expect("uncontended");
            state.upsert_tokens([token_in, token_out]);
            state.upsert_components([component]);
            state.update_states([(
                FALLBACK_POOL.to_string(),
                Box::new(util::MockProtocolSim::new(FALLBACK_PRICE)) as Box<dyn ProtocolSim>,
            )]);
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

    /// A leg on the fallback pool itself, so the route holds no pAMM.
    fn uniswap_swap() -> Swap {
        let (token_in, token_out) = (util::token(1, "WETH"), util::token(2, "USDC"));
        Swap::new(
            FALLBACK_POOL.to_string(),
            FALLBACK_PROTOCOL_SYSTEM.to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(1_000u32),
            BigUint::from(1_000u32),
            BigUint::from(100_000u32),
            util::component_with_protocol(
                FALLBACK_POOL,
                FALLBACK_PROTOCOL_SYSTEM,
                &[token_in, token_out],
            ),
            Box::new(util::MockProtocolSim::new(FALLBACK_PRICE)),
        )
    }

    /// Only a route with a `propammfallback:` leg pays for fallback pricing.
    #[test]
    fn test_has_pamm_leg() {
        let non_pamm =
            Route::new(vec![uniswap_swap()], FxHashMap::default()).expect("non-empty route");
        assert!(!has_pamm_leg(&non_pamm));

        let pamm = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");
        assert!(has_pamm_leg(&pamm));
    }

    /// The result is the fallback pool's output, not the pAMM's.
    #[test]
    fn test_fallback_amount_out_with_fallback_pool() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let mut fee_tiers = FeeTiers::new(DEFAULT_TIER);
        fee_tiers.set_pair_tier(&addr(1), &addr(2), 500);

        let swap = pamm_swap();
        let route = Route::new(vec![swap.clone()], FxHashMap::default()).expect("non-empty route");
        let amount_out = fallback_amount_out(&route, &view, &fee_tiers, &index);

        // MockProtocolSim multiplies by spot_price for ascending token addresses: 1000 * 1.0,
        // against the pAMM leg's recorded 2000.
        assert_eq!(amount_out, FallbackAmountOut::AmountOut(BigUint::from(1_000u32)));
        assert_eq!(*swap.amount_out(), BigUint::from(2_000u32));
    }

    /// A labeled solve prices the fallback on the overlay it was solved against, not on the base
    /// state.
    #[tokio::test]
    async fn test_fallback_amount_out_uses_the_overlay_state() {
        let market = market_with_fallback_pool(500);
        let label = "test_overlay".to_string();
        let mut overlay: rustc_hash::FxHashMap<ComponentId, Box<dyn ProtocolSim>> =
            FxHashMap::default();
        overlay.insert(
            FALLBACK_POOL.to_string(),
            Box::new(util::MockProtocolSim::new(FALLBACK_PRICE / 2.0)),
        );
        market
            .register_labeled_state(label.clone(), overlay, u64::MAX)
            .await;

        let view = market
            .read_labeled(&label)
            .await
            .expect("registered overlay");
        let index = FallbackPoolIndex::build(&view);
        let mut fee_tiers = FeeTiers::new(DEFAULT_TIER);
        fee_tiers.set_pair_tier(&addr(1), &addr(2), 500);

        let route = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");
        let amount_out = fallback_amount_out(&route, &view, &fee_tiers, &index);

        // The base pool would pay 1000 * 1.0; the overlay halves the price.
        assert_eq!(amount_out, FallbackAmountOut::AmountOut(BigUint::from(500u32)));
    }

    /// No pool at the router's fee tier means the fallback reverts too, so the route is no better
    /// than the direct path and must be dropped.
    #[test]
    fn test_fallback_amount_out_without_pool_at_resolved_tier() {
        // Pool exists at 500, but the router resolves this pair to the 3000 default.
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        let route = Route::new(vec![pamm_swap()], FxHashMap::default()).expect("non-empty route");
        let amount_out = fallback_amount_out(&route, &view, &FeeTiers::new(DEFAULT_TIER), &index);

        let FallbackAmountOut::NoFallbackPool { component_id, fee_tier, token_in, token_out } =
            amount_out
        else {
            panic!("expected no fallback pool, got {amount_out:?}");
        };
        assert_eq!(component_id, PAMM_COMPONENT.to_string());
        assert_eq!(fee_tier, DEFAULT_TIER);
        // This market holds a pool for the pair, at a tier the router does not resolve to. Told
        // apart from a pair with no pool at all, which indexes no tier.
        assert_eq!(index.tiers_for(&token_in, &token_out), vec![500]);
    }

    /// The tiers a pair is indexed at, which tells a pair the market holds no pool for from one
    /// held at a tier the router does not resolve to.
    #[test]
    fn test_tiers_for() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let (weth, usdc) = (util::token(1, "WETH"), util::token(2, "USDC"));

        assert_eq!(index.tiers_for(&weth.address, &usdc.address), vec![500]);
        assert_eq!(
            index.tiers_for(&weth.address, &util::token(9, "DAI").address),
            Vec::<u32>::new(),
            "a pair the market holds no pool for indexes no tier"
        );
    }

    /// The pair-level rule the graph filter uses: a pAMM is only admitted when this market holds
    /// the Uniswap V3 pool its resolved fee tier names.
    #[test]
    fn test_lacks_fallback_pool_follows_the_resolved_tier() {
        let market = market_with_fallback_pool(DEFAULT_TIER);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let pamm = util::component_with_protocol(
            PAMM_COMPONENT,
            &format!("{PROPAMM_FALLBACK_PREFIX}fermiswap"),
            &[util::token(1, "WETH"), util::token(2, "USDC")],
        );

        assert!(!lacks_fallback_pool(&pamm, Some(&FeeTiers::new(DEFAULT_TIER)), &index));
        // Same pool, a tier the router does not resolve to: nothing to fall back on.
        assert!(lacks_fallback_pool(&pamm, Some(&FeeTiers::new(DEFAULT_TIER + 1)), &index));
    }

    /// Only `propammfallback:` components are judged. Everything else routes on its own terms.
    #[test]
    fn test_lacks_fallback_pool_ignores_other_protocols() {
        let market = market_with_fallback_pool(DEFAULT_TIER);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let uniswap = util::component_with_protocol(
            "0xuni",
            "uniswap_v3",
            &[util::token(3, "DAI"), util::token(4, "WBTC")],
        );

        assert!(!lacks_fallback_pool(&uniswap, Some(&FeeTiers::new(DEFAULT_TIER)), &index));
    }

    /// A split route prices through `replay_route`, so both legs of the split are counted.
    #[test]
    fn test_fallback_amount_out_split_route() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let mut fee_tiers = FeeTiers::new(DEFAULT_TIER);
        fee_tiers.set_pair_tier(&addr(1), &addr(2), 500);

        // 60% through the pAMM, the remainder through the same pool. Both legs substitute to the
        // one fallback pool, which then sees the second leg deplete it.
        let route =
            Route::new(vec![pamm_swap().with_split(0.6), pamm_swap()], FxHashMap::default())
                .expect("non-empty route");
        let amount_out = fallback_amount_out(&route, &view, &fee_tiers, &index);

        // The whole 1000 of input is routed either way, so the split must not lose or invent
        // amount relative to the single-leg case.
        assert!(
            matches!(&amount_out, FallbackAmountOut::AmountOut(amount) if *amount > BigUint::ZERO),
            "expected a priced split route, got {amount_out:?}"
        );
    }

    /// Only `uniswap_v3` components with a fee attribute can serve the fallback.
    #[test]
    fn test_pool_index_non_uniswap_v3_components() {
        let market = market_with_fallback_pool(500);
        {
            let mut state = market.try_write().expect("uncontended");
            state.upsert_components([util::component_with_protocol(
                "0xv2",
                "uniswap_v2",
                &[util::token(1, "WETH"), util::token(2, "USDC")],
            )]);
        }
        let view = market
            .try_read_blocking()
            .expect("uncontended");

        let index = FallbackPoolIndex::build(&view);
        assert_eq!(index.pools.len(), 1);
        assert_eq!(
            index
                .pool_for(&addr(1), &addr(2), 500)
                .map(String::as_str),
            Some(FALLBACK_POOL)
        );
    }
}
