//! Tests for the Frank-Wolfe split search.

use super::*;
use crate::algorithm::{
    decomposition::components::{Hop, PoolRef, SellLimitKind, SequentialRoute},
    test_utils::{token, ConstantProductSim},
};

/// `10^18`, the on-chain unit of the 18-decimal test tokens.
fn unit() -> BigUint {
    BigUint::from(10u8).pow(18)
}

fn whole(amount: u64) -> BigUint {
    BigUint::from(amount) * unit()
}

fn pool(id: &str, reserve_a: u64, reserve_b: u64) -> PoolRef {
    PoolRef::new(
        id.to_string(),
        SellLimitKind::Enforced,
        Box::new(ConstantProductSim {
            reserve_0: whole(reserve_a),
            reserve_1: whole(reserve_b),
            gas: 100_000,
        }),
        None,
    )
}

/// A one-hop A -> B route over a single pool, with the hop's split already set.
fn route(id: &str, reserve_a: u64, reserve_b: u64) -> SequentialRoute {
    let (token_a, token_b) = (token(0x0A, "A"), token(0x0B, "B"));
    let mut hop = Hop::new(token_a.clone(), token_b.clone(), vec![pool(id, reserve_a, reserve_b)])
        .expect("hop has a pool");
    hop.set_splits(vec![Fraction::one()])
        .expect("one split for one pool");
    SequentialRoute::new(vec![token_a, token_b], vec![hop]).expect("route matches its path")
}

/// Gas priced at zero, so the tests compare output alone.
fn gas_prices() -> (BigUint, ()) {
    (BigUint::zero(), ())
}

fn split_values(solution: &SplitSolution) -> Vec<f64> {
    solution
        .splits
        .iter()
        .map(Fraction::to_f64)
        .collect()
}

#[test]
fn test_two_equal_alternatives_split_evenly() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes = vec![route("a", 1_000, 1_000), route("b", 1_000, 1_000)];

    let solution =
        split_by_frank_wolfe(&mut routes, &whole(100), &prices).expect("the search succeeds");

    let splits = split_values(&solution);
    assert!(
        (splits[0] - 0.5).abs() < 0.05 && (splits[1] - 0.5).abs() < 0.05,
        "two identical pools should share the flow, got {splits:?}"
    );
}

#[test]
fn test_the_deeper_alternative_takes_more() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes = vec![route("shallow", 100, 100), route("deep", 10_000, 10_000)];

    let solution =
        split_by_frank_wolfe(&mut routes, &whole(50), &prices).expect("the search succeeds");

    let splits = split_values(&solution);
    assert!(splits[1] > splits[0], "the deeper pool should carry more, got {splits:?}");
}

#[test]
fn test_a_small_alternative_still_gets_a_share() {
    // The case the fold chain gets wrong: an alternative that can only absorb a few percent. The
    // pairwise first pass offers it 100%, 50%, then 0% and settles on zero.
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes = vec![route("deep", 100_000, 100_000), route("small", 200, 200)];

    let solution =
        split_by_frank_wolfe(&mut routes, &whole(1_000), &prices).expect("the search succeeds");

    let splits = split_values(&solution);
    assert!(splits[1] > 0.0, "the small pool should keep a share, got {splits:?}");
    assert!(splits[1] < splits[0], "but less than the deep one, got {splits:?}");
}

#[test]
fn test_a_worthless_alternative_is_left_out() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    // The second pool prices A at a hundredth of the first, so moving flow into it always loses.
    let mut routes = vec![route("good", 10_000, 10_000), route("bad", 10_000, 100)];

    let solution =
        split_by_frank_wolfe(&mut routes, &whole(100), &prices).expect("the search succeeds");

    let splits = split_values(&solution);
    assert!(splits[0] > 0.9, "the good pool should keep the flow, got {splits:?}");
}

#[test]
fn test_one_alternative_takes_everything() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes = vec![route("only", 10_000, 10_000)];

    let solution =
        split_by_frank_wolfe(&mut routes, &whole(100), &prices).expect("the search succeeds");

    assert_eq!(solution.splits, vec![Fraction::one()]);
    assert_eq!(solution.sold, whole(100));
}

#[test]
fn test_no_alternatives_is_an_empty_solution() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes: Vec<SequentialRoute> = Vec::new();

    let solution = split_by_frank_wolfe(&mut routes, &whole(100), &prices)
        .expect("an empty set is not an error");

    assert!(solution.splits.is_empty());
    assert!(solution.sold.is_zero());
}

#[test]
fn test_a_zero_sell_amount_is_rejected() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes = vec![route("a", 1_000, 1_000), route("b", 1_000, 1_000)];

    let result = split_by_frank_wolfe(&mut routes, &BigUint::zero(), &prices);

    assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
}

#[test]
fn test_the_splits_match_what_was_sold() {
    let (wei, _) = gas_prices();
    let prices = TokenPriceData::new(wei.clone(), None);
    let mut routes = vec![route("a", 5_000, 5_000), route("b", 2_000, 2_000)];
    let amount = whole(300);

    let solution =
        split_by_frank_wolfe(&mut routes, &amount, &prices).expect("the search succeeds");

    let sold: BigUint = routes
        .iter()
        .map(Sellable::sell_amount)
        .fold(BigUint::zero(), |total, route| total + route);
    assert_eq!(solution.sold, sold, "the reported sold amount is what the routes hold");
    for (route, split) in routes.iter().zip(&solution.splits) {
        assert_eq!(split, &split_of(route.sell_amount(), &amount));
    }
}
