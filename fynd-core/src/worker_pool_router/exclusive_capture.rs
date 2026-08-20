//! Solves how much an exclusive leg must withhold for a route to land on its committed amount.
//!
//! An exclusive leg captures surplus by emitting less than it could. When the leg is the last hop
//! of its path, withholding one unit costs the user exactly one unit, so the amount to withhold is
//! the surplus itself. When swaps sit between the leg and the route's output token, the withheld
//! amount passes through those pools first: withholding `d` at the leg lowers the route output by
//! `g(x) - g(x - d)`, where `g` is the composition of the downstream pools. Capturing a surplus `s`
//! means solving `g(x) - g(x - d) = s` for `d`.
//!
//! There is no closed form for `g` across mixed protocols, so this module inverts it numerically.
//! Every swap carries the pool state it was quoted against ([`Swap::protocol_state`]), so a
//! candidate route can be re-simulated without reading the market again.
//!
//! `g` is non-decreasing for every protocol we route through, which is what makes the search
//! sound: the route output falls monotonically as the leg withholds more. The search keeps the
//! best withholding that still leaves the user at or above the target, so stopping early costs the
//! protocol capture and never costs the user output.

use num_bigint::BigUint;
use num_traits::{CheckedSub, Zero};
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::{
    models::{token::Token, Address},
    simulation::protocol_sim::ProtocolSim,
};

use crate::{algorithm::sim_guard::GuardedProtocolSim, types::quote::Swap};

/// Bisection steps taken when narrowing the withheld amount.
///
/// Each step is one replay of the route, so this is the simulation work one candidate costs. The
/// search stops early once the interval closes; where it does not, it leaves up to
/// `leg_output / 2^24` of the surplus unwithheld. That residue stays with the user, so spending
/// fewer steps costs the protocol capture rather than correctness.
const MAX_BISECTION_STEPS: u32 = 24;

/// Decimals and address for every token a candidate route touches, taken from the market before
/// solving. Pool simulations need the full token, not just its address.
pub(super) type TokenLookup = FxHashMap<Address, Token>;

/// Why a route's withheld amount could not be solved.
///
/// A candidate that cannot be solved cannot carry a commitment, so `best_exclusive_candidate`
/// drops it rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CaptureError {
    /// A token on the route was absent from the lookup built for it.
    MissingToken(Address),
    /// A pool simulation failed or panicked while replaying the route.
    Simulation(String),
    /// The route produced nothing for its output token, so there is no baseline to reduce.
    EmptyBaseline,
}

/// Returns what the route delivers when nothing is withheld.
///
/// This is the cheap half of the module: one replay, enough to tell whether a candidate can be
/// solved at all. `solve_withheld_amount` does the search.
///
/// # Errors
///
/// Returns [`CaptureError`] when the route cannot be replayed against the states its swaps carry.
pub(super) fn replay_baseline(
    swaps: &[Swap],
    input_token: &Address,
    output_token: &Address,
    tokens: &TokenLookup,
) -> Result<BigUint, CaptureError> {
    let baseline = simulate_with_withholding(swaps, input_token, output_token, None, tokens)?;
    if baseline.is_zero() {
        return Err(CaptureError::EmptyBaseline);
    }
    Ok(baseline)
}

/// Returns how much the leg at `leg_index` must withhold, in its own output token, for the route
/// to deliver `surplus` less than it otherwise would.
///
/// `swaps` is the route in emitted order (topological). `input_token` and `output_token` are the
/// route's ends.
///
/// The answer never exceeds the leg's own output, and never withholds so much that the route falls
/// below `baseline - surplus`. A surplus at or above the baseline takes the leg's whole output; the
/// caller's commitment gates keep that out of reach in practice.
///
/// # Errors
///
/// Returns [`CaptureError`] when the route cannot be replayed against the states its swaps carry.
pub(super) fn solve_withheld_amount(
    swaps: &[Swap],
    input_token: &Address,
    output_token: &Address,
    leg_index: usize,
    surplus: &BigUint,
    tokens: &TokenLookup,
) -> Result<BigUint, CaptureError> {
    let leg_output = swaps
        .get(leg_index)
        .map(Swap::amount_out)
        .cloned()
        .unwrap_or_default();

    let replay = |withheld: &BigUint| {
        simulate_with_withholding(
            swaps,
            input_token,
            output_token,
            Some((leg_index, withheld)),
            tokens,
        )
    };

    let baseline = replay_baseline(swaps, input_token, output_token, tokens)?;
    let Some(target) = baseline.checked_sub(surplus) else {
        return Ok(leg_output);
    };

    // `low` is always an amount whose replay clears `target` and `high` always the first amount
    // past it, so the answer is `low` whenever the loop stops, however early that is.
    let mut low = BigUint::ZERO;
    let mut high = leg_output + 1u32;

    for _ in 0..MAX_BISECTION_STEPS {
        if &high - &low <= BigUint::from(1u32) {
            break;
        }
        let mid = (&low + &high) / 2u32;
        if replay(&mid)? >= target {
            low = mid;
        } else {
            high = mid;
        }
    }

    Ok(low)
}

