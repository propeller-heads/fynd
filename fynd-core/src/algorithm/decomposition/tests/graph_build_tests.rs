//! Tests for the candidate graph build and for the grouping it picks.

use num_bigint::BigUint;
use rustc_hash::FxHashSet;
use tycho_simulation::tycho_core::simulation::protocol_sim::ProtocolSim;

use super::*;
use crate::{
    algorithm::{
        decomposition::{
            components::{Branch, BranchSide, Fraction, Hop, PoolRef, SellLimitKind},
            token_graph::{AllowedTokens, SearchBounds, TokenGraph},
        },
        most_liquid::DepthAndPrice,
        test_utils::{market_read, setup_market_unweighted, token, MockProtocolSim},
    },
    feed::market_data::MarketData,
    graph::{
        petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
        GraphManager,
    },
};

/// `(component_id, token_in, token_out, spot_price)` for a fee-free mock pool.
type PoolSpec<'a> = (&'a str, Token, Token, f64);

fn market_with(
    pools: Vec<PoolSpec<'_>>,
) -> (MarketData, PetgraphStableDiGraphManager<DepthAndPrice>) {
    let pairs: Vec<(Token, Token)> = pools
        .iter()
        .map(|(_, token_in, token_out, _)| (token_in.clone(), token_out.clone()))
        .collect();
    let specs = pools
        .iter()
        .zip(&pairs)
        .map(|((id, _, _, spot_price), (token_in, token_out))| {
            let sim = MockProtocolSim::new(*spot_price)
                .with_fee(0.0)
                .with_tokens(&[token_in.clone(), token_out.clone()]);
            (*id, token_in, token_out, Box::new(sim) as Box<dyn ProtocolSim>)
        })
        .collect();
    let (market, _) = setup_market_unweighted(specs);
    // The shared helper builds an unweighted manager; a solve's graph carries edge weights.
    let mut manager = PetgraphStableDiGraphManager::<DepthAndPrice>::default();
    manager.initialize_graph(
        &market_read(&market)
            .base_market_state()
            .component_topology(),
    );
    (market, manager)
}

/// Build inputs with the caps a solve uses and no price floor.
fn params() -> SubgraphParams {
    SubgraphParams { max_routes: 30, minimum_price: 0.0 }
}

/// Search bounds that only the hop limit constrains.
fn bounds(max_hops: usize) -> SearchBounds {
    SearchBounds { max_hops, max_paths: usize::MAX, deadline: None, connector_tokens: None }
}

/// A graph every token may be routed through.
fn open_graph<'a>(
    graph: &'a StableDiGraph<DepthAndPrice>,
    sell: &'a Token,
    buy: &'a Token,
) -> TokenGraph<'a> {
    TokenGraph::new(
        graph,
        &AllowedTokens {
            connector_tokens: None,
            prices: None,
            endpoints: [&sell.address, &buy.address],
        },
    )
}

/// Token sequence of every branch, in ranked order, rendered as `"A->C->B"`.
fn branch_labels(graph: &DecompositionGraph) -> Vec<String> {
    graph
        .branches()
        .iter()
        .map(Branch::token_path_label)
        .collect()
}

fn pool_ids(branch: &Branch, leg: usize) -> Vec<String> {
    branch
        .hop_at(leg)
        .pools()
        .iter()
        .map(|pool| pool.component_id().clone())
        .collect()
}

/// Searches the graph and builds the candidate subgraph, the way a solve does.
fn build(
    graph: &TokenGraph<'_>,
    market: &MarketData,
    depths: Option<&ComponentDepths>,
    sell: &Token,
    buy: &Token,
    params: &SubgraphParams,
    bounds: &SearchBounds,
) -> Option<DecompositionGraph> {
    let paths = graph.paths_between(&sell.address, &buy.address, bounds);
    let view = market_read(market);
    // `GraphBuildFailure` is "nothing routable here", which several cases assert; anything else is
    // a broken fixture and should stop the test rather than read as an empty result.
    match build_decomposition_graph(view.base_market_state(), depths, params, paths) {
        Ok(solution) => Some(solution),
        Err(DecompositionError::GraphBuildFailure) => None,
        Err(error) => panic!("the fixtures build: {error}"),
    }
}

