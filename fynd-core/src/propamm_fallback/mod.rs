//! Computes the amount out a route delivers when its pAMM legs fall back to Uniswap V3.
//!
//! The encoder derives `min_amount_out` from this number instead of from the pAMM quote.
//!
//! A pAMM quote only reaches the chain in the block the maker quotes for, so simulating a pAMM
//! route against a mined block reverts. Routing the leg through the PropAMMRouter
//! (`propammrouter:` protocol family) falls back to Uniswap V3 instead of reverting. The fallback
//! pays less than the pAMM quote, so a `min_amount_out` derived from that quote still reverts the
//! route. Deriving it from this number does not.

pub mod fee_fetcher;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use num_bigint::BigUint;
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim};

use crate::{
    algorithm::sim_guard::GuardedProtocolSim,
    feed::market_data::MarketDataView,
    types::{ComponentId, Swap},
};

/// Must match `tycho-execution`'s `PROPAMM_ROUTER_PREFIX`.
pub const PROPAMM_ROUTER_PREFIX: &str = "propammrouter:";

/// Protocol system whose pools the PropAMMRouter falls back to.
const FALLBACK_PROTOCOL_SYSTEM: &str = "uniswap_v3";

/// Static attribute carrying a Uniswap V3 pool's fee tier, in hundredths of a bip.
const FEE_ATTRIBUTE: &str = "fee";

/// The router's `fallbackFee` as deployed, used for pairs without a per-pair override.
const DEFAULT_FALLBACK_FEE: u32 = 3000;

/// Mirrors the router's `resolvedFee`: the per-pair override if set, else `fallbackFee`.
///
/// Both are settable on chain without an upgrade, so this is refreshed rather than hardcoded.
#[derive(Debug, Clone)]
pub struct FallbackFees {
    default_fee: u32,
    per_pair: HashMap<(Address, Address), u32>,
}

impl Default for FallbackFees {
    fn default() -> Self {
        Self { default_fee: DEFAULT_FALLBACK_FEE, per_pair: HashMap::new() }
    }
}

impl FallbackFees {
    /// Order-independent, like the router's `_pairKey`.
    pub fn resolved_fee(&self, token_a: &Address, token_b: &Address) -> u32 {
        self.per_pair
            .get(&sorted_pair(token_a, token_b))
            .copied()
            .unwrap_or(self.default_fee)
    }

    /// Replaces the global `fallbackFee`.
    pub fn set_default_fee(&mut self, fee: u32) {
        self.default_fee = fee;
    }

    /// A fee of 0 clears the override, matching the router's `setPairFee`.
    pub fn set_pair_fee(&mut self, token_a: &Address, token_b: &Address, fee: u32) {
        let key = sorted_pair(token_a, token_b);
        if fee == 0 {
            self.per_pair.remove(&key);
        } else {
            self.per_pair.insert(key, fee);
        }
    }
}

/// Shared with the task that refreshes it from the router.
#[derive(Debug, Clone, Default)]
pub struct SharedFallbackFees(Arc<RwLock<FallbackFees>>);

impl SharedFallbackFees {
    /// Returns a copy of the current fee tiers.
    pub fn snapshot(&self) -> FallbackFees {
        self.0
            .read()
            .expect("fallback fees lock poisoned")
            .clone()
    }

    /// Replaces the fee tiers with freshly fetched on-chain values.
    pub fn set(&self, fees: FallbackFees) {
        *self
            .0
            .write()
            .expect("fallback fees lock poisoned") = fees;
    }
}

/// Fallback pools by token pair and fee tier, so finding one is a lookup, not a market scan.
#[derive(Debug, Default)]
pub struct FallbackPoolIndex {
    pools: HashMap<(Address, Address, u32), ComponentId>,
}

impl FallbackPoolIndex {
    /// Skips components that are not two-token or carry no `fee` attribute. The router's fallback
    /// is a single-hop `exactInputSingle`, so nothing else can serve it.
    pub fn build(market: &MarketDataView<'_>) -> Self {
        let mut pools = HashMap::new();
        for (component_id, tokens) in market.component_topology() {
            let Some(component) = market.get_component(&component_id) else { continue };
            if component.protocol_system != FALLBACK_PROTOCOL_SYSTEM {
                continue;
            }
            let [token_a, token_b] = tokens.as_slice() else { continue };
            let Some(fee) = component
                .static_attributes
                .get(FEE_ATTRIBUTE)
                .and_then(parse_fee)
            else {
                continue;
            };
            let (low, high) = sorted_pair(token_a, token_b);
            pools.insert((low, high, fee), component_id);
        }
        Self { pools }
    }