/// Replays `swaps` against the states they carry and returns what reaches `output_token`.
///
/// `withholding` names a leg and how much less than its simulated output it emits. `None` replays
/// the route untouched.
///
/// Balances are threaded exactly as [`crate::replay::replay_route`] threads them: swaps run in the
/// route's topological order, a positive `split` takes that fraction of the balance its token had
/// when the branch opened, the final swap of a group takes the remainder, and a pool used twice
/// sees its own post-swap state the second time.
///
/// Unlike `replay_route` this reads no market. Each swap's own quote-time state is the point: the
/// question is what the route would have produced at the state it was quoted against, not what it
/// would produce now.
fn simulate_with_withholding(
    swaps: &[Swap],
    input_token: &Address,
    output_token: &Address,
    withholding: Option<(usize, &BigUint)>,
    tokens: &TokenLookup,
) -> Result<BigUint, CaptureError> {
    let total_in: BigUint = swaps
        .iter()
        .filter(|swap| swap.token_in() == input_token)
        .map(Swap::amount_in)
        .sum();

    let mut available: FxHashMap<Address, BigUint> = FxHashMap::default();
    available.insert(input_token.clone(), total_in);
    let mut branch_totals: FxHashMap<Address, BigUint> = FxHashMap::default();
    let mut post_swap: FxHashMap<String, Box<dyn ProtocolSim>> = FxHashMap::default();

    for (index, swap) in swaps.iter().enumerate() {
        let token_in = lookup(tokens, swap.token_in())?;
        let token_out = lookup(tokens, swap.token_out())?;

        let branch_total = branch_totals
            .entry(swap.token_in().clone())
            .or_insert_with(|| {
                available
                    .get(swap.token_in())
                    .cloned()
                    .unwrap_or_default()
            })
            .clone();
        let remaining = available
            .entry(swap.token_in().clone())
            .or_default();
        let amount_in = if *swap.split() > 0.0 {
            let (part, _) =
                crate::algorithm::split_primitives::split_amount(&branch_total, *swap.split());
            part.min(remaining.clone())
        } else {
            remaining.clone()
        };
        *remaining -= &amount_in;

        let sim = post_swap
            .get(swap.component_id())
            .map(|state| state.as_ref())
            .unwrap_or_else(|| swap.protocol_state());
        let result = sim
            .get_amount_out_guarded(amount_in, token_in, token_out)
            .map_err(|e| CaptureError::Simulation(e.to_string()))?;

        let amount_out = match withholding {
            Some((leg_index, withheld)) if index == leg_index => result
                .amount
                .checked_sub(withheld)
                .unwrap_or_default(),
            _ => result.amount.clone(),
        };

        *available
            .entry(swap.token_out().clone())
            .or_default() += amount_out;
        post_swap.insert(swap.component_id().to_string(), result.new_state);
    }

    Ok(available
        .remove(output_token)
        .unwrap_or_default())
}

