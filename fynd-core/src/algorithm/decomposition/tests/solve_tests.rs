//! Tests for the decomposition solving core.
//!
//! The recovery paths of `recursive_solve_splits` and the post-processing of `_solve` are the
//! parts with no defibot test coverage worth porting, so the cases here are built from the
//! behaviour the Python states rather than from its test suite. Where a defibot test does exist it
//! is named.

use std::sync::Arc;

use num_rational::BigRational;
use rustc_hash::FxHashMap;

use super::*;
use crate::algorithm::{
    decomposition::{
        components::{PoolRef, SellLimitKind},
        optimizers::pair_comparison::PairComparison,
        test_fixtures::{
            graph as build_graph, hop as build_hop, pool, route as build_route, single_pool_hop,
            split, tenfold_pool, token_a, token_b, token_c, token_d, FixedRateSim,
        },
    },
    test_utils::ConstantProductSim,
};

/// Gas priced at zero, so every test ranks on gross output unless it says otherwise.
/// Gas priced at zero, so the fixtures compare output alone.
fn free_gas() -> BigUint {
    BigUint::zero()
}

fn solve(graph: &mut DecompositionGraph, sell_amount: &BigUint) {
    solve_graph(
        graph,
        sell_amount,
        &PairComparison,
        &PairComparison,
        &GasPrices::new(free_gas(), None),
    )
    .expect("the fixtures solve");
}

fn solve_branch(route: &mut SequentialRoute, sell_amount: u64) {
    solve_route(
        route,
        &BigUint::from(sell_amount),
        &PairComparison,
        &GasPrices::new(free_gas(), None),
    )
    .expect("the fixtures solve");
}

fn solve_all(graph: &mut DecompositionGraph, sell_amount: &BigUint) -> (BigUint, BigUint) {
    solve_solution_graph(
        graph,
        sell_amount,
        &PairComparison,
        &PairComparison,
        &GasPrices::new(free_gas(), None),
    )
    .expect("the fixtures solve")
}

/// A constant-product pool over 18-decimal tokens.
fn cp_pool(id: &str, reserve_0: u64, reserve_1: u64) -> PoolRef {
    let unit = BigUint::from(10u8).pow(18);
    PoolRef::new(
        id.to_string(),
        SellLimitKind::Enforced,
        Box::new(ConstantProductSim {
            reserve_0: BigUint::from(reserve_0) * &unit,
            reserve_1: BigUint::from(reserve_1) * unit,
            gas: 50_000,
        }),
        None,
    )
}

/// `amount` whole tokens in 18-decimal on-chain units.
fn whole(amount: u64) -> BigUint {
    BigUint::from(amount) * BigUint::from(10u8).pow(18)
}

// ===================== The 80% clamp (order_solver.py:589-593) =====================

#[test]
fn test_solve_route_clamps_the_sell_amount_to_four_fifths_of_the_limit() {
    // The pool will take 100 units. Asking for 1000 must settle on 80, not on 100: the solver never
    // exhausts a route.
    let mut route = build_route(
        vec![token_a(), token_b()],
        vec![build_hop(
            token_a(),
            token_b(),
            vec![pool("capped", FixedRateSim::new(10).with_sell_limit(100))],
        )],
    );

    solve_branch(&mut route, 1_000);

    assert_eq!(route.sell_amount(), &BigUint::from(80u8));
    assert_eq!(route.buy_amount(), &BigUint::from(800u32));
}

#[test]
fn test_solve_graph_clamps_the_sell_amount_to_four_fifths_of_the_limit() {
    // The same rule one level up: the graph's limit is the sum over its branches.
    let branch = |id: &str| {
        build_route(
            vec![token_a(), token_b()],
            vec![build_hop(
                token_a(),
                token_b(),
                vec![pool(id, FixedRateSim::new(10).with_sell_limit(100))],
            )],
        )
    };
    let mut graph = build_graph(vec![branch("left"), branch("right")], Vec::new());

    solve(&mut graph, &BigUint::from(10_000u32));

    // 200 units of capacity, clamped to 160 and then shared by the two identical branches.
    assert_eq!(graph.sell_amount(), &BigUint::from(160u8));
}

#[test]
fn test_clamp_leaves_amounts_under_the_limit_alone() {
    let limit = BigUint::from(100u8);

    assert_eq!(clamp_to_limit(&BigUint::from(99u8), &limit), BigUint::from(99u8));
    assert_eq!(clamp_to_limit(&limit, &limit), limit);
    assert_eq!(clamp_to_limit(&BigUint::from(101u8), &limit), BigUint::from(80u8));
}

