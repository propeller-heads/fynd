//! APEX pool construction from the solver's live market state, following Turbine's split:
//! V2/V3-family pools convert to APEX's native pool models (fast path in the price-search hot
//! loop), everything else wraps its `ProtocolSim` in a [`TychoApexPool`] adapter.
//!
//! The adapter is copied from Turbine (`src/clearing_algorithm/apex/solver.rs`), with Turbine's
//! protocol enum replaced by the protocol-system string and its token metadata sourced from the
//! market state's registry.

use std::{collections::HashMap, sync::Arc};

use alloy::primitives::Address as AlloyAddress;
use apex_solver::{
    core::{
        pools::{
            custom::ApexPool,
            uniswap_v2::UniswapV2Pool,
            uniswap_v3::{TickInfo, TickList, UniswapV3Pool},
            Pool, PoolMetadata,
        },
        Fraction,
    },
    types::{Address as ApexAddress, U256 as ApexU256},
};
use fynd_core::feed::market_data::MarketState;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::debug;
use tycho_simulation::{
    evm::protocol::uniswap_v2::state::UniswapV2State,
    tycho_common::models::token::Token as TychoToken,
    tycho_core::simulation::protocol_sim::{
        Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint,
    },
};

use super::{
    alloy_from_bytes, apex_addr,
    snapshot::{scale_down_floor, scale_up},
    ChainTokens,
};

/// Protocols whose state converts to APEX's native V2 pool.
const V2_PROTOCOLS: [&str; 3] = ["uniswap_v2", "sushiswap_v2", "pancakeswap_v2"];
/// Protocols whose state converts to APEX's native V3 pool.
const V3_PROTOCOLS: [&str; 2] = ["uniswap_v3", "pancakeswap_v3"];

#[derive(Debug, Default, Serialize)]
pub(crate) struct PoolCounts {
    pub native_v2: usize,
    pub native_v3: usize,
    pub wrapped: usize,
    pub skipped: usize,
}

/// Build the APEX pool set over `universe`. Every component contributes one APEX pool per
/// token pair whose (ETH-folded) tokens are both in the universe: two-token pools map 1:1,
/// multi-token pools (Curve tripools, Balancer weighted) expand into one pair view per
/// combination, all views sharing the component's simulation. Native ETH (the zero address)
/// folds into WETH on the APEX side while the simulation keeps its real ETH token, so
/// native-ETH pools (Curve stETH/ETH, Uniswap V4) join the WETH liquidity graph.
pub(crate) fn build_pools(
    state: &MarketState,
    universe: &HashMap<AlloyAddress, TychoToken>,
    chain: &ChainTokens,
) -> (Vec<Pool>, PoolCounts) {
    let mut pools = Vec::new();
    let mut counts = PoolCounts::default();

    for (component_id, token_addresses) in state.component_topology() {
        let views = pair_views(state, &token_addresses, universe, chain);
        if views.is_empty() {
            counts.skipped += 1;
            continue;
        }
        let Some(component) = state.get_component(&component_id) else {
            counts.skipped += 1;
            continue;
        };
        let Some(sim) = state.get_simulation_state(&component_id) else {
            counts.skipped += 1;
            continue;
        };
        let protocol = component.protocol_system.as_str();

        for (view_ix, view) in views.iter().enumerate() {
            let metadata = PoolMetadata {
                address: view_address(&component_id, view_ix),
                token_0: apex_addr(view.token_0),
                token_1: apex_addr(view.token_1),
            };

            // Native fast paths only ever apply to the first (and only) view: V2/V3 pools are
            // two-token and never use native ETH.
            if view_ix == 0 && views.len() == 1 {
                if V2_PROTOCOLS.contains(&protocol) {
                    if let Some(v2) = native_v2(sim, view.tycho_0.decimals, view.tycho_1.decimals) {
                        counts.native_v2 += 1;
                        pools.push(Pool::UniswapV2(metadata, v2));
                        continue;
                    }
                }
                if V3_PROTOCOLS.contains(&protocol) {
                    if let Some(v3) = native_v3(sim) {
                        counts.native_v3 += 1;
                        pools.push(Pool::UniswapV3(metadata, v3));
                        continue;
                    }
                    // A V3 pool that fails native extraction (odd fee tier, unserializable
                    // state) still solves through the wrapper below.
                    debug!(component_id, "V3 native extraction failed; wrapping instead");
                }
            }

            let wrapper = TychoApexPool {
                protocol: protocol.to_string(),
                tokens: HashMap::from([
                    (apex_addr(view.token_0), view.tycho_0.clone()),
                    (apex_addr(view.token_1), view.tycho_1.clone()),
                ]),
                pool: Arc::from(sim.clone_box()),
            };
            counts.wrapped += 1;
            pools.push(Pool::Apex(metadata, Arc::new(wrapper)));
        }
    }

    (pools, counts)
}