#[test]
fn test_two_pools_on_one_pair_become_one_branch() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![
        ("ab_cheap", a.clone(), b.clone(), 2.0),
        ("ab_rich", a.clone(), b.clone(), 3.0),
    ]);

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params(), &bounds(2))
            .expect("the fixtures build a route");

    assert_eq!(solution.branches().len(), 1);
    let route = &solution.branches()[0];
    assert_eq!(route.hops().count(), 1);
    // Equal depth, so ranking falls through to price: the richer pool leads.
    assert_eq!(pool_ids(route, 0), ["ab_rich", "ab_cheap"]);
}

#[test]
fn test_distinct_token_sequences_become_distinct_branches() {
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 10.0),
        ("ac", a.clone(), c.clone(), 2.0),
        ("cb", c.clone(), b.clone(), 0.5),
    ]);

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params(), &bounds(2))
            .expect("the fixtures build a route");

    assert_eq!(branch_labels(&solution), ["A->B", "A->C->B"]);
}

#[test]
fn test_pool_reused_across_hops_is_skipped() {
    // `multi` holds A, B and C, so it offers A->B, B->C and A->C. The A->B->C sequence can
    // only be served by `multi` at both legs, so the second leg is left empty and the whole
    // sequence is discarded; the direct A->C branch survives.
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let mut market = crate::feed::market_data::MarketState::new();
    let sim = MockProtocolSim::new(2.0)
        .with_fee(0.0)
        .with_tokens(&[a.clone(), b.clone(), c.clone()]);
    market.upsert_components(std::iter::once(crate::algorithm::test_utils::component(
        "multi",
        &[a.clone(), b.clone(), c.clone()],
    )));
    market.update_states([("multi".to_string(), Box::new(sim) as Box<dyn ProtocolSim>)]);
    market.upsert_tokens(vec![a.clone(), b.clone(), c.clone()]);

    let mut manager = PetgraphStableDiGraphManager::<DepthAndPrice>::default();
    manager.initialize_graph(&market.component_topology());

    let graph = open_graph(manager.graph(), &a, &c);
    let paths = graph.paths_between(&a.address, &c.address, &bounds(2));
    let solution = build_decomposition_graph(&market, None, &params(), paths).expect("a route");

    assert_eq!(branch_labels(&solution), ["A->C"]);
}

#[test]
fn test_minimum_price_drops_cheap_paths() {
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 10.0),
        ("ac", a.clone(), c.clone(), 2.0),
        ("cb", c.clone(), b.clone(), 0.5),
    ]);
    let mut params = params();
    // A->C->B prices at 2 * (1 / 0.5) = 4, below the floor; the direct pool prices at 10.
    params.minimum_price = 5.0;

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params, &bounds(2))
            .expect("the fixtures build a route");

    assert_eq!(branch_labels(&solution), ["A->B"]);
}

#[test]
fn test_a_token_outside_the_allowlist_is_not_routed_through() {
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 10.0),
        ("ac", a.clone(), c.clone(), 2.0),
        ("cb", c.clone(), b.clone(), 0.5),
    ]);
    // The allowlist admits the endpoints only, so `A->C->B` is never walked.
    let allowed = FxHashSet::from_iter([a.address.clone(), b.address.clone()]);
    let graph = TokenGraph::new(
        manager.graph(),
        &AllowedTokens {
            connector_tokens: Some(&allowed),
            prices: None,
            endpoints: [&a.address, &b.address],
        },
    );

    let solution = build(&graph, &market, None, &a, &b, &params(), &bounds(2))
        .expect("the fixtures build a route");

    assert_eq!(branch_labels(&solution), ["A->B"]);
}

#[test]
fn test_max_routes_keeps_the_heaviest_branches() {
    let (a, b, c, d) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"), token(0x0D, "D"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 10.0),
        ("ac", a.clone(), c.clone(), 2.0),
        ("cb", c.clone(), b.clone(), 0.5),
        ("ad", a.clone(), d.clone(), 1.0),
        ("db", d.clone(), b.clone(), 1.0),
    ]);
    let mut params = params();
    params.max_routes = 2;

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params, &bounds(2))
            .expect("the fixtures build a route");

    // Route prices are 10 (direct), 4 (via C) and 1 (via D); depths are absent so inertia is
    // the same for every pool and weight collapses to price.
    assert_eq!(branch_labels(&solution), ["A->B", "A->C->B"]);
}