// ===================== Halving restart (order_solver.py:619-625) =====================

#[test]
fn test_solve_route_halves_the_sell_amount_after_a_simulation_failure() {
    // The second hop reports plenty of capacity but its math fails above 2500 units of the
    // intermediate token. The first hop multiplies by ten, so 1000 -> 500 -> 250 is the first size
    // whose output the second hop can actually evaluate.
    let mut route = build_route(
        vec![token_a(), token_b(), token_c()],
        vec![
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            single_pool_hop(
                token_b(),
                token_c(),
                pool(
                    "bc",
                    FixedRateSim::new(1)
                        .with_spot_price(1.0)
                        .with_simulation_failure_above(2_500),
                ),
            ),
        ],
    );

    solve_branch(&mut route, 1_000);

    assert_eq!(route.sell_amount(), &BigUint::from(250u32));
    assert_eq!(route.buy_amount(), &BigUint::from(2_500u32));
}

// ===================== Limit restart (order_solver.py:626-647) =====================

#[test]
fn test_solve_route_restarts_below_a_limit_hit_at_an_intermediate_token() {
    // The first hop's two pools quote wildly different spot prices while both paying 100x, so the
    // route's own limit — computed from the unsolved mean price — badly overstates what the second
    // hop will take. The second hop then refuses, and the restart casts its limit back through the
    // now-solved first hop to land on a size that fits.
    let mut route = build_route(
        vec![token_a(), token_b(), token_c()],
        vec![
            build_hop(
                token_a(),
                token_b(),
                vec![
                    pool("fast", FixedRateSim::new(100).with_spot_price(100.0)),
                    pool("slow", FixedRateSim::new(100).with_spot_price(1.0)),
                ],
            ),
            single_pool_hop(
                token_b(),
                token_c(),
                pool("bc", FixedRateSim::new(1).with_sell_limit(1_000)),
            ),
        ],
    );

    solve_branch(&mut route, 1_000);

    // 999 units of the intermediate token cast back through the solved first hop's price of 100.
    assert_eq!(route.sell_amount(), &BigUint::from(9u8));
    assert_eq!(route.buy_amount(), &BigUint::from(900u32));
    assert!(route.hops()[1].sell_amount() <= &BigUint::from(1_000u32));
}

#[test]
fn test_limit_restart_amount_uses_the_limit_directly_at_the_first_hop() {
    // The limit is already denominated in the route's sell token, so there is nothing to cast.
    let route = build_route(
        vec![token_a(), token_b(), token_c()],
        vec![
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            single_pool_hop(token_b(), token_c(), tenfold_pool("bc")),
        ],
    );

    let restart = limit_restart_amount(
        &route,
        0,
        &token_a().address,
        &BigUint::from(500u32),
        &BigUint::from(10u8),
    )
    .expect("no cast needed");

    assert_eq!(restart, BigUint::from(499u32));
}

#[test]
fn test_limit_restart_amount_caps_an_optimistic_cast_at_one_below_the_attempt() {
    // Casting 999 units of the intermediate token back through a price of ten gives 99, which is
    // above the 20 that just failed. Without the cap the branch would retry at a larger size.
    let route = build_route(
        vec![token_a(), token_b(), token_c()],
        vec![
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            single_pool_hop(token_b(), token_c(), tenfold_pool("bc")),
        ],
    );

    let restart = limit_restart_amount(
        &route,
        1,
        &token_b().address,
        &BigUint::from(1_000u32),
        &BigUint::from(20u8),
    )
    .expect("the cast succeeds");

    assert_eq!(restart, BigUint::from(19u8));
}

#[test]
fn test_limit_restart_amount_floors_a_zero_limit_at_zero() {
    let route = build_route(
        vec![token_a(), token_b()],
        vec![single_pool_hop(token_a(), token_b(), tenfold_pool("ab"))],
    );

    let restart =
        limit_restart_amount(&route, 0, &token_a().address, &BigUint::zero(), &BigUint::from(5u8))
            .expect("no cast needed");

    assert!(restart.is_zero());
}

// ===================== Loop removal (order_solver.py:855-894) =====================

