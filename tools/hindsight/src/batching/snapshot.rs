//! Builds one block's APEX input snapshot from the solver's live market state: the priced token
//! universe (Turbine-style: the block's traded tokens plus hub tokens), initial prices in APEX's
//! scaled-unit convention, the pool set over that universe, and the trades encoded as orders.

use std::collections::HashMap;

use alloy::primitives::Address as AlloyAddress;
use apex_solver::{
    core::{Fraction, LimitOrder, Token as ApexToken},
    types::U256 as ApexU256,
};
use fynd_core::{derived::TokenGasPrices, feed::market_data::MarketState};
use tracing::warn;
use tycho_simulation::tycho_common::models::token::Token as TychoToken;

use super::{apex_addr, apex_u256, pools, pools::PoolCounts, ChainTokens, PreparedOrder};
use crate::decoder::DecodedTrade;

/// APEX seeds unpriced tokens at 1e8 (`ApexConfig::starting_price`); Turbine anchors its price
/// vector so the cluster minimum sits there too. We normalize the same way.
const MIN_INITIAL_PRICE: f64 = 1e8;

/// Everything one block's APEX runs need, snapshotted so the market-data locks can be released
/// before the CPU-bound solves start.
pub(crate) struct Snapshot {
    pub apex_tokens: Vec<ApexToken>,
    pub initial_prices: HashMap<apex_solver::types::Address, ApexU256>,
    pub pools: Vec<apex_solver::core::pools::Pool>,
    pub pool_counts: PoolCounts,
    /// Trades encoded as APEX orders (indices into the block's trade slice).
    pub prepared: Vec<PreparedOrder>,
    /// Trade indices that never entered APEX because a token had no derived price.
    pub out_of_universe: Vec<usize>,
    /// Trades excluded up front: sandwich-flagged, zero amounts, or degenerate pairs.
    pub excluded_sandwiched: usize,
    /// Universe metadata for building records and scaling amounts.
    pub token_meta: HashMap<AlloyAddress, TokenMeta>,
}

#[derive(Clone)]
pub(crate) struct TokenMeta {
    pub symbol: String,
    pub decimals: u32,
    /// ETH value of one atomic unit (from the derived price), for cross-token aggregation in
    /// the report. Zero when unknown.
    pub eth_per_atomic: f64,
}

impl Snapshot {
    pub fn meta(&self, token: AlloyAddress) -> TokenMeta {
        self.token_meta
            .get(&token)
            .cloned()
            .unwrap_or(TokenMeta {
                symbol: format!("{token:#x}"),
                decimals: 18,
                eth_per_atomic: 0.0,
            })
    }
}