#[test]
fn test_pool_depth_ranks_pools_within_a_hop() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![
        ("ab_shallow", a.clone(), b.clone(), 2.0),
        ("ab_deep", a.clone(), b.clone(), 2.0),
    ]);
    let depths: ComponentDepths = ComponentDepths::from_iter([
        (
            ("ab_shallow".to_string(), a.address.clone(), b.address.clone()),
            BigUint::from(1_000u64) * BigUint::from(10u64).pow(18),
        ),
        (
            ("ab_deep".to_string(), a.address.clone(), b.address.clone()),
            BigUint::from(1_000_000u64) * BigUint::from(10u64).pow(18),
        ),
        // Reverse-direction entries must not be picked up for an A->B hop.
        (
            ("ab_shallow".to_string(), b.address.clone(), a.address.clone()),
            BigUint::from(10u64).pow(30),
        ),
    ]);

    let solution = build(
        &open_graph(manager.graph(), &a, &b),
        &market,
        Some(&depths),
        &a,
        &b,
        &params(),
        &bounds(2),
    )
    .expect("the fixtures build a route");

    assert_eq!(pool_ids(&solution.branches()[0], 0), ["ab_deep", "ab_shallow"]);
}

#[test]
fn test_fresh_graph_is_unsolved() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("ab", a.clone(), b.clone(), 2.0)]);

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params(), &bounds(2))
            .expect("the fixtures build a route");

    assert!(!solution.solved());
    assert!(solution.outer_splits().is_empty());
    assert!(solution.branches()[0]
        .hop_at(0)
        .splits()
        .is_empty());
}

#[test]
fn test_all_paths_filtered_out_has_no_route() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("ab", a.clone(), b.clone(), 2.0)]);
    let mut params = params();
    params.minimum_price = 1_000.0;

    assert!(build(
        &open_graph(manager.graph(), &a, &b),
        &market,
        None,
        &a,
        &b,
        &params,
        &bounds(2)
    )
    .is_none());
}

#[test]
fn test_no_connecting_path_has_no_route() {
    let (a, b, c, d) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"), token(0x0D, "D"));
    let (market, manager) =
        market_with(vec![("ab", a.clone(), b.clone(), 2.0), ("cd", c.clone(), d.clone(), 2.0)]);

    assert!(build(
        &open_graph(manager.graph(), &a, &d),
        &market,
        None,
        &a,
        &d,
        &params(),
        &bounds(2)
    )
    .is_none());
}

#[test]
fn test_a_token_outside_the_graph_has_no_paths() {
    // Which endpoint is missing is answered by `TokenGraph::contains_token`, and the solve
    // asks before it searches. The build only ever sees an empty path set.
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let (market, manager) = market_with(vec![("ab", a.clone(), b.clone(), 2.0)]);
    let graph = open_graph(manager.graph(), &a, &c);

    assert!(!graph.contains_token(&c.address));
    assert!(graph
        .paths_between(&a.address, &c.address, &bounds(2))
        .is_empty());

    assert!(build(
        &open_graph(manager.graph(), &a, &c),
        &market,
        None,
        &a,
        &c,
        &params(),
        &bounds(2)
    )
    .is_none());
}

#[test]
fn test_max_hops_bounds_path_length() {
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let (market, manager) =
        market_with(vec![("ac", a.clone(), c.clone(), 2.0), ("cb", c.clone(), b.clone(), 0.5)]);

    assert!(build(
        &open_graph(manager.graph(), &a, &b),
        &market,
        None,
        &a,
        &b,
        &params(),
        &bounds(1)
    )
    .is_none());
}

#[test]
fn test_elapsed_deadline_truncates_the_enumeration() {
    // The clock is read every `DEADLINE_CHECK_INTERVAL` edges, so a graph this small never
    // reads it at all and a past deadline cannot stop it. That bounded overshoot is the
    // deliberate trade: the check must not cost more than the work it guards.
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("ab", a.clone(), b.clone(), 2.0)]);
    let mut bounds = bounds(2);
    bounds.deadline = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params(), &bounds)
            .expect("the fixtures build a route");

    assert_eq!(solution.branches().len(), 1);
}

#[test]
fn test_max_paths_truncates_the_enumeration() {
    // Three pools on the same pair are three enumerated paths. Capping at one keeps a whole,
    // solvable branch built from the first path found rather than a partial anything.
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![
        ("ab_1", a.clone(), b.clone(), 2.0),
        ("ab_2", a.clone(), b.clone(), 3.0),
        ("ab_3", a.clone(), b.clone(), 4.0),
    ]);
    let mut bounds = bounds(2);
    bounds.max_paths = 1;

    let solution =
        build(&open_graph(manager.graph(), &a, &b), &market, None, &a, &b, &params(), &bounds)
            .expect("the fixtures build a route");

    assert_eq!(solution.branches().len(), 1);
    assert_eq!(pool_ids(&solution.branches()[0], 0).len(), 1);
}