/// defibot's `test_handle_route_loops` (`test_decomposition_solver.py:1096-1137`), with the
/// four-token diamond spelled in the fixtures' tokens: `a -> b -> c -> d` against
/// `a -> c -> b -> d`. The second branch traverses `c -> b`, the reverse of a pair the first
/// branch already claimed.
fn looping_graph(reversed_first: bool) -> DecompositionGraph {
    let forward = build_route(
        vec![token_a(), token_b(), token_c(), token_d()],
        vec![
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            single_pool_hop(token_b(), token_c(), tenfold_pool("bc")),
            single_pool_hop(token_c(), token_d(), tenfold_pool("cd")),
        ],
    );
    let backward = build_route(
        vec![token_a(), token_c(), token_b(), token_d()],
        vec![
            single_pool_hop(token_a(), token_c(), tenfold_pool("ac")),
            single_pool_hop(token_c(), token_b(), tenfold_pool("cb")),
            single_pool_hop(token_b(), token_d(), tenfold_pool("bd")),
        ],
    );

    let branches = if reversed_first { vec![backward, forward] } else { vec![forward, backward] };
    let mut graph = build_graph(branches, vec![split(1, 2); 2]);
    graph
        .sell(&BigUint::from(100u8))
        .expect("the fixture pools are unbounded");
    graph
}

#[test]
fn test_remove_loops_drops_the_branch_that_reverses_a_claimed_direction() {
    let mut graph = looping_graph(false);

    let removed = remove_loops(&mut graph).expect("one branch survives");

    assert!(removed);
    assert_eq!(graph.branches().len(), 1);
    assert_eq!(
        graph.branches()[0]
            .hop()
            .token_out()
            .address,
        token_b().address
    );
    // The graph is unsolved again, which is what forces the caller to re-solve.
    assert!(graph.outer_splits().is_empty());
}

#[test]
fn test_remove_loops_keeps_whichever_branch_claims_a_direction_first() {
    // The registry is never reset between branches, so branch order alone decides the survivor.
    let mut graph = looping_graph(true);

    let removed = remove_loops(&mut graph).expect("one branch survives");

    assert!(removed);
    assert_eq!(graph.branches().len(), 1);
    assert_eq!(
        graph.branches()[0]
            .hop()
            .token_out()
            .address,
        token_c().address
    );
}

#[test]
fn test_remove_loops_ignores_hops_that_did_not_trade() {
    // defibot reads directions off executed swaps, so a graph nothing has been sold on has no
    // directions to conflict.
    let forward = build_route(
        vec![token_a(), token_b(), token_c(), token_d()],
        vec![
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            single_pool_hop(token_b(), token_c(), tenfold_pool("bc")),
            single_pool_hop(token_c(), token_d(), tenfold_pool("cd")),
        ],
    );
    let backward = build_route(
        vec![token_a(), token_c(), token_b(), token_d()],
        vec![
            single_pool_hop(token_a(), token_c(), tenfold_pool("ac")),
            single_pool_hop(token_c(), token_b(), tenfold_pool("cb")),
            single_pool_hop(token_b(), token_d(), tenfold_pool("bd")),
        ],
    );
    let mut graph = build_graph(vec![forward, backward], vec![split(1, 2); 2]);

    assert!(!remove_loops(&mut graph).expect("nothing to remove"));
    assert_eq!(graph.branches().len(), 2);
}

#[test]
fn test_remove_loops_refuses_to_empty_the_graph() {
    // A single branch that doubles back on itself would be removed, leaving nothing behind.
    let mut route = build_route(
        vec![token_a(), token_b(), token_a(), token_b()],
        vec![
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            single_pool_hop(token_b(), token_a(), tenfold_pool("ba")),
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab2")),
        ],
    );
    route
        .sell(&BigUint::from(10u8))
        .expect("the fixture pools are unbounded");
    let mut graph = build_graph(vec![route], vec![Fraction::one()]);

    let error = remove_loops(&mut graph).expect_err("nothing would be left");

    assert!(matches!(error, DecompositionError::InvalidStructure { .. }));
}

#[test]
fn test_solve_solution_graph_resolves_after_removing_a_loop() {
    // Constant-product pools, so the split search puts volume on *both* branches and the reversed
    // one really does traverse its pair. A branch the search left at zero trades nothing, claims no
    // direction, and would not be removed at all.
    let forward = build_route(
        vec![token_a(), token_b(), token_c(), token_d()],
        vec![
            single_pool_hop(token_a(), token_b(), cp_pool("ab", 1_000_000, 1_000_000)),
            single_pool_hop(token_b(), token_c(), cp_pool("bc", 1_000_000, 1_000_000)),
            single_pool_hop(token_c(), token_d(), cp_pool("cd", 1_000_000, 1_000_000)),
        ],
    );
    let backward = build_route(
        vec![token_a(), token_c(), token_b(), token_d()],
        vec![
            single_pool_hop(token_a(), token_c(), cp_pool("ac", 1_000_000, 1_000_000)),
            single_pool_hop(token_c(), token_b(), cp_pool("cb", 1_000_000, 1_000_000)),
            single_pool_hop(token_b(), token_d(), cp_pool("bd", 1_000_000, 1_000_000)),
        ],
    );
    let mut graph = build_graph(vec![forward, backward], Vec::new());

    let (bought, _) = solve_all(&mut graph, &whole(1_000));

    assert_eq!(graph.branches().len(), 1);
    assert_eq!(graph.outer_splits(), &[Fraction::one()]);
    assert!(!bought.is_zero());
}