/// Build the block's snapshot. `token_prices` is fynd's derived map of "token atomic units per
/// wei of the gas token" — the priceability filter and the source of APEX's initial prices.
pub(crate) fn build_snapshot(
    state: &MarketState,
    token_prices: &TokenGasPrices,
    trades: &[DecodedTrade],
    chain: &ChainTokens,
) -> Snapshot {
    // Candidate universe: tokens the block's non-sandwiched trades touch, plus the hubs.
    let mut excluded_sandwiched = 0usize;
    let mut candidates: Vec<AlloyAddress> = chain.hubs.clone();
    for trade in trades {
        if trade.sandwich.is_some() {
            excluded_sandwiched += 1;
            continue;
        }
        let (sell, buy) = chain.fold(trade.token_in, trade.token_out);
        candidates.push(sell);
        candidates.push(buy);
    }
    candidates.sort();
    candidates.dedup();

    // Keep candidates that are registered, priceable, and price-computable. Everything below
    // (orders, pools, records) is defined over this universe.
    let mut universe: HashMap<AlloyAddress, TychoToken> = HashMap::new();
    let mut raw_prices: HashMap<AlloyAddress, f64> = HashMap::new();
    let mut eth_values: HashMap<AlloyAddress, f64> = HashMap::new();
    for address in candidates {
        let core_address =
            tycho_simulation::tycho_common::models::Address::from(address.into_array());
        let Some(token) = state.get_token(&core_address) else {
            continue;
        };
        let Some(price) = token_prices.get(&core_address) else {
            continue;
        };
        let Some((scaled_unit_value, eth_per_atomic)) = unit_values(price, token.decimals) else {
            continue;
        };
        raw_prices.insert(address, scaled_unit_value);
        eth_values.insert(address, eth_per_atomic);
        universe.insert(address, token.clone());
    }

    let initial_prices = normalize_prices(&raw_prices);

    let mut token_meta = HashMap::new();
    let mut apex_tokens = Vec::with_capacity(universe.len());
    for (address, token) in &universe {
        token_meta.insert(
            *address,
            TokenMeta {
                symbol: token.symbol.clone(),
                decimals: token.decimals,
                eth_per_atomic: eth_values
                    .get(address)
                    .copied()
                    .unwrap_or(0.0),
            },
        );
        apex_tokens.push(ApexToken::new(
            apex_addr(*address),
            &token.symbol,
            u8::try_from(token.decimals).unwrap_or(18),
        ));
    }

    let (pools, pool_counts) = pools::build_pools(state, &universe, chain);

    let (prepared, out_of_universe, more_excluded) = prepare_orders(trades, &universe, chain);
    let excluded_sandwiched = excluded_sandwiched + more_excluded;

    Snapshot {
        apex_tokens,
        initial_prices,
        pools,
        pool_counts,
        prepared,
        out_of_universe,
        excluded_sandwiched,
        token_meta,
    }
}

/// Encode each in-universe trade as one order per limit-price variant: permissive (limit is one
/// scaled atom for the full sell amount, so the order may always fill) and anchored (limit is
/// the actual settled execution price, so APEX must beat reality to fill). Returns the prepared
/// orders, the out-of-universe trade indices, and the count of degenerate trades excluded.
fn prepare_orders(
    trades: &[DecodedTrade],
    universe: &HashMap<AlloyAddress, TychoToken>,
    chain: &ChainTokens,
) -> (Vec<PreparedOrder>, Vec<usize>, usize) {
    let mut prepared = Vec::new();
    let mut out_of_universe = Vec::new();
    let mut excluded = 0usize;
    for (trade_ix, trade) in trades.iter().enumerate() {
        if trade.sandwich.is_some() {
            continue;
        }
        let (sell, buy) = chain.fold(trade.token_in, trade.token_out);
        if sell == buy || trade.amount_in.is_zero() {
            excluded += 1;
            continue;
        }
        if !universe.contains_key(&sell) || !universe.contains_key(&buy) {
            out_of_universe.push(trade_ix);
            continue;
        }
        let sell_decimals = universe[&sell].decimals;
        let scaled_sell = scale_up(apex_u256(trade.amount_in), sell_decimals);
        if scaled_sell.is_zero() {
            out_of_universe.push(trade_ix);
            continue;
        }
        let order_id = format!("{:#x}-{trade_ix}", trade.tx_hash);
        let permissive = LimitOrder::new(
            scaled_sell,
            Fraction::new(ApexU256::from(1u64), scaled_sell),
            order_id.clone(),
            apex_addr(trade.sender),
        );
        // A zero settled output degenerates the anchored limit to the permissive floor.
        let scaled_settled = scale_up(apex_u256(trade.amount_out), universe[&buy].decimals)
            .max(ApexU256::from(1u64));
        let anchored = LimitOrder::new(
            scaled_sell,
            Fraction::new(scaled_settled, scaled_sell),
            order_id.clone(),
            apex_addr(trade.sender),
        );
        // The user's signed slippage limit when the decoder recovered it; the anchored limit
        // otherwise (tighter than the true one, so fills stay valid).
        let (user_limit_scaled, limit_source) = match trade.min_amount_out {
            Some(min_out) => (
                scale_up(apex_u256(min_out), universe[&buy].decimals).max(ApexU256::from(1u64)),
                "calldata",
            ),
            None => (scaled_settled, "settled_fallback"),
        };
        let user_limit = LimitOrder::new(
            scaled_sell,
            Fraction::new(user_limit_scaled, scaled_sell),
            order_id,
            apex_addr(trade.sender),
        );
        prepared.push(PreparedOrder {
            trade_ix,
            permissive,
            anchored,
            user_limit,
            limit_source,
            scaled_sell,
            sell_token: sell,
            buy_token: buy,
        });
    }
    (prepared, out_of_universe, excluded)
}

