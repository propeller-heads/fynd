//! The depth search for pools without a native `PoolTargetPrice` query.
//!
//! A pool's depth is the input of `token_in` after which its marginal price, net of the pool's fee,
//! has fallen to a target. Pools that answer `query_pool_swap` with a `PoolTargetPrice`
//! constraint give it directly; this search finds it for the others by swapping and reading the
//! marginal price of the state each swap returns.
//!
//! tycho's `PoolTargetPrice` answers read the target net of the fee, `price * (1 - fee)`. Its
//! `spot_price` is marked up by the fee, `price / (1 - fee)`, for some pools (Uniswap V2 and V3)
//! and net of it for others (many Uniswap V4 pools). Marked up in both directions,
//! `1 / spot_price(token_out, token_in)` is the net price; net in both,
//! `spot_price(token_in, token_out)` is. So the lower of the two is the net marginal price either
//! way, and the search reads that, the quantity the native answers compare with their target.
//!
//! The steps follow tycho-simulation's generic `query_pool_swap`, as of tycho-simulation 0.423:
//! the same choice of the next amount (inverse quadratic, then secant, then the geometric mean).
//! It differs in four ways:
//!
//! - It keeps no copy of the pool state beyond the swap it reads.
//! - It walks down from the input limit before it narrows the bracket. tycho's search halves a
//!   bracket that starts at 0, so a depth far under the input limit can use all 30 swaps before any
//!   amount meets the target, and the search then returns 0.
//! - It swaps the input limit only when no walk amount misses the target, and returns the input
//!   limit when even that swap leaves the price above the target.
//! - It stops at `RELATIVE_PRECISION`, not at a bracket of 1 unit.

use num_bigint::BigUint;
use num_traits::{FromPrimitive, One, ToPrimitive, Zero};
use tycho_simulation::tycho_common::{
    models::token::Token,
    simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
};

use crate::algorithm::sim_guard::GuardedProtocolSim;

/// The most swaps the search runs after it brackets the depth.
const MAX_ITERATIONS: u32 = 30;
/// The smallest share of the bracket an inverse quadratic step must cut off to be taken.
const INVERSE_QUADRATIC_MIN_CUT: f64 = 0.01;
/// The search stops when the bracket is narrower than this share of its lower end. A pool depth
/// needs no more precision than this.
const RELATIVE_PRECISION: f64 = 1e-5;
/// The factor the walk divides the amount by at each step.
const BRACKET_STEP: u32 = 1000;

/// One swap the search ran: the input and the net marginal price of the state it left.
struct PricePoint {
    amount_in: BigUint,
    price: f64,
}

/// The largest input of `token_in` after which the pool's net marginal price stays at or above
/// `target_price`. `marginal_price` is the net marginal price before the swap. Both prices are in
/// whole tokens of `token_out` per whole token of `token_in`.
///
/// # Errors
///
/// `InvalidInput` when the target is above the marginal price, and any error of the pool's own
/// simulation on the swaps that bracket the depth.
pub(crate) fn search_depth(
    state: &dyn ProtocolSim,
    marginal_price: f64,
    target_price: f64,
    token_in: &Token,
    token_out: &Token,
) -> Result<BigUint, SimulationError> {
    if target_price > marginal_price {
        return Err(SimulationError::InvalidInput(
            format!("Target {target_price} > marginal price {marginal_price}"),
            None,
        ));
    }

    let (max_in, _) = state.get_limits(token_in.address.clone(), token_out.address.clone())?;

    let mut search = Search {
        state,
        token_in,
        token_out,
        target_price,
        points: vec![PricePoint { amount_in: BigUint::zero(), price: marginal_price }],
        best: BigUint::zero(),
        best_error: (marginal_price - target_price) / target_price,
    };
    let mut low = BigUint::zero();
    let mut high = max_in.clone();

    // Walk down from the input limit, dividing the amount by `BRACKET_STEP` at each step, until
    // an amount keeps the price above the target. The module doc says why.
    let mut amount = &max_in / BRACKET_STEP;
    while !amount.is_zero() {
        // A pool can reject an amount the walk tries, such as one too small to swap. The walk
        // then stops, and the loop below brackets the depth from what it has.
        let Ok(price) = search.swap(&amount) else {
            break;
        };
        if price == target_price {
            return Ok(amount);
        }
        if price > target_price {
            low = amount;
            break;
        }
        high = amount.clone();
        amount /= BRACKET_STEP;
    }

    // A walk amount that takes the price under the target bounds the depth, and the input limit
    // then takes it under the target too. The search swaps the input limit only when no walk
    // amount did, because that swap crosses every tick of a concentrated-liquidity pool.
    if high == max_in {
        let limit_price = search.swap(&max_in)?;
        if limit_price >= target_price {
            return Ok(max_in);
        }
        // tycho's search keeps the input limit's point second, after the spot price.
        if let Some(limit_point) = search.points.pop() {
            search.points.insert(1, limit_point);
        }
    }

    for _ in 0..MAX_ITERATIONS {
        let amount = next_amount(&search.points, &low, &high, target_price);
        let price = search.swap(&amount)?;
        // tycho's stop rule with its zero tolerance: the price hits the target exactly.
        if price == target_price {
            return Ok(amount);
        }
        if price > target_price {
            low = amount;
        } else {
            high = amount;
        }
        if &high - &low <= BigUint::one() || is_precise_enough(&low, &high) {
            break;
        }
    }
    Ok(search.best)
}