    /// The component the router would fall back to.
    pub fn pool_for(&self, token_a: &Address, token_b: &Address, fee: u32) -> Option<&ComponentId> {
        let (low, high) = sorted_pair(token_a, token_b);
        self.pools.get(&(low, high, fee))
    }
}

/// The amount out a route delivers when its pAMM legs fall back to Uniswap V3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackAmountOut {
    /// No pAMM leg, so `min_amount_out` needs no adjustment.
    NoPropAmmLeg,
    /// Derive `min_amount_out` from this amount.
    AmountOut(BigUint),
    /// No Uniswap V3 pool at the router's fee tier, so the fallback reverts too. Drop the route.
    NoFallbackPool {
        /// The pAMM leg with no fallback pool.
        component_id: ComponentId,
        /// The fee tier the router would have used.
        fee: u32,
    },
    /// Split routes are not supported yet.
    SplitNotSupported,
}

/// Re-simulates `swaps` with every `propammrouter:` leg replaced by its fallback pool.
///
/// Walks left to right so a substituted leg's smaller output feeds the next one. Non-pAMM legs are
/// re-simulated too, since their input changes once an upstream leg falls back.
pub fn fallback_amount_out(
    swaps: &[Swap],
    market: &MarketDataView<'_>,
    fees: &FallbackFees,
    index: &FallbackPoolIndex,
) -> FallbackAmountOut {
    if !swaps.iter().any(|swap| {
        swap.protocol()
            .starts_with(PROPAMM_ROUTER_PREFIX)
    }) {
        return FallbackAmountOut::NoPropAmmLeg;
    }
    if swaps
        .iter()
        .any(|swap| *swap.split() != 0.0)
    {
        return FallbackAmountOut::SplitNotSupported;
    }

    let Some(first) = swaps.first() else { return FallbackAmountOut::NoPropAmmLeg };
    let mut amount = first.amount_in().clone();

    for swap in swaps {
        let Some(token_in) = market.get_token(swap.token_in()) else {
            return FallbackAmountOut::SplitNotSupported;
        };
        let Some(token_out) = market.get_token(swap.token_out()) else {
            return FallbackAmountOut::SplitNotSupported;
        };

        let state: &dyn ProtocolSim = if swap
            .protocol()
            .starts_with(PROPAMM_ROUTER_PREFIX)
        {
            let fee = fees.resolved_fee(swap.token_in(), swap.token_out());
            let Some(pool) = index.pool_for(swap.token_in(), swap.token_out(), fee) else {
                return FallbackAmountOut::NoFallbackPool {
                    component_id: swap.component_id().to_string(),
                    fee,
                };
            };
            match market.get_simulation_state(pool) {
                Some(state) => state,
                // Removed between building the index and this read; treat as no fallback pool.
                None => {
                    return FallbackAmountOut::NoFallbackPool {
                        component_id: swap.component_id().to_string(),
                        fee,
                    }
                }
            }
        } else {
            swap.protocol_state()
        };

        match state.get_amount_out_guarded(amount.clone(), token_in, token_out) {
            Ok(result) => amount = result.amount,
            // No usable amount, so no justifiable `min_amount_out`. Drop the route.
            Err(_) => {
                return FallbackAmountOut::NoFallbackPool {
                    component_id: swap.component_id().to_string(),
                    fee: fees.resolved_fee(swap.token_in(), swap.token_out()),
                }
            }
        }
    }

    FallbackAmountOut::AmountOut(amount)
}

/// Order-independent pair key, matching the router's `_pairKey`.
fn sorted_pair(token_a: &Address, token_b: &Address) -> (Address, Address) {
    if token_a <= token_b {
        (token_a.clone(), token_b.clone())
    } else {
        (token_b.clone(), token_a.clone())
    }
}