/// Resolves a route token to the full token the pool simulations need.
fn lookup<'a>(tokens: &'a TokenLookup, address: &Address) -> Result<&'a Token, CaptureError> {
    tokens
        .get(address)
        .ok_or_else(|| CaptureError::MissingToken(address.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::{component, token, ConstantProductSim};

    /// A constant-product pool holding `reserve_in` of the token it takes and `reserve_out` of the
    /// one it gives.
    fn pool(
        token_in: &Token,
        token_out: &Token,
        reserve_in: u64,
        reserve_out: u64,
    ) -> Box<dyn ProtocolSim> {
        let (reserve_0, reserve_1) = if token_in.address < token_out.address {
            (reserve_in, reserve_out)
        } else {
            (reserve_out, reserve_in)
        };
        Box::new(ConstantProductSim {
            reserve_0: BigUint::from(reserve_0),
            reserve_1: BigUint::from(reserve_1),
            gas: 0,
        })
    }

    /// Builds a chain of legs, simulating each hop so every leg carries the amounts a solver
    /// would really have quoted it with.
    fn chain(hops: Vec<(&str, &Token, &Token, Box<dyn ProtocolSim>)>, amount_in: u64) -> Vec<Swap> {
        let mut amount = BigUint::from(amount_in);
        let mut swaps = Vec::new();
        for (id, token_in, token_out, sim) in hops {
            let amount_out = sim
                .get_amount_out(amount.clone(), token_in, token_out)
                .expect("test pool must quote")
                .amount;
            swaps.push(Swap::new(
                id.to_string(),
                "uniswap_v2".to_string(),
                token_in.address.clone(),
                token_out.address.clone(),
                amount.clone(),
                amount_out.clone(),
                BigUint::ZERO,
                component(id, &[token_in.clone(), token_out.clone()]),
                sim,
            ));
            amount = amount_out;
        }
        swaps
    }

    fn lookup_for(tokens: &[&Token]) -> TokenLookup {
        tokens
            .iter()
            .map(|t| (t.address.clone(), (*t).clone()))
            .collect()
    }

    /// The route output with nothing withheld. Asserts the replay agrees with the amounts the
    /// legs carry, so a broken fixture fails here rather than somewhere subtler.
    fn baseline_of(
        swaps: &[Swap],
        input: &Address,
        output: &Address,
        tokens: &TokenLookup,
    ) -> BigUint {
        let replayed = simulate_with_withholding(swaps, input, output, None, tokens)
            .expect("route must replay");
        assert_eq!(
            replayed,
            *swaps
                .last()
                .expect("non-empty route")
                .amount_out(),
            "replay must reproduce the amounts the route carries",
        );
        replayed
    }

    #[test]
    fn test_terminal_leg_withholds_the_surplus_itself() {
        let (a, b) = (token(0x01, "A"), token(0x02, "B"));
        let tokens = lookup_for(&[&a, &b]);
        let swaps = chain(vec![("exclusive", &a, &b, pool(&a, &b, 1_000_000, 1_000_000))], 1_000);
        let out = baseline_of(&swaps, &a.address, &b.address, &tokens);

        let surplus = BigUint::from(50u32);
        let withheld =
            solve_withheld_amount(&swaps, &a.address, &b.address, 0, &surplus, &tokens).unwrap();

        assert_eq!(withheld, surplus, "a leg that ends the route withholds the surplus itself");
        assert!(out > surplus);
    }

    #[test]
    fn test_mid_path_leg_lands_the_route_on_the_target() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let tokens = lookup_for(&[&a, &b, &c]);
        let swaps = chain(
            vec![
                ("exclusive", &a, &b, pool(&a, &b, 1_000_000, 1_000_000)),
                ("downstream", &b, &c, pool(&b, &c, 500_000, 1_000_000)),
            ],
            1_000,
        );
        let baseline = baseline_of(&swaps, &a.address, &c.address, &tokens);

        let surplus = BigUint::from(100u32);
        let withheld =
            solve_withheld_amount(&swaps, &a.address, &c.address, 0, &surplus, &tokens).unwrap();
        let target = &baseline - &surplus;

        let landed = simulate_with_withholding(
            &swaps,
            &a.address,
            &c.address,
            Some((0, &withheld)),
            &tokens,
        )
        .unwrap();
        let one_more = &withheld + 1u32;
        let overshoot = simulate_with_withholding(
            &swaps,
            &a.address,
            &c.address,
            Some((0, &one_more)),
            &tokens,
        )
        .unwrap();

        assert!(landed >= target, "the user keeps at least the committed amount");
        assert!(overshoot < target, "withholding one more would take the user below it");
        assert!(withheld > BigUint::ZERO);
    }

    #[test]
    fn test_solved_amount_exceeds_the_average_price_estimate() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let tokens = lookup_for(&[&a, &b, &c]);
        // A thin downstream pool: the marginal price is well below the average price, which is
        // exactly where scaling the surplus by the average price under-captures.
        let swaps = chain(
            vec![
                ("exclusive", &a, &b, pool(&a, &b, 1_000_000, 1_000_000)),
                ("downstream", &b, &c, pool(&b, &c, 200_000, 200_000)),
            ],
            100_000,
        );
        let baseline = baseline_of(&swaps, &a.address, &c.address, &tokens);

        let surplus = BigUint::from(2_000u32);
        let withheld =
            solve_withheld_amount(&swaps, &a.address, &c.address, 0, &surplus, &tokens).unwrap();
        let average_price_estimate = &surplus * swaps[0].amount_out() / &baseline;

        assert!(
            withheld > average_price_estimate,
            "solved {withheld} must exceed the average-price estimate {average_price_estimate}",
        );
    }

    #[test]
    fn test_surplus_above_the_baseline_takes_the_whole_leg() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let tokens = lookup_for(&[&a, &b, &c]);
        let swaps = chain(
            vec![
                ("exclusive", &a, &b, pool(&a, &b, 1_000_000, 1_000_000)),
                ("downstream", &b, &c, pool(&b, &c, 500_000, 1_000_000)),
            ],
            1_000,
        );
        let baseline = baseline_of(&swaps, &a.address, &c.address, &tokens);

        let withheld =
            solve_withheld_amount(&swaps, &a.address, &c.address, 0, &(&baseline + 1u32), &tokens)
                .unwrap();

        assert_eq!(withheld, *swaps[0].amount_out());
    }

    #[test]
    fn test_missing_token_is_an_error() {
        let (a, b) = (token(0x01, "A"), token(0x02, "B"));
        let swaps = chain(vec![("exclusive", &a, &b, pool(&a, &b, 1_000_000, 1_000_000))], 1_000);
        let tokens = lookup_for(&[&a]);

        let solved =
            solve_withheld_amount(&swaps, &a.address, &b.address, 0, &BigUint::from(1u32), &tokens);

        assert_eq!(solved, Err(CaptureError::MissingToken(b.address.clone())));
    }
}