// ===================== The second pass (order_solver.py:285-296) =====================

/// A branch whose hop holds an impact-free but capped pool and a shallow fallback pool.
///
/// How the two share the hop depends on the size the branch is asked for, which is what makes the
/// second pass observable: solving the branch for the whole order and solving it for the share it
/// actually receives give different inner splits.
fn capped_branch() -> SequentialRoute {
    build_route(
        vec![token_a(), token_b()],
        vec![build_hop(
            token_a(),
            token_b(),
            vec![
                pool("capped", FixedRateSim::new(1).with_sell_limit(4_000_000_000_000_000_000)),
                cp_pool("shallow", 100_000, 100_000),
            ],
        )],
    )
}

/// A branch over one deep pool, which is where the order goes once the capped pool is full.
fn deep_branch() -> SequentialRoute {
    build_route(
        vec![token_a(), token_b()],
        vec![single_pool_hop(token_a(), token_b(), cp_pool("deep", 10_000_000, 10_000_000))],
    )
}

#[test]
fn test_second_pass_resolves_each_branch_for_the_amount_it_receives() {
    let mut first_pass_only = build_graph(vec![capped_branch(), deep_branch()], Vec::new());
    let mut full = build_graph(vec![capped_branch(), deep_branch()], Vec::new());

    solve(&mut first_pass_only, &whole(10));
    solve_all(&mut full, &whole(10));

    let optimistic = first_pass_only.branches()[0]
        .hop_at(0)
        .splits()[0]
        .clone();
    let allocated = full.branches()[0].hop_at(0).splits()[0].clone();
    assert!(
        allocated > optimistic,
        "a branch solved for its share of the order should lean harder on the impact-free pool: \
         {allocated:?} vs {optimistic:?}"
    );
    // Re-solving is not cosmetic: the branch buys more for the same input than it would have.
    assert!(full.branches()[0].buy_amount() > first_pass_only.branches()[0].buy_amount());
    assert_eq!(full.branches()[0].sell_amount(), first_pass_only.branches()[0].sell_amount());
}

#[test]
fn test_second_pass_leaves_zero_split_branches_alone() {
    // A branch the outer search discarded is never re-solved, so it keeps the zero it was given.
    let strong = build_route(
        vec![token_a(), token_b()],
        vec![single_pool_hop(token_a(), token_b(), pool("strong", FixedRateSim::new(10)))],
    );
    let weak = build_route(
        vec![token_a(), token_b()],
        vec![single_pool_hop(token_a(), token_b(), pool("weak", FixedRateSim::new(1)))],
    );
    let mut graph = build_graph(vec![strong, weak], Vec::new());

    solve_all(&mut graph, &BigUint::from(1_000u32));

    assert!(graph.outer_splits()[1].is_zero());
    assert!(graph.branches()[1]
        .sell_amount()
        .is_zero());
}

// ===================== Coupled paths (utils.py:18-47) =====================

/// Two branches over the *same* pools, the shape of defibot's `test_sell_with_coupled_paths`.
fn coupled_graph() -> DecompositionGraph {
    let branch = || {
        build_route(
            vec![token_a(), token_b(), token_c()],
            vec![
                single_pool_hop(token_a(), token_b(), cp_pool("ab", 1_000_000, 1_000_000)),
                single_pool_hop(token_b(), token_c(), cp_pool("bc", 1_000_000, 1_000_000)),
            ],
        )
    };
    build_graph(vec![branch(), branch()], vec![split(1, 2); 2])
}