/// One APEX-side pair view of a component: the (ETH-folded) addresses APEX routes by, and the
/// component's real Tycho tokens the simulation is called with.
struct PairView {
    token_0: AlloyAddress,
    token_1: AlloyAddress,
    tycho_0: TychoToken,
    tycho_1: TychoToken,
}

/// The component's in-universe pair views. Native ETH folds into WETH on the APEX side; a
/// component carrying both ETH and WETH is skipped (the fold would collide). Multi-token
/// components yield one view per token combination.
fn pair_views(
    state: &MarketState,
    addresses: &[tycho_simulation::tycho_common::models::Address],
    universe: &HashMap<AlloyAddress, TychoToken>,
    chain: &ChainTokens,
) -> Vec<PairView> {
    let weth = chain.wrapped_native;
    let mut mapped: Vec<(AlloyAddress, TychoToken)> = Vec::with_capacity(addresses.len());
    for address in addresses {
        let Some(alloy) = alloy_from_bytes(address.as_ref()) else {
            return Vec::new();
        };
        // The simulation needs the component's actual token (ETH), APEX the folded one (WETH).
        let Some(tycho_token) = state.get_token(address) else {
            return Vec::new();
        };
        let folded = if alloy.is_zero() { weth } else { alloy };
        mapped.push((folded, tycho_token.clone()));
    }
    let mut seen = std::collections::HashSet::new();
    if !mapped
        .iter()
        .all(|(a, _)| seen.insert(*a))
    {
        return Vec::new();
    }
    let mut views = Vec::new();
    for i in 0..mapped.len() {
        for j in (i + 1)..mapped.len() {
            if universe.contains_key(&mapped[i].0) && universe.contains_key(&mapped[j].0) {
                views.push(PairView {
                    token_0: mapped[i].0,
                    token_1: mapped[j].0,
                    tycho_0: mapped[i].1.clone(),
                    tycho_1: mapped[j].1.clone(),
                });
            }
        }
    }
    views
}

/// A distinct APEX address per pair view: APEX indexes pools by address, so a multi-token
/// component's views cannot share one. The component id keeps the first bytes; the last byte
/// carries the view index. A cross-component collision would need two component ids one
/// last-byte step apart — negligible, and it would only merge two pools' identities.
fn view_address(component_id: &str, view_ix: usize) -> ApexAddress {
    let mut address = ApexAddress::from(component_id);
    address.0[19] = address.0[19].wrapping_add(u8::try_from(view_ix & 0xff).unwrap_or(0));
    address
}

/// Downcast to `UniswapV2State` and convert: reserves scale up to APEX's 18-decimal precision
/// (Turbine's convention — APEX's V2 math runs entirely in the scaled domain).
fn native_v2(sim: &dyn ProtocolSim, decimals_0: u32, decimals_1: u32) -> Option<UniswapV2Pool> {
    let state = sim
        .as_any()
        .downcast_ref::<UniswapV2State>()?;
    Some(UniswapV2Pool {
        reserve_0: scale_up(apex_u256_from_alloy(state.reserve0), decimals_0),
        reserve_1: scale_up(apex_u256_from_alloy(state.reserve1), decimals_1),
    })
}

/// The serde image of `UniswapV3State` (its fields are private, but the state serializes via
/// typetag as `{"protocol", "state"}`). Any mismatch — an unmapped fee tier, an amount that
/// doesn't fit `serde_json`'s number range — returns `None` and the pool falls back to the
/// wrapped path, so this stays an optimization, never a correctness gate.
#[derive(Deserialize)]
struct V3StateJson {
    liquidity: serde_json::Value,
    sqrt_price: String,
    fee: String,
    tick: i32,
    ticks: V3TickListJson,
}

#[derive(Deserialize)]
struct V3TickListJson {
    ticks: Vec<V3TickJson>,
}

#[derive(Deserialize)]
struct V3TickJson {
    index: i32,
    net_liquidity: serde_json::Value,
}

