//! The depth search for pools without a native `TradeLimitPrice` query.
//!
//! The search follows tycho-simulation's generic `query_pool_swap` for a trade limit price, as of
//! tycho-simulation 0.423: the same trade price, the same choice of the next amount (inverse
//! quadratic, then secant, then the geometric mean) and the same errors. It differs in five ways:
//!
//! - It returns only the amount, so it keeps no copy of the pool state. On a concentrated-liquidity
//!   pool those copies cost more than the swaps.
//! - It walks down from the input limit before it narrows the bracket. tycho's search halves a
//!   bracket that starts at 0, so a depth far under the input limit can use all 30 swaps before any
//!   amount meets the target, and the search then returns 0.
//! - It swaps the input limit only when no walk amount misses the target.
//! - It stops at `RELATIVE_PRECISION`, not at a bracket of 1 unit.
//! - A swap that returns zero raises the lower end of the bracket, where tycho's search lowers the
//!   upper end. A zero output comes from rounding on a tiny amount, not from a large one.

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
/// The smallest output, in the token's smallest unit, at which the walk trusts a price. Rounding
/// such an output moves its price by at most 0.0001%.
const MIN_WALK_OUT: u32 = 1_000_000;

/// One swap the search ran: the input and the trade price it got.
struct PricePoint {
    amount_in: BigUint,
    price: f64,
}