#[test]
fn test_sell_with_coupled_paths_restores_the_pre_trade_states() {
    let mut graph = coupled_graph();
    let before: Vec<Box<dyn ProtocolSim>> = graph_pools(&graph)
        .into_iter()
        .map(|(_, state)| state.clone_box())
        .collect();

    sell_with_coupled_paths(&mut graph, &whole(1_000)).expect("the branches sell");

    let after = graph_pools(&graph);
    assert_eq!(before.len(), after.len());
    for (original, (_, current)) in before.iter().zip(after) {
        assert!(ProtocolSim::eq(current, original.as_ref()));
    }
}

#[test]
fn test_sell_with_coupled_paths_reverts_even_when_a_branch_fails() {
    // The revert lives in a `finally`. A branch whose hop was left unsolved fails on the way out
    // and the states still have to come back.
    let mut graph = coupled_graph();
    graph.branches_mut()[1]
        .hop_at_mut(0)
        .set_splits(Vec::new())
        .expect("clearing is always valid");
    let before: Vec<Box<dyn ProtocolSim>> = graph_pools(&graph)
        .into_iter()
        .map(|(_, state)| state.clone_box())
        .collect();

    let error = sell_with_coupled_paths(&mut graph, &whole(1_000))
        .expect_err("an unsolved hop cannot sell");

    assert!(matches!(error, DecompositionError::Unsolved { .. }));
    for (original, (_, current)) in before.iter().zip(graph_pools(&graph)) {
        assert!(ProtocolSim::eq(current, original.as_ref()));
    }
}

#[test]
fn test_sell_with_coupled_paths_updates_only_the_branches_not_yet_sold() {
    let mut graph = coupled_graph();

    sell_with_coupled_paths(&mut graph, &whole(1_000)).expect("the branches sell");

    // Both branches sold the same amount into the same pools, but the second traded against the
    // liquidity the first had already taken.
    assert_eq!(graph.branches()[0].sell_amount(), graph.branches()[1].sell_amount());
    assert!(graph.branches()[1].buy_amount() < graph.branches()[0].buy_amount());
}

// ===================== _solve_without_splits (order_solver.py:810-853) =====================

/// A one-hop branch whose single pool pays `multiple` and stops at `sell_limit`.
fn capacity_branch(id: &str, multiple: u64, sell_limit: u64) -> SequentialRoute {
    build_route(
        vec![token_a(), token_b()],
        vec![single_pool_hop(
            token_a(),
            token_b(),
            pool(id, FixedRateSim::new(multiple).with_sell_limit(sell_limit)),
        )],
    )
}

fn without_splits(graph: &mut DecompositionGraph, sell_amount: u64) {
    solve_without_splits(graph, &BigUint::from(sell_amount), &GasPrices::new(free_gas(), None))
        .expect("the fixtures sell");
}

#[test]
fn test_solve_without_splits_fills_the_best_branches_first() {
    // Ranked on what each branch buys, the tenfold branch goes first. Each branch can only take 90
    // — `decrease_until_sell` backs a refused size off by 10% rather than settling on the limit
    // itself — so the last branch absorbs only what is left of the 250.
    let mut graph = build_graph(
        vec![
            capacity_branch("poor", 1, 100),
            capacity_branch("rich", 10, 100),
            capacity_branch("fair", 5, 100),
        ],
        Vec::new(),
    );

    without_splits(&mut graph, 250);

    assert_eq!(graph.branches()[1].sell_amount(), &BigUint::from(90u8));
    assert_eq!(graph.branches()[2].sell_amount(), &BigUint::from(90u8));
    assert_eq!(graph.branches()[0].sell_amount(), &BigUint::from(70u8));
    assert_eq!(graph.sell_amount(), &BigUint::from(250u32));
}

#[test]
fn test_solve_without_splits_skips_a_branch_reusing_an_included_pool() {
    let mut graph = build_graph(
        vec![capacity_branch("shared", 10, 100), capacity_branch("shared", 10, 100)],
        Vec::new(),
    );

    without_splits(&mut graph, 250);

    assert_eq!(graph.branches()[0].sell_amount(), &BigUint::from(90u8));
    assert!(graph.branches()[1]
        .sell_amount()
        .is_zero());
    assert!(graph.outer_splits()[1].is_zero());
}

#[test]
fn test_solve_without_splits_leaves_a_shortfall_when_liquidity_runs_out() {
    // Two branches of 90 usable units each against an order of 1000: the splits must add up to
    // less than one rather than being normalised into a promise the market cannot keep.
    let mut graph = build_graph(
        vec![capacity_branch("left", 10, 100), capacity_branch("right", 10, 100)],
        Vec::new(),
    );

    without_splits(&mut graph, 1_000);

    let total = graph
        .outer_splits()
        .iter()
        .fold(BigRational::zero(), |sum, split| sum + split.as_ratio());
    assert_eq!(total, BigRational::new(180.into(), 1_000.into()));
    assert_eq!(graph.buy_amount(), &BigUint::from(1_800u32));
}