/// Tycho encodes the `fee` attribute big-endian in up to 4 bytes.
fn parse_fee(raw: &tycho_simulation::tycho_common::Bytes) -> Option<u32> {
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
    use tycho_simulation::tycho_common::Bytes;

    use super::*;
    use crate::algorithm::test_utils::{self as util, addr};

    #[test]
    fn test_resolved_fee_defaults_and_overrides() {
        let mut fees = FallbackFees::default();
        assert_eq!(fees.resolved_fee(&addr(1), &addr(2)), DEFAULT_FALLBACK_FEE);

        fees.set_pair_fee(&addr(2), &addr(1), 500);
        // Order-independent, like the router's sorted pair key.
        assert_eq!(fees.resolved_fee(&addr(1), &addr(2)), 500);
        assert_eq!(fees.resolved_fee(&addr(2), &addr(1)), 500);

        // A zero fee clears the override, matching `setPairFee`.
        fees.set_pair_fee(&addr(1), &addr(2), 0);
        assert_eq!(fees.resolved_fee(&addr(1), &addr(2)), DEFAULT_FALLBACK_FEE);

        fees.set_default_fee(100);
        assert_eq!(fees.resolved_fee(&addr(1), &addr(2)), 100);
    }

    #[test]
    fn test_parse_fee_accepts_tycho_encoding() {
        assert_eq!(parse_fee(&Bytes::from(3000_i32.to_be_bytes().to_vec())), Some(3000));
        // Tycho trims leading zero bytes on some attributes.
        assert_eq!(parse_fee(&Bytes::from(vec![0x01, 0xf4])), Some(500));
        assert_eq!(parse_fee(&Bytes::from(Vec::new())), None);
        assert_eq!(parse_fee(&Bytes::from(vec![0u8; 5])), None);
    }

    #[test]
    fn test_pool_for_is_order_independent() {
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

    /// A pAMM quoting 2x and a fallback pool quoting 1x, so the result is visibly the fallback.
    const PAMM_PRICE: f64 = 2.0;
    const FALLBACK_PRICE: f64 = 1.0;
    const FALLBACK_POOL: &str = "0xretry";
    const PAMM_COMPONENT: &str = "0xpamm";
    const PAMM_PROTOCOL: &str = "propammrouter:fermiswap";

    /// Market holding one `uniswap_v3` fallback pool for the (1, 2) pair at `fee`.
    fn market_with_fallback_pool(fee: u32) -> crate::feed::market_data::MarketData {
        let (token_in, token_out) = (util::token(1, "WETH"), util::token(2, "USDC"));
        let mut component = util::component_with_protocol(
            FALLBACK_POOL,
            FALLBACK_PROTOCOL_SYSTEM,
            &[token_in.clone(), token_out.clone()],
        );
        component
            .static_attributes
            .insert(FEE_ATTRIBUTE.to_string(), Bytes::from(fee.to_be_bytes().to_vec()));

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

    /// A route with no pAMM leg keeps its usual `min_amount_out`.
    #[test]
    fn test_fallback_amount_out_no_pamm_leg() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");

        assert_eq!(
            fallback_amount_out(
                &[],
                &view,
                &FallbackFees::default(),
                &FallbackPoolIndex::default()
            ),
            FallbackAmountOut::NoPropAmmLeg
        );
    }

    /// The result is the fallback pool's output, not the pAMM's.
    #[test]
    fn test_fallback_amount_out_uses_the_fallback_pool() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);
        let mut fees = FallbackFees::default();
        fees.set_pair_fee(&addr(1), &addr(2), 500);

        let swap = pamm_swap();
        let amount_out = fallback_amount_out(std::slice::from_ref(&swap), &view, &fees, &index);

        // MockProtocolSim multiplies by spot_price for ascending token addresses: 1000 * 1.0,
        // against the pAMM leg's recorded 2000.
        assert_eq!(amount_out, FallbackAmountOut::AmountOut(BigUint::from(1_000u32)));
        assert_eq!(*swap.amount_out(), BigUint::from(2_000u32));
    }

    /// No pool at the router's fee tier means the fallback reverts too, so the route is no better
    /// than the direct path and must be dropped.
    #[test]
    fn test_fallback_amount_out_none_when_no_pool_at_resolved_fee() {
        // Pool exists at 500, but the router resolves this pair to the 3000 default.
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        let swap = pamm_swap();
        let amount_out = fallback_amount_out(
            std::slice::from_ref(&swap),
            &view,
            &FallbackFees::default(),
            &index,
        );

        assert_eq!(
            amount_out,
            FallbackAmountOut::NoFallbackPool {
                component_id: PAMM_COMPONENT.to_string(),
                fee: DEFAULT_FALLBACK_FEE,
            }
        );
    }

    /// Split routes are out of scope for now; the caller must not silently treat them as backed.
    #[test]
    fn test_fallback_amount_out_split_not_supported() {
        let market = market_with_fallback_pool(500);
        let view = market
            .try_read_blocking()
            .expect("uncontended");
        let index = FallbackPoolIndex::build(&view);

        let swap = pamm_swap().with_split(0.5);
        let amount_out = fallback_amount_out(
            std::slice::from_ref(&swap),
            &view,
            &FallbackFees::default(),
            &index,
        );

        assert_eq!(amount_out, FallbackAmountOut::SplitNotSupported);
    }

    /// Only `uniswap_v3` components with a fee attribute can serve the fallback.
    #[test]
    fn test_pool_index_skips_non_uniswap_v3_components() {
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