/// The swaps one depth search ran, and the amount whose price came closest to the target from
/// above.
struct Search<'a> {
    state: &'a dyn ProtocolSim,
    token_in: &'a Token,
    token_out: &'a Token,
    target_price: f64,
    points: Vec<PricePoint>,
    best: BigUint,
    /// How far above the target the price after `best` is, as a share of the target.
    best_error: f64,
}

impl Search<'_> {
    /// Swaps `amount`, records the net marginal price of the state it leaves as a price point, and
    /// keeps `amount` as the best one when that price meets the target closer than the best so
    /// far. Returns the price.
    fn swap(&mut self, amount: &BigUint) -> Result<f64, SimulationError> {
        let new_state = self
            .state
            .get_amount_out_guarded(amount.clone(), self.token_in, self.token_out)?
            .new_state;
        let price = net_marginal_price(new_state.as_ref(), self.token_in, self.token_out)?;
        self.points
            .push(PricePoint { amount_in: amount.clone(), price });
        if price >= self.target_price {
            let error = (price - self.target_price) / self.target_price;
            if error < self.best_error {
                self.best_error = error;
                self.best = amount.clone();
            }
        }
        Ok(price)
    }
}

/// Whether the bracket is narrower than `RELATIVE_PRECISION` of its lower end, so every amount in
/// it is within that share of the depth.
fn is_precise_enough(low: &BigUint, high: &BigUint) -> bool {
    let low_f64 = to_f64(low);
    low_f64 > 0.0 && (to_f64(high) - low_f64) <= low_f64 * RELATIVE_PRECISION
}

/// The lower of the pool's two spot quotes for `token_in` → `token_out`, which is its marginal
/// price net of its fee. The module doc says why.
pub(crate) fn net_marginal_price(
    state: &dyn ProtocolSim,
    token_in: &Token,
    token_out: &Token,
) -> Result<f64, SimulationError> {
    let spot_price = state.spot_price(token_in, token_out)?;
    let reverse_spot_price = state.spot_price(token_out, token_in)?;
    Ok(spot_price.min(1.0 / reverse_spot_price))
}

/// `value` as an `f64`. A value past `f64::MAX` gives infinity.
fn to_f64(value: &BigUint) -> f64 {
    value.to_f64().unwrap_or(f64::INFINITY)
}

/// The next input to try: an inverse quadratic step when it cuts the bracket enough, else a
/// secant step, else the geometric mean of the bracket.
fn next_amount(points: &[PricePoint], low: &BigUint, high: &BigUint, target_price: f64) -> BigUint {
    let low_f64 = to_f64(low);
    let high_f64 = to_f64(high);
    let inside =
        |estimate: f64| BigUint::from_f64(estimate).filter(|amount| amount > low && amount < high);
    let len = points.len();

    if len >= 3 {
        if let Some(estimate) = inverse_quadratic(&points[len - 3..], target_price) {
            let cut = (estimate - low_f64).min(high_f64 - estimate);
            if cut > (high_f64 - low_f64) * INVERSE_QUADRATIC_MIN_CUT {
                if let Some(amount) = inside(estimate) {
                    return amount;
                }
            }
        }
    }
    if len >= 2 {
        if let Some(amount) = secant(&points[len - 2..], target_price).and_then(inside) {
            return amount;
        }
    }
    geometric_mean(low, high)
}