#[test]
fn test_max_routes_zero_is_rejected() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("ab", a.clone(), b.clone(), 2.0)]);
    let mut params = params();
    params.max_routes = 0;

    let graph = open_graph(manager.graph(), &a, &b);
    let paths = graph.paths_between(&a.address, &b.address, &bounds(2));
    let view = market_read(&market);
    let result = build_decomposition_graph(view.base_market_state(), None, &params, paths);

    assert!(matches!(result, Err(DecompositionError::InvalidInput { .. })));
}

// ===================== Grouping by neighbour token =====================

/// `A -> X` reachable four ways: through `B` then either `C` or `D`, through `E`, and directly.
///
/// Three of the four token paths leave `A` through the same `ab` pool, which is the shape the
/// grouping exists for.
fn forked_market() -> (MarketData, PetgraphStableDiGraphManager<DepthAndPrice>, Vec<Token>) {
    let (a, b, c) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"));
    let (d, e, x) = (token(0x0D, "D"), token(0x0E, "E"), token(0x0F, "X"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 2.0),
        ("bc", b.clone(), c.clone(), 2.0),
        ("cx", c.clone(), x.clone(), 2.0),
        ("bd", b.clone(), d.clone(), 2.0),
        ("dx", d.clone(), x.clone(), 2.0),
        ("bx", b.clone(), x.clone(), 2.0),
        ("ae", a.clone(), e.clone(), 2.0),
        ("ex", e.clone(), x.clone(), 2.0),
        ("ax", a.clone(), x.clone(), 2.0),
    ]);
    (market, manager, vec![a, b, c, d, e, x])
}

#[test]
fn test_token_paths_sharing_a_first_hop_become_one_branch() {
    let (market, manager, tokens) = forked_market();
    let (a, x) = (tokens[0].clone(), tokens[5].clone());

    let solution =
        build(&open_graph(manager.graph(), &a, &x), &market, None, &a, &x, &params(), &bounds(3))
            .expect("the fixtures build a route");

    // Six token paths — A>X, A>E>X, A>B>X, A>B>C>X, A>B>D>X and A>E... — collapse onto three
    // distinct neighbour tokens, so there are three outer splits rather than one per path.
    let mut leaves: Vec<String> = solution
        .branches()
        .iter()
        .map(|branch| branch.hop().token_out().symbol.clone())
        .collect();
    leaves.sort();
    assert_eq!(leaves, ["B", "E", "X"]);
    assert_eq!(solution.branches().len(), 3);
}

#[test]
fn test_a_grouped_branch_holds_its_shared_first_hop_once() {
    // The measured bug: three paths through one `A/B` pool each held a private copy of it, so
    // the outer split allocated that pool's liquidity three times over.
    let (market, manager, tokens) = forked_market();
    let (a, x) = (tokens[0].clone(), tokens[5].clone());

    let solution =
        build(&open_graph(manager.graph(), &a, &x), &market, None, &a, &x, &params(), &bounds(3))
            .expect("the fixtures build a route");

    let through_b = solution
        .branches()
        .iter()
        .find(|branch| branch.hop().token_out().symbol == "B")
        .expect("the paths through B were grouped");
    // Several tails, and the `ab` pool appears exactly once across the whole branch.
    assert!(through_b.sequences().len() > 1, "{}", through_b.token_path_label());
    let copies = through_b
        .hops()
        .flat_map(|hop| hop.pools())
        .filter(|pool| pool.component_id() == "ab")
        .count();
    assert_eq!(copies, 1);
    assert_eq!(pool_ids(through_b, 0), ["ab"]);
}

#[test]
fn test_a_group_on_the_buy_token_keeps_its_members_as_a_tail_less_branch() {
    // defibot drops all but the first member of this group (`order_solver.py:535-537`) with no
    // stated reason. Nothing is dropped here: the direct pool survives as its own branch,
    // carrying an outer split alongside the multi-hop ones.
    let (market, manager, tokens) = forked_market();
    let (a, x) = (tokens[0].clone(), tokens[5].clone());

    let solution =
        build(&open_graph(manager.graph(), &a, &x), &market, None, &a, &x, &params(), &bounds(3))
            .expect("the fixtures build a route");

    let direct = solution
        .branches()
        .iter()
        .find(|branch| branch.hop().token_out().symbol == "X")
        .expect("the direct pool is its own group");
    assert!(direct.sequences().is_empty());
    assert_eq!(direct.token_path_label(), "A->X");
    assert_eq!(pool_ids(direct, 0), ["ax"]);
}