/// The token's value per 18-decimal-scaled unit in wei, and per atomic unit in ETH. Derived
/// prices are "atomic units per wei", so one atomic unit is worth den/num wei; the scaled-unit
/// value is that times 10^(decimals-18).
fn unit_values(
    price: &tycho_simulation::tycho_core::simulation::protocol_sim::Price,
    decimals: u32,
) -> Option<(f64, f64)> {
    let numerator: f64 = price
        .numerator
        .to_string()
        .parse()
        .ok()?;
    let denominator: f64 = price
        .denominator
        .to_string()
        .parse()
        .ok()?;
    if numerator <= 0.0 || denominator <= 0.0 {
        return None;
    }
    let wei_per_atomic = denominator / numerator;
    let scaled_unit_value = wei_per_atomic * 10f64.powi(decimals.cast_signed() - 18);
    if !scaled_unit_value.is_finite() || scaled_unit_value <= 0.0 {
        return None;
    }
    Some((scaled_unit_value, wei_per_atomic / 1e18))
}

/// Scale relative values into APEX's integer price vector: the minimum lands at 1e8 and
/// everything keeps its ratio. Extreme spreads are clamped into u128 with a warning — such
/// tokens are dust and a clamped price only affects them.
fn normalize_prices(
    raw: &HashMap<AlloyAddress, f64>,
) -> HashMap<apex_solver::types::Address, ApexU256> {
    let mut prices = HashMap::new();
    let Some(min) = raw
        .values()
        .copied()
        .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.min(v))))
    else {
        return prices;
    };
    let scale = MIN_INITIAL_PRICE / min;
    for (address, value) in raw {
        // f64→u128 truncation is fine here: prices are ≥1e8 by construction, so the integer
        // part carries far more precision than the derived price itself.
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let clamped = {
            let scaled = value * scale;
            if scaled >= u128::MAX as f64 {
                warn!(token = %address, scaled, "initial price clamped");
            }
            scaled.min(u128::MAX as f64 / 2.0) as u128
        };
        prices.insert(apex_addr(*address), ApexU256::from(clamped.max(1)));
    }
    prices
}

/// Raw native units → APEX's 18-decimal scale.
pub(crate) fn scale_up(amount: ApexU256, decimals: u32) -> ApexU256 {
    let exp = 18u32.saturating_sub(decimals);
    amount.saturating_mul(ApexU256::from(10u64).pow(ApexU256::from(exp)))
}

/// APEX's 18-decimal scale → raw native units, flooring (what the user receives is never
/// overstated).
pub(crate) fn scale_down_floor(amount: ApexU256, decimals: u32) -> ApexU256 {
    let exp = 18u32.saturating_sub(decimals);
    amount / ApexU256::from(10u64).pow(ApexU256::from(exp))
}

/// APEX's 18-decimal scale → raw native units, ceiling (what the user sends is never
/// understated).
pub(crate) fn scale_down_ceil(amount: ApexU256, decimals: u32) -> ApexU256 {
    let exp = 18u32.saturating_sub(decimals);
    let divisor = ApexU256::from(10u64).pow(ApexU256::from(exp));
    let quotient = amount / divisor;
    if amount % divisor > ApexU256::ZERO {
        quotient + ApexU256::from(1u64)
    } else {
        quotient
    }
}