/// The largest input of `token_in` whose trade price, `amount_out / amount_in` in whole tokens,
/// stays at or above `target_price`. Both prices are in whole tokens of `token_out` per whole
/// token of `token_in`.
///
/// # Errors
///
/// `InvalidInput` when the target is above the spot price, or below the trade price of the pool's
/// whole input limit, and any error of the pool's own simulation.
pub(crate) fn search_depth(
    state: &dyn ProtocolSim,
    spot_price: f64,
    target_price: f64,
    token_in: &Token,
    token_out: &Token,
) -> Result<BigUint, SimulationError> {
    if target_price > spot_price {
        return Err(SimulationError::InvalidInput(
            format!("Target {target_price} > spot {spot_price}"),
            None,
        ));
    }

    let (max_in, _) = state.get_limits(token_in.address.clone(), token_out.address.clone())?;

    let mut search = Search {
        state,
        token_in,
        token_out,
        target_price,
        points: vec![PricePoint { amount_in: BigUint::zero(), price: spot_price }],
        best: BigUint::zero(),
        best_error: (spot_price - target_price) / target_price,
    };
    let mut low = BigUint::zero();
    let mut high = max_in.clone();

    // Walk down from the input limit, dividing the amount by `BRACKET_STEP` at each step, until
    // an amount meets the target. The module doc says why.
    let mut amount = &max_in / BRACKET_STEP;
    while !amount.is_zero() {
        // A pool can reject an amount the walk tries, such as one too small to swap. The walk
        // then stops, and the loop below brackets the depth from what it has.
        let Ok((amount_out, price)) = search.swap(&amount) else {
            break;
        };
        // Under `MIN_WALK_OUT`, rounding the output can move its price by more than the slippage
        // threshold. The walk stops here and leaves the bracket to the loop below, with `low`
        // still 0.
        if amount_out < BigUint::from(MIN_WALK_OUT) {
            break;
        }
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

    // A walk amount that trades under the target price bounds the depth, and the input limit then
    // trades under the target too. The search swaps the input limit only when no walk amount did,
    // because that swap crosses every tick of a concentrated-liquidity pool.
    if high == max_in {
        let limit_out = state
            .get_amount_out_guarded(max_in.clone(), token_in, token_out)?
            .amount;
        let limit_price = trade_price(&max_in, &limit_out, token_in, token_out);
        if target_price < limit_price {
            return Err(SimulationError::InvalidInput(
                format!("Target {target_price} < limit {limit_price}"),
                None,
            ));
        }
        search
            .points
            .insert(1, PricePoint { amount_in: max_in, price: limit_price });
    }

    for _ in 0..MAX_ITERATIONS {
        let amount = next_amount(&search.points, &low, &high, target_price);
        let (amount_out, price) = search.swap(&amount)?;
        // tycho's stop rule with its zero tolerance: the price hits the target exactly.
        if price == target_price {
            return Ok(amount);
        }
        if price > target_price || amount_out.is_zero() {
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
    /// How far above the target the price of `best` is, as a share of the target.
    best_error: f64,
}

impl Search<'_> {
    /// Swaps `amount`, records the swap as a price point, and keeps `amount` as the best one when
    /// its price meets the target closer than the best so far. Returns the output and the price.
    fn swap(&mut self, amount: &BigUint) -> Result<(BigUint, f64), SimulationError> {
        let amount_out = self
            .state
            .get_amount_out_guarded(amount.clone(), self.token_in, self.token_out)?
            .amount;
        let price = trade_price(amount, &amount_out, self.token_in, self.token_out);
        self.points
            .push(PricePoint { amount_in: amount.clone(), price });
        if price >= self.target_price {
            let error = (price - self.target_price) / self.target_price;
            if error < self.best_error {
                self.best_error = error;
                self.best = amount.clone();
            }
        }
        Ok((amount_out, price))
    }
}

/// Whether the bracket is narrower than `RELATIVE_PRECISION` of its lower end, so every amount in
/// it is within that share of the depth.
fn is_precise_enough(low: &BigUint, high: &BigUint) -> bool {
    let low_f64 = to_f64(low);
    low_f64 > 0.0 && (to_f64(high) - low_f64) <= low_f64 * RELATIVE_PRECISION
}

/// The trade price in whole tokens: how much `token_out` one `token_in` bought.
fn trade_price(
    amount_in: &BigUint,
    amount_out: &BigUint,
    token_in: &Token,
    token_out: &Token,
) -> f64 {
    if amount_in.is_zero() {
        return f64::MAX;
    }
    let decimal_diff =
        i32::try_from(i64::from(token_in.decimals) - i64::from(token_out.decimals)).unwrap_or(0);
    (to_f64(amount_out) / to_f64(amount_in)) * 10_f64.powi(decimal_diff)
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
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

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

    fn price_at(
        state: &UniswapV2State,
        amount: &BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> f64 {
        let amount_out = state
            .get_amount_out(amount.clone(), token_in, token_out)
            .expect("the pool swaps the amount")
            .amount;
        trade_price(amount, &amount_out, token_in, token_out)
    }

    /// Searches the depth at `target_price` and checks that it is the depth: its trade price
    /// meets the target, and 2.5e-5 more input misses it.
    fn assert_search_finds_depth(
        state: &UniswapV2State,
        target_price: f64,
        token_in: &Token,
        token_out: &Token,
    ) {
        let spot_price = state
            .spot_price(token_in, token_out)
            .expect("the pool has a spot price");

        let depth = search_depth(state, spot_price, target_price, token_in, token_out)
            .expect("the search finds a depth");

        assert!(price_at(state, &depth, token_in, token_out) >= target_price);
        let past_depth = &depth + &depth / BigUint::from(40_000u32);
        assert!(price_at(state, &past_depth, token_in, token_out) < target_price);
    }

    #[rstest]
    #[case::same_decimals(18, 18, 1_000, 2_000)]
    #[case::high_to_low(18, 6, 5_000, 10_000_000)]
    #[case::low_to_high(6, 18, 10_000_000, 5_000)]
    #[case::small_difference(8, 18, 333, 5_000)]
    fn test_search_depth_at_one_percent(
        #[case] decimals_in: u32,
        #[case] decimals_out: u32,
        #[case] reserve_in: u64,
        #[case] reserve_out: u64,
    ) {
        let (state, token_in, token_out) =
            univ2(reserve_in, reserve_out, decimals_in, decimals_out);
        let spot_price = state
            .spot_price(&token_in, &token_out)
            .expect("the pool has a spot price");

        assert_search_finds_depth(&state, spot_price * 0.99, &token_in, &token_out);
    }

    #[test]
    fn test_search_depth_far_under_the_input_limit() {
        // The fee takes the trade price 0.6% under the spot price at once, so a target just under
        // that leaves a depth many times smaller than the input limit.
        let (state, token_in, token_out) = univ2(1_000_000, 2_000_000, 18, 18);
        let spot_price = state
            .spot_price(&token_in, &token_out)
            .expect("the pool has a spot price");
        let target_price = spot_price * 0.997 * 0.997 * (1.0 - 1e-5);
        let (max_in, _) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("the pool has limits");

        let depth = search_depth(&state, spot_price, target_price, &token_in, &token_out)
            .expect("the search finds a depth");

        assert!(depth < &max_in / BRACKET_STEP);
        assert_search_finds_depth(&state, target_price, &token_in, &token_out);
    }

    #[test]
    fn test_search_depth_target_above_spot() {
        let (state, token_in, token_out) = univ2(1_000, 2_000, 18, 18);
        let spot_price = state
            .spot_price(&token_in, &token_out)
            .expect("the pool has a spot price");

        let result = search_depth(&state, spot_price, spot_price * 1.01, &token_in, &token_out);

        assert!(matches!(result, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn test_search_depth_target_under_the_limit_price() {
        // The whole input limit trades above a target this low, so the pool has no depth there.
        let (state, token_in, token_out) = univ2(1_000, 2_000, 18, 18);
        let spot_price = state
            .spot_price(&token_in, &token_out)
            .expect("the pool has a spot price");

        let result = search_depth(&state, spot_price, spot_price * 1e-9, &token_in, &token_out);

        assert!(matches!(result, Err(SimulationError::InvalidInput(..))));
    }
}