fn inverse_quadratic(points: &[PricePoint], target_price: f64) -> Option<f64> {
    let [p0, p1, p2] = points else {
        return None;
    };
    let (a0, a1, a2) = (to_f64(&p0.amount_in), to_f64(&p1.amount_in), to_f64(&p2.amount_in));
    let (pr0, pr1, pr2) = (p0.price, p1.price, p2.price);

    let d1 = (pr0 - pr1) * (pr0 - pr2);
    let d2 = (pr1 - pr0) * (pr1 - pr2);
    let d3 = (pr2 - pr0) * (pr2 - pr1);

    let result = a0 * (target_price - pr1) * (target_price - pr2) / d1 +
        a1 * (target_price - pr0) * (target_price - pr2) / d2 +
        a2 * (target_price - pr0) * (target_price - pr1) / d3;

    (result.is_finite() && result > 0.0).then_some(result)
}

fn secant(points: &[PricePoint], target_price: f64) -> Option<f64> {
    let [p0, p1] = points else {
        return None;
    };
    let (a0, a1) = (to_f64(&p0.amount_in), to_f64(&p1.amount_in));
    let result = a1 - (p1.price - target_price) * (a1 - a0) / (p1.price - p0.price);
    (result.is_finite() && result > 0.0).then_some(result)
}