#[test]
fn test_solve_without_splits_zeroes_branches_past_the_exhaustion_point() {
    // Deviation from defibot (`order_solver.py:833-834`), which breaks out of the loop and then
    // builds the splits from sizes those branches never received. Here the whole order fits in the
    // first branch, so the other two must end on zero and the splits must sum to exactly one.
    let mut graph = build_graph(
        vec![
            capacity_branch("rich", 10, 10_000),
            capacity_branch("fair", 5, 10_000),
            capacity_branch("poor", 1, 10_000),
        ],
        Vec::new(),
    );

    without_splits(&mut graph, 1_000);

    assert_eq!(graph.branches()[0].sell_amount(), &BigUint::from(1_000u32));
    assert!(graph.branches()[1]
        .sell_amount()
        .is_zero());
    assert!(graph.branches()[2]
        .sell_amount()
        .is_zero());
    let total = graph
        .outer_splits()
        .iter()
        .fold(BigRational::zero(), |sum, split| sum + split.as_ratio());
    assert_eq!(total, BigRational::one());
    assert_eq!(graph.sell_amount(), &BigUint::from(1_000u32));
}

// ===================== The final comparison (order_solver.py:300-310) =====================

/// A solved one-branch graph that has already sold `sell_amount`.
fn sold_graph(id: &str, multiple: u64, sell_amount: u64) -> DecompositionGraph {
    let mut graph = build_graph(
        vec![build_route(
            vec![token_a(), token_b()],
            vec![single_pool_hop(token_a(), token_b(), pool(id, FixedRateSim::new(multiple)))],
        )],
        vec![Fraction::one()],
    );
    graph
        .sell(&BigUint::from(sell_amount))
        .expect("the fixture pools are unbounded");
    graph
}

#[test]
fn test_choose_solution_returns_the_reference_when_the_candidate_buys_less() {
    let candidate = sold_graph("candidate", 5, 100);
    let reference = sold_graph("reference", 10, 100);

    let choice = choose_solution(&candidate, Some(&reference), &GasPrices::new(free_gas(), None));

    assert_eq!(choice, SolutionChoice::Reference);
}

#[test]
fn test_choose_solution_keeps_the_candidate_when_it_ties() {
    let candidate = sold_graph("candidate", 10, 100);
    let reference = sold_graph("reference", 10, 100);

    let choice = choose_solution(&candidate, Some(&reference), &GasPrices::new(free_gas(), None));

    assert_eq!(choice, SolutionChoice::Candidate);
}

#[test]
fn test_choose_solution_without_a_reference_keeps_the_candidate() {
    let candidate = sold_graph("candidate", 1, 100);

    assert_eq!(
        choose_solution(&candidate, None, &GasPrices::new(free_gas(), None)),
        SolutionChoice::Candidate
    );
}

#[test]
fn test_net_of_gas_charges_the_graph_gas() {
    use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

    let graph = sold_graph("candidate", 10, 100);
    let mut token_prices = FxHashMap::default();
    token_prices.insert(token_b().address, Price::new(BigUint::from(1u8), BigUint::from(1u8)));
    let wei = BigUint::from(100u8);

    // One unit of gas at 100 wei, priced one-for-one into the buy token, against 1000 bought.
    let net =
        net_of_gas(&graph, &GasPrices::new(wei.clone(), Some(Arc::new(token_prices.clone()))));

    assert_eq!(net, BigInt::from(900));
}

// ===================== Rounding =====================

#[test]
fn test_round_to_nearest_breaks_ties_towards_even() {
    // 2.5 rounds down to 2 and 3.5 rounds up to 4, the way Python's `round` does.
    let half = BigRational::new(1.into(), 2.into());

    assert_eq!(round_to_nearest(&half, &BigUint::from(5u8)), BigUint::from(2u8));
    assert_eq!(round_to_nearest(&half, &BigUint::from(7u8)), BigUint::from(4u8));
    assert_eq!(round_to_nearest(&half, &BigUint::from(4u8)), BigUint::from(2u8));
}

#[test]
fn test_round_to_nearest_rounds_up_past_the_halfway_point() {
    let two_thirds = BigRational::new(2.into(), 3.into());

    assert_eq!(round_to_nearest(&two_thirds, &BigUint::from(5u8)), BigUint::from(3u8));
}