fn native_v3(sim: &dyn ProtocolSim) -> Option<UniswapV3Pool> {
    let value = serde_json::to_value(sim).ok()?;
    let state: V3StateJson = serde_json::from_value(value.get("state")?.clone()).ok()?;
    let (fee_tier, tick_spacing) = fee_and_spacing(&state.fee)?;
    let liquidity: u128 = state
        .liquidity
        .to_string()
        .parse()
        .ok()?;
    let sqrt_price = parse_hex_u256(&state.sqrt_price)?;

    let mut ticks = Vec::with_capacity(state.ticks.ticks.len());
    for tick in &state.ticks.ticks {
        let net_liquidity: i128 = tick
            .net_liquidity
            .to_string()
            .parse()
            .ok()?;
        ticks.push(TickInfo::new(tick.index, net_liquidity));
    }
    ticks.sort_by_key(|t| t.index);

    Some(UniswapV3Pool {
        fee_tier,
        liquidity: ApexU256::from(liquidity),
        sqrt_price_x96: sqrt_price,
        tick: state.tick,
        ticks: TickList { tick_spacing, ticks },
    })
}

/// Tycho's `FeeAmount` variant name → (fee in hundredths of a bip, tick spacing). Turbine's map
/// plus the Pancake tiers tycho also models; unmapped tiers wrap instead.
fn fee_and_spacing(fee: &str) -> Option<(u32, u16)> {
    match fee {
        "Lowest" => Some((100, 1)),
        "Low" => Some((500, 10)),
        "MediumLow" => Some((2500, 50)),
        "Medium" => Some((3000, 60)),
        "High" => Some((10_000, 200)),
        _ => None,
    }
}

fn parse_hex_u256(hex: &str) -> Option<ApexU256> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    ApexU256::from_str_radix(stripped, 16).ok()
}

fn apex_u256_from_alloy(value: alloy::primitives::U256) -> ApexU256 {
    ApexU256::from_le_bytes(value.to_le_bytes::<32>())
}

fn u256_to_biguint(value: ApexU256) -> BigUint {
    BigUint::from_bytes_le(&value.to_le_bytes::<32>())
}

fn biguint_to_u256(value: &BigUint) -> ApexU256 {
    let bytes = value.to_bytes_le();
    if bytes.len() > 32 {
        return ApexU256::MAX;
    }
    ApexU256::from_le_slice(&bytes)
}

/// Adapter implementing APEX's custom-pool trait on a Tycho `ProtocolSim` — Turbine's
/// `TychoApexPool`, retargeted at hindsight's market state.
#[derive(Debug, Clone)]
struct TychoApexPool {
    protocol: String,
    /// Keyed by APEX address; values are the pool's actual Tycho tokens.
    tokens: HashMap<ApexAddress, TychoToken>,
    pool: Arc<dyn ProtocolSim>,
}

/// Opaque payload persisted per wrapped pool in the `ApexInputData` dump. apex-solver
/// round-trips it verbatim; only this module knows the shape.
#[derive(Serialize, Deserialize)]
struct CustomPoolPayload {
    protocol: String,
    tokens: Vec<TychoToken>,
    state: serde_json::Value,
}

impl ApexPool for TychoApexPool {
    /// APEX's supply query, answered by `ProtocolSim::query_pool_swap` with a target-price
    /// constraint. APEX prices live in the scaled (all-18-decimals) domain, so both fraction
    /// sides convert to the raw atomic-unit domain before reaching the simulation; the returned
    /// amount converts back. Any simulation error means "no supply at this price".
    fn query_supply(&self, pair: apex_solver::core::TradingPair, swap_price: Fraction) -> ApexU256 {
        // ProtocolSim prices are token_out/token_in, the inverse of APEX's orientation.
        let price = Price::new(
            u256_to_biguint(
                pair.buy_token
                    .increase_precision(swap_price.denominator),
            ),
            u256_to_biguint(
                pair.sell_token
                    .increase_precision(swap_price.numerator),
            ),
        );
        let (Some(token_in), Some(token_out)) = (
            self.tokens.get(&pair.buy_token.address),
            self.tokens
                .get(&pair.sell_token.address),
        ) else {
            return ApexU256::ZERO;
        };
        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target: price,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let amount_out = match self.pool.query_pool_swap(&params) {
            Ok(swap) => swap.amount_out().clone(),
            Err(_) => BigUint::ZERO,
        };
        pair.sell_token
            .increase_precision(biguint_to_u256(&amount_out))
    }