fn geometric_mean(a: &BigUint, b: &BigUint) -> BigUint {
    let (a_f64, b_f64) = (to_f64(a), to_f64(b));
    if a_f64 <= 0.0 || b_f64 <= 0.0 {
        return (a + b) / 2u32;
    }
    BigUint::from_f64((a_f64 * b_f64).sqrt()).unwrap_or_else(|| (a + b) / 2u32)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::simulation::protocol_sim::{Price, QueryPoolSwapParams, SwapConstraint},
    };

    use super::*;
    use crate::algorithm::test_utils::token_with_decimals;

    /// A UniswapV2 pool holding `reserve_in` whole `token_in` and `reserve_out` whole
    /// `token_out`, with `token_in` as its first token.
    fn univ2(
        reserve_in: u64,
        reserve_out: u64,
        decimals_in: u32,
        decimals_out: u32,
    ) -> (UniswapV2State, Token, Token) {
        let token_in = token_with_decimals(0x01, "IN", decimals_in);
        let token_out = token_with_decimals(0x02, "OUT", decimals_out);
        let state = UniswapV2State::new(
            U256::from(reserve_in) * U256::from(10u64).pow(U256::from(decimals_in)),
            U256::from(reserve_out) * U256::from(10u64).pow(U256::from(decimals_out)),
        );
        (state, token_in, token_out)
    }

    fn marginal_price(state: &dyn ProtocolSim, token_in: &Token, token_out: &Token) -> f64 {
        net_marginal_price(state, token_in, token_out).expect("the pool has spot prices")
    }

    /// The net marginal price the pool shows after swapping `amount`.
    fn price_after(
        state: &UniswapV2State,
        amount: &BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> f64 {
        let new_state = state
            .get_amount_out(amount.clone(), token_in, token_out)
            .expect("the pool swaps the amount")
            .new_state;
        marginal_price(new_state.as_ref(), token_in, token_out)
    }

    /// Searches the depth at `target_price` and checks that it is the depth: the price after it
    /// stays at or above the target, and after 2.5e-5 more input it falls under.
    fn assert_search_finds_depth(
        state: &UniswapV2State,
        target_price: f64,
        token_in: &Token,
        token_out: &Token,
    ) {
        let marginal_price = marginal_price(state, token_in, token_out);

        let depth = search_depth(state, marginal_price, target_price, token_in, token_out)
            .expect("the search finds a depth");

        assert!(price_after(state, &depth, token_in, token_out) >= target_price);
        let past_depth = &depth + &depth / BigUint::from(40_000u32);
        assert!(price_after(state, &past_depth, token_in, token_out) < target_price);
    }

    #[rstest]
    #[case::same_decimals(18, 18, 1_000, 2_000)]
    #[case::high_to_low(18, 6, 5_000, 10_000_000)]
    #[case::low_to_high(6, 18, 10_000_000, 5_000)]
    #[case::small_difference(8, 18, 333, 5_000)]
    fn test_search_depth_at_one_and_a_half_percent(
        #[case] decimals_in: u32,
        #[case] decimals_out: u32,
        #[case] reserve_in: u64,
        #[case] reserve_out: u64,
    ) {
        let (state, token_in, token_out) =
            univ2(reserve_in, reserve_out, decimals_in, decimals_out);
        let target_price = marginal_price(&state, &token_in, &token_out) * 0.985;

        assert_search_finds_depth(&state, target_price, &token_in, &token_out);
    }

    #[rstest]
    #[case::same_decimals(18, 18)]
    #[case::high_to_low(18, 6)]
    #[case::low_to_high(6, 18)]
    fn test_search_depth_matches_native_pool_target_price(
        #[case] decimals_in: u32,
        #[case] decimals_out: u32,
    ) {
        // UniswapV2 answers `PoolTargetPrice` itself, so its answer is the depth this search
        // must find for pools that cannot, from the same target.
        let (state, token_in, token_out) = univ2(5_000, 10_000_000, decimals_in, decimals_out);
        let marginal = marginal_price(&state, &token_in, &token_out);
        let target_price = marginal * 0.985;
        let decimal_diff = i32::try_from(decimals_in).expect("decimals fit") -
            i32::try_from(decimals_out).expect("decimals fit");
        let target = Price::new(
            BigUint::from((target_price * 1e18) as u128),
            BigUint::from(10u64).pow(u32::try_from(18 + decimal_diff).expect("positive")),
        );
        let params = QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let native = state
            .query_pool_swap(&params)
            .expect("UniswapV2 answers PoolTargetPrice")
            .amount_in()
            .clone();

        let searched = search_depth(&state, marginal, target_price, &token_in, &token_out)
            .expect("the search finds a depth");

        // tycho's answer keeps the reserve product fixed, while a real swap leaves the fee in the
        // pool and grows it, so the two differ by about half the 0.3% fee.
        let difference = (to_f64(&searched) - to_f64(&native)).abs() / to_f64(&native);
        assert!(difference < 0.003, "searched {searched}, native {native}");
    }

    #[test]
    fn test_search_depth_far_under_the_input_limit() {
        let (state, token_in, token_out) = univ2(1_000_000, 2_000_000, 18, 18);
        let target_price = marginal_price(&state, &token_in, &token_out) * (1.0 - 1e-7);
        let (max_in, _) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("the pool has limits");

        let depth = search_depth(
            &state,
            marginal_price(&state, &token_in, &token_out),
            target_price,
            &token_in,
            &token_out,
        )
        .expect("the search finds a depth");

        assert!(depth < &max_in / BRACKET_STEP);
        assert!(price_after(&state, &depth, &token_in, &token_out) >= target_price);
    }

    #[test]
    fn test_search_depth_target_above_marginal_price() {
        let (state, token_in, token_out) = univ2(1_000, 2_000, 18, 18);
        let marginal = marginal_price(&state, &token_in, &token_out);

        let result = search_depth(&state, marginal, marginal * 1.01, &token_in, &token_out);

        assert!(matches!(result, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn test_search_depth_target_under_the_limit_price() {
        // Swapping the whole input limit leaves the price above a target this low, so the depth
        // is the input limit.
        let (state, token_in, token_out) = univ2(1_000, 2_000, 18, 18);
        let marginal = marginal_price(&state, &token_in, &token_out);
        let (max_in, _) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("the pool has limits");

        let depth = search_depth(&state, marginal, marginal * 1e-9, &token_in, &token_out)
            .expect("the search returns the input limit");

        assert_eq!(depth, max_in);
    }
}