#[test]
fn test_grouping_keeps_every_token_path_as_a_tail() {
    let (market, manager, tokens) = forked_market();
    let (a, x) = (tokens[0].clone(), tokens[5].clone());

    let solution =
        build(&open_graph(manager.graph(), &a, &x), &market, None, &a, &x, &params(), &bounds(3))
            .expect("the fixtures build a route");

    // Grouping must not lose alternatives, only stop double-counting the hop they share.
    let paths: usize = solution
        .branches()
        .iter()
        .map(|branch| branch.sequences().len().max(1))
        .sum();
    assert_eq!(paths, 5, "{}", branch_paths(solution.branches()));
}

// ---------- _remove_duplicated_routes ----------

/// A tail `token_in -> token_out` over one pool per component id.
fn tail(token_in: &Token, token_out: &Token, components: &[&str]) -> SequentialRoute {
    let pools = components
        .iter()
        .map(|id| {
            PoolRef::new(
                (*id).to_string(),
                SellLimitKind::Enforced,
                Box::new(MockProtocolSim::new(2.0).with_fee(0.0)),
                None,
            )
        })
        .collect();
    SequentialRoute::new(
        vec![token_in.clone(), token_out.clone()],
        vec![Hop::new(token_in.clone(), token_out.clone(), pools).expect("hop has pools")],
    )
    .expect("route matches its token path")
}

#[test]
fn test_remove_duplicated_routes_drops_a_pool_appearing_in_two_tails() {
    // A pool with more than two tokens can serve two legs, so two parallel tails can hold it.
    // Left alone, the tail split would spend its liquidity twice at once.
    let (b, x) = (token(0x0B, "B"), token(0x0F, "X"));
    let mut tails = vec![tail(&b, &x, &["shared", "only_first"]), tail(&b, &x, &["shared"])];

    remove_duplicated_routes(&mut tails);

    // The lower-weight tail gives it up — here that leaves it with no pools, so the tail goes.
    assert_eq!(tails.len(), 1);
    assert_eq!(pool_ids_of(&tails[0]), ["shared", "only_first"], "the heavier tail keeps the pool");
}

#[test]
fn test_remove_duplicated_routes_keeps_a_tail_that_survives_losing_the_pool() {
    let (b, x) = (token(0x0B, "B"), token(0x0F, "X"));
    let mut tails = vec![tail(&b, &x, &["shared"]), tail(&b, &x, &["shared", "spare"])];

    remove_duplicated_routes(&mut tails);

    // Removing the duplicate from the second tail leaves it a pool, so nothing is dropped
    // wholesale — the best case of `order_solver.py:773-779`.
    assert_eq!(tails.len(), 2);
    assert_eq!(pool_ids_of(&tails[0]), ["shared"]);
    assert_eq!(pool_ids_of(&tails[1]), ["spare"]);
}

#[test]
fn test_remove_duplicated_routes_leaves_disjoint_tails_alone() {
    let (b, x) = (token(0x0B, "B"), token(0x0F, "X"));
    let mut tails = vec![tail(&b, &x, &["first"]), tail(&b, &x, &["second"])];

    remove_duplicated_routes(&mut tails);

    assert_eq!(tails.len(), 2);
    assert_eq!(pool_ids_of(&tails[0]), ["first"]);
    assert_eq!(pool_ids_of(&tails[1]), ["second"]);
}

fn pool_ids_of(tail: &SequentialRoute) -> Vec<String> {
    tail.hops()
        .iter()
        .flat_map(Hop::pools)
        .map(|pool| pool.component_id().clone())
        .collect()
}

/// A pool trading at `spot_price` with no fee.
fn pool(id: &str, spot_price: f64) -> PoolRef {
    PoolRef::new(
        id.to_string(),
        SellLimitKind::Enforced,
        Box::new(MockProtocolSim::new(spot_price).with_fee(0.0)),
        None,
    )
}

/// A one-pool hop between two tokens, already solved — its single pool takes everything.
///
/// Selling on a branch requires every hop below it to be solved, so the fixtures build them that
/// way rather than having each test set the same split.
fn hop(id: &str, token_in: &Token, token_out: &Token) -> Hop {
    let mut hop =
        Hop::new(token_in.clone(), token_out.clone(), vec![pool(id, 2.0)]).expect("hop has pools");
    hop.set_splits(vec![Fraction::one()])
        .expect("one split for one pool");
    hop
}