    fn get_amount_out(
        &self,
        token_in: ApexAddress,
        token_out: ApexAddress,
        amount_in: ApexU256,
        min_amount_out: ApexU256,
    ) -> ApexU256 {
        let (Some(tycho_in), Some(tycho_out)) =
            (self.tokens.get(&token_in), self.tokens.get(&token_out))
        else {
            return ApexU256::ZERO;
        };
        let raw_in = scale_down_floor(amount_in, tycho_in.decimals);
        let amount_out =
            match self
                .pool
                .get_amount_out(u256_to_biguint(raw_in), tycho_in, tycho_out)
            {
                Ok(result) => scale_up(biguint_to_u256(&result.amount), tycho_out.decimals),
                Err(_) => ApexU256::ZERO,
            };
        if amount_out >= min_amount_out {
            amount_out
        } else {
            ApexU256::ZERO
        }
    }

    fn to_snapshot_json(&self) -> Option<serde_json::Value> {
        let state = serde_json::to_value(&*self.pool as &dyn ProtocolSim).ok()?;
        let tokens: Vec<TychoToken> = self.tokens.values().cloned().collect();
        serde_json::to_value(CustomPoolPayload { protocol: self.protocol.clone(), tokens, state })
            .ok()
    }
}

/// Rebuild a wrapped pool from its dumped payload, so a captured batch re-solves against the
/// same pool set the live snapshot had. apex-solver carries these through as opaque JSON — only
/// this module knows the shape, and only hindsight has the `ProtocolSim` implementations. The
/// concrete state type round-trips because `ProtocolSim` is a `#[typetag::serde]` trait.
pub(crate) fn rebuild_wrapped(
    custom: &apex_solver::serialization::CustomPoolJson,
) -> anyhow::Result<Pool> {
    let payload: CustomPoolPayload = serde_json::from_value(custom.data.clone())
        .map_err(|e| anyhow::anyhow!("wrapped pool {}: bad payload: {e}", custom.id))?;
    let state: Box<dyn ProtocolSim> = serde_json::from_value(payload.state).map_err(|e| {
        anyhow::anyhow!("wrapped pool {}: state did not round-trip: {e}", custom.id)
    })?;

    let mut tokens = HashMap::with_capacity(payload.tokens.len());
    for token in payload.tokens {
        let address = alloy_from_bytes(&token.address)
            .ok_or_else(|| anyhow::anyhow!("wrapped pool {}: bad token address", custom.id))?;
        tokens.insert(apex_addr(address), token);
    }
    // `Address::from(&str)` is the same conversion the dump's ids were written with, view
    // index included.
    let address = ApexAddress::from(custom.id.as_str());
    let token_0 = ApexAddress::from(custom.token0.as_str());
    let token_1 = ApexAddress::from(custom.token1.as_str());

    let wrapper = TychoApexPool { protocol: payload.protocol, tokens, pool: Arc::from(state) };
    Ok(Pool::Apex(PoolMetadata { address, token_0, token_1 }, Arc::new(wrapper)))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::U256;
    use tycho_simulation::{
        evm::protocol::{
            uniswap_v3::{enums::FeeAmount, state::UniswapV3State},
            utils::uniswap::tick_list::TickInfo as TychoTickInfo,
        },
        tycho_common::models::Chain,
    };

    use super::*;

    fn tycho_token(address: &str, symbol: &str, decimals: u32) -> TychoToken {
        TychoToken::new(
            &tycho_simulation::tycho_common::models::Address::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    #[test]
    fn test_native_v3_extraction() {
        let ticks = vec![
            TychoTickInfo::new(-887_270, 197_200_850).unwrap(),
            TychoTickInfo::new(50_520, 1_006_476_937).unwrap(),
            TychoTickInfo::new(887_270, -1_203_677_787).unwrap(),
        ];
        let state = UniswapV3State::new(
            6_857_434_793_814,
            U256::from_str("2591207495334834035139096140614").unwrap(),
            FeeAmount::Low,
            69_754,
            ticks,
        )
        .unwrap();

        let pool = native_v3(&state).expect("native extraction should succeed");
        assert_eq!(pool.fee_tier, 500);
        assert_eq!(pool.liquidity, ApexU256::from(6_857_434_793_814u64));
        assert_eq!(
            pool.sqrt_price_x96,
            ApexU256::from_str_radix("2591207495334834035139096140614", 10).unwrap()
        );
        assert_eq!(pool.tick, 69_754);
        assert_eq!(pool.ticks.tick_spacing, 10);
        assert_eq!(pool.ticks.ticks.len(), 3);
        assert_eq!(pool.ticks.ticks[0].index, -887_270);
        assert_eq!(pool.ticks.ticks[0].net_liquidity, 197_200_850);
        assert_eq!(pool.ticks.ticks[2].net_liquidity, -1_203_677_787);
    }

    #[test]
    fn test_wrapped_pool_get_amount_out_scaling() {
        // 1000 WETH (18 dec) vs 4,000,000 USDC (6 dec): spot ~4000 USDC/WETH. Selling 1 WETH
        // through the 0.3% V2 fee should yield just under 3988 USDC, and the adapter must
        // return it in APEX's 18-decimal scale.
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let weth_apex = apex_addr(weth.parse().unwrap());
        let usdc_apex = apex_addr(usdc.parse().unwrap());
        let pool = TychoApexPool {
            protocol: "uniswap_v2".to_string(),
            tokens: HashMap::from([
                (weth_apex, tycho_token(weth, "WETH", 18)),
                (usdc_apex, tycho_token(usdc, "USDC", 6)),
            ]),
            // Tycho's V2 state maps reserve0 to the address-sorted first token: USDC < WETH.
            pool: Arc::new(UniswapV2State::new(
                U256::from(4_000_000u64) * U256::from(10u64).pow(U256::from(6)),
                U256::from(1_000u64) * U256::from(10u64).pow(U256::from(18)),
            )),
        };

        let one_weth_scaled = ApexU256::from(10u64).pow(ApexU256::from(18));
        let out = pool.get_amount_out(weth_apex, usdc_apex, one_weth_scaled, ApexU256::ZERO);

        // Constant-product with 0.3% fee: floor(1e18*997*4_000_000e6 / (1000e18*1000 + 1e18*997)).
        let expected_raw = 3_984_027_924u128;
        let expected_scaled =
            ApexU256::from(expected_raw) * ApexU256::from(10u64).pow(ApexU256::from(12));
        assert_eq!(out, expected_scaled);
    }

    #[test]
    fn test_fee_and_spacing_unmapped_tier_wraps() {
        assert_eq!(fee_and_spacing("Medium"), Some((3000, 60)));
        assert_eq!(fee_and_spacing("MediumHigh"), None);
    }

    /// A dumped wrapped pool can be rebuilt from its snapshot payload, so an input dump is a
    /// complete record of the batch and every solve over it can be deferred off the live path.
    /// `ProtocolSim` is a `#[typetag::serde]` trait, so the concrete state type round-trips.
    #[test]
    fn test_wrapped_pool_round_trips_through_its_snapshot() {
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let weth_apex = apex_addr(weth.parse().unwrap());
        let usdc_apex = apex_addr(usdc.parse().unwrap());
        let pool = TychoApexPool {
            protocol: "uniswap_v2".to_string(),
            tokens: HashMap::from([
                (weth_apex, tycho_token(weth, "WETH", 18)),
                (usdc_apex, tycho_token(usdc, "USDC", 6)),
            ]),
            pool: Arc::new(UniswapV2State::new(
                U256::from(4_000_000u64) * U256::from(10u64).pow(U256::from(6)),
                U256::from(1_000u64) * U256::from(10u64).pow(U256::from(18)),
            )),
        };
        let one_weth_scaled = ApexU256::from(10u64).pow(ApexU256::from(18));
        let before = pool.get_amount_out(weth_apex, usdc_apex, one_weth_scaled, ApexU256::ZERO);

        let snapshot = pool
            .to_snapshot_json()
            .expect("wrapped pool must serialize");
        let payload: CustomPoolPayload =
            serde_json::from_value(snapshot).expect("payload must deserialize");
        let state: Box<dyn ProtocolSim> =
            serde_json::from_value(payload.state).expect("ProtocolSim must round-trip");
        let rebuilt = TychoApexPool {
            protocol: payload.protocol,
            tokens: payload
                .tokens
                .into_iter()
                .map(|token| {
                    let address =
                        alloy_from_bytes(&token.address).expect("dumped token address is 20 bytes");
                    (apex_addr(address), token)
                })
                .collect(),
            pool: Arc::from(state),
        };

        assert_eq!(rebuilt.protocol, "uniswap_v2");
        assert_eq!(
            rebuilt.get_amount_out(weth_apex, usdc_apex, one_weth_scaled, ApexU256::ZERO),
            before,
        );
    }
}