/// A token path through `tokens`, one pool per leg, pool ids taken from `ids`.
fn path(tokens: &[Token], ids: &[&str]) -> SequentialRoute {
    let hops = tokens
        .windows(2)
        .zip(ids)
        .map(|(pair, id)| hop(id, &pair[0], &pair[1]))
        .collect();
    SequentialRoute::new(tokens.to_vec(), hops).expect("route matches its token path")
}

/// `A`, `B`, `C`, `D`, `X` — `A` sells, `X` buys.
fn tokens() -> (Token, Token, Token, Token, Token) {
    (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"), token(0x0D, "D"), token(0x11, "X"))
}

// ==================== choosing the end ====================

#[test]
fn test_distinct_neighbours_counts_each_end() {
    let (a, b, c, d, x) = tokens();
    // Two paths leaving through B and C, both arriving through D.
    let routes = vec![
        path(&[a.clone(), b, d.clone(), x.clone()], &["ab", "bd", "dx"]),
        path(&[a, c, d, x], &["ac", "cd", "dx2"]),
    ];

    assert_eq!(distinct_neighbours(&routes, NeighbourEnd::Head), 2);
    assert_eq!(distinct_neighbours(&routes, NeighbourEnd::Tail), 1);
}

#[test]
fn test_grouping_picks_the_end_with_fewer_branches() {
    let (a, b, c, d, x) = tokens();
    // Two sell-side neighbours, one buy-side: tail grouping yields the single branch.
    let routes = vec![
        path(&[a.clone(), b, d.clone(), x.clone()], &["ab", "bd", "dx"]),
        path(&[a, c, d, x], &["ac", "cd", "dx2"]),
    ];

    let branches = group_into_branches(routes, 10).expect("grouping succeeds");

    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].side(), BranchSide::Tail);
    assert_eq!(branches[0].sequences().len(), 2);
}

#[test]
fn test_grouping_keeps_the_head_end_when_it_is_no_worse() {
    let (a, b, c, d, x) = tokens();
    // One sell-side neighbour, two buy-side: head grouping wins outright.
    let routes = vec![
        path(&[a.clone(), b.clone(), c, x.clone()], &["ab", "bc", "cx"]),
        path(&[a, b, d, x], &["ab2", "bd", "dx"]),
    ];

    let branches = group_into_branches(routes, 10).expect("grouping succeeds");

    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].side(), BranchSide::Head);
}

#[test]
fn test_grouping_breaks_a_tie_towards_the_head() {
    let (a, b, c, d, x) = tokens();
    // Two neighbours at each end. defibot's shape is the head one, so a tie keeps it.
    let routes = vec![
        path(&[a.clone(), b, c.clone(), x.clone()], &["ab", "bc", "cx"]),
        path(&[a, d, c, x], &["ad", "dc", "cx2"]),
    ];
    assert_eq!(distinct_neighbours(&routes, NeighbourEnd::Head), 2);
    assert_eq!(distinct_neighbours(&routes, NeighbourEnd::Tail), 1);

    // Make the tail end just as varied as the head end.
    let (a, b, c, d, x) = tokens();
    let routes = vec![
        path(&[a.clone(), b, c.clone(), x.clone()], &["ab", "bc", "cx"]),
        path(&[a, d.clone(), d, x], &["ad", "dd", "dx"]),
    ];
    assert_eq!(distinct_neighbours(&routes, NeighbourEnd::Head), 2);
    assert_eq!(distinct_neighbours(&routes, NeighbourEnd::Tail), 2);

    let branches = group_into_branches(routes, 10).expect("grouping succeeds");
    assert!(branches
        .iter()
        .all(|branch| branch.side() == BranchSide::Head));
}

#[test]
fn test_a_direct_path_groups_as_a_one_hop_branch() {
    let (a, b, _c, _d, x) = tokens();
    // A direct pool has nothing before its last hop, so tail grouping must not try to build a
    // sequence out of it.
    let routes = vec![path(&[a.clone(), x.clone()], &["ax"]), path(&[a, b, x], &["ab", "bx"])];

    let branches = group_by_tail_token(routes, 10).expect("grouping succeeds");

    let direct = branches
        .iter()
        .find(|branch| branch.sequences().is_empty())
        .expect("the direct path became a one-hop branch");
    assert_eq!(direct.side(), BranchSide::Head);
    assert_eq!(direct.token_path_label(), "A->X");
}
