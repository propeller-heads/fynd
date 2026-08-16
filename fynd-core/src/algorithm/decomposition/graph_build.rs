//! Candidate discovery for the decomposition algorithm.
//!
//! Port of `build_routes_subgraph` and `_create_one_hop_route`
//! (`defibot/solver/order_solver/decomposition/order_solver.py:387-515` and `:556-573`). The
//! output is an unsolved [`DecompositionGraph`]: every pool-level alternative that survives
//! filtering, arranged into the fixed three-level shape the optimizers later assign splits to.
//!
//! The pipeline is:
//!
//! 1. Enumerate every simple token path from sell to buy token within the hop limit, one path per
//!    pool combination.
//! 2. Drop paths touching an excluded token, and paths whose marginal price is non-positive or
//!    below the configured floor.
//! 3. Group the surviving paths by their token sequence. One group becomes one [`SequentialRoute`];
//!    the pools the group offers at leg *i* become that leg's [`Hop`].
//! 4. Rank those token paths on [`SequentialRoute::weight`].
//! 5. Group them again, by the first token after the sell token ([`group_by_head_token`],
//!    `order_solver.py:517-554`). One group becomes one [`Branch`]: a single shared first hop plus
//!    one tail per member.
//! 6. Keep the best [`SubgraphParams::max_routes`] branches.
//!
//! # Why there are two grouping steps
//!
//! Step 3 makes one branch per *token sequence*, so `A>B>C` and `A>B>D>C` stay apart. But both
//! leave `A` through the `A/B` pools, and each holds a private [`PoolRef`] copy of them. The outer
//! split search scores branches against untouched state, so it would allocate that pool's liquidity
//! once to each — an order split across three paths through one first hop is priced as though the
//! pool had three times its depth.
//!
//! Step 5 is the fix, and it is the reason `_group_by_neighbour_token` exists in defibot. Its
//! docstring: *"This allows for solving each group as a subgraph of independent paths (i.e. no
//! shared pools)."* After it, the shared hop is one object that the branch sells exactly once.
//!
//! Sharing *between* branches is a different problem and is still handled elsewhere:
//! `sell_with_coupled_paths` re-sells the branches against each other's post-trade liquidity, and
//! `split_primitives::build_split_route` merges a hop shared across branches into one on-chain
//! swap.
//!
//! # Deviations from defibot
//!
//! * defibot keeps only the first member of a group whose neighbour token is the buy token
//!   (`order_solver.py:535-537`), with no comment or test explaining the drop. Every member is kept
//!   here. In this pipeline the distinction is invisible — step 3 has already merged all direct
//!   `sell -> buy` pool paths into a single token sequence, so that group has exactly one member —
//!   but the rule is not reproduced, because dropping alternatives on an unexplained condition
//!   would lose liquidity the moment the shape changed.
//! * defibot unwraps a depth-1 graph whose single member is itself a `ParallelRoute` (`:508-510`).
//!   That is tree bookkeeping with no analogue in a fixed structure.
//! * defibot filters paths by symbol (`:437`); tokens are identified by address here, since symbols
//!   are not unique on-chain.
//! * A path whose marginal price is not finite is dropped. defibot compares `Decimal` NaN, which
//!   silently answers `False` to both `<= 0` and `< minimum_price` and lets the path through.
//!
//! # Bounding the enumeration
//!
//! Simple-path enumeration is exponential in the hop limit and defibot bounds it with a topology
//! filter on `list_paths` (`defibot/solver/market_graph/_market_graph.py:229-236`). That filter is
//! deliberately **not** ported: it needs the encoded price functions served by
//! `price_function_gw`, which Fynd has no equivalent of, and defibot's own base configuration ships
//! it off (`propeller-solver-core/core/defibot.yaml:620`, `enable_topology_filter: false`) — so
//! porting it faithfully would produce a disabled feature.
//!
//! The enumeration is bounded directly instead, by two limits carried on
//! [`SubgraphParams`]: a wall-clock [`deadline`](SubgraphParams::deadline) and a cap on
//! the number of paths ([`max_paths`](SubgraphParams::max_paths)). Hitting either stops the
//! search and keeps the paths found so far.
//!
//! **The cost is candidate quality, never correctness.** The search is depth-first in graph edge
//! order, so a truncated run is a *prefix* of the full path set, not a sample of it: every path it
//! kept is whole, and every later stage — grouping, ranking, solving, assembly, `Route::validate`
//! — runs to completion over that smaller set. The solve returns a route that is complete and
//! encodable; it may simply not be the best one. The alternative, letting a dense three-hop graph
//! run the solve clock out, returns nothing at all.

use rustc_hash::{FxHashMap, FxHashSet};
use tracing::debug;
use tycho_simulation::tycho_common::models::{token::Token, Address};

use crate::{
    algorithm::decomposition::{
        components::{
            Branch, DecompositionError, DecompositionGraph, Hop, PoolRef, SellLimitKind,
            SequentialRoute,
        },
        models::DirectPath,
    },
    derived::types::ComponentDepths,
    feed::market_data::MarketState,
    types::ComponentId,
};

/// Inputs to [`build_decomposition_graph`].
pub(crate) struct SubgraphParams {
    /// Cap on parallel alternatives kept, applied both to the branches of the graph and to the
    /// pools of a single hop.
    ///
    /// This is defibot's `solver.order_solver.decomposition.max_splits` (30 in production,
    /// `solver-config.yaml:195`), renamed because defibot reuses the name for the EqualStartV2
    /// optimizer's iteration budget, which is an unrelated quantity.
    pub(crate) max_routes: usize,
    /// Lowest acceptable path marginal price, in buy-token per sell-token human units.
    pub(crate) minimum_price: f64,
}

/// Builds the unsolved candidate graph between the order's endpoints.
///
/// The returned [`DecompositionGraph`] has no outer splits and no hop splits: it is a menu of
/// alternatives, and [`DecompositionGraph::weight`] must therefore take its unsolved
/// (maximum-over-branches) branch when the caller ranks it.
///
/// `depths` is the derived [`ComponentDepths`] store, or `None` when depths have not been computed.
/// Depth is looked up per hop *direction*; a missing entry is legitimate and lets
/// [`PoolRef::inertia`] fall back.
///
/// `None` when `paths` is empty or every path was filtered out (`order_solver.py:431-432`,
/// `:502-503`); the caller knows the endpoints and reports the failure.
///
/// # Errors
///
/// [`AlgorithmError::InvalidConfiguration`] when `max_routes` is zero.
pub(crate) fn build_decomposition_graph(
    market: &MarketState,
    depths: Option<&ComponentDepths>,
    params: &SubgraphParams,
    paths: Vec<DirectPath>,
) -> Result<DecompositionGraph, DecompositionError> {
    if params.max_routes == 0 {
        // Every cap below truncates to this, so zero would empty each hop and report the emptiness
        // as a structural failure two layers down. `DecompositionConfig::new` rejects it at
        // startup; this covers a caller that builds `SubgraphParams` by hand.
        return Err(DecompositionError::InvalidInput {
            reason: "max_routes must be at least 1".to_string(),
        });
    }
    if paths.is_empty() {
        debug!("decomposition found no candidate path");
        return Err(DecompositionError::GraphBuildFailure);
    }

    let mut sequences: Vec<SequentialRoute> = Vec::new();
    for group in group_by_token_sequence(market, &paths, params) {
        match build_branch(market, depths, &group, params.max_routes) {
            Ok(route) => sequences.push(route),
            // One token sequence has a leg no pool is left to trade. The others are unaffected.
            Err(DecompositionError::EmptyHop { token_in, token_out }) => {
                debug!(%token_in, %token_out, "dropping a token sequence with an untradable leg");
            }
            Err(error) => return Err(error),
        }
    }
    if sequences.is_empty() {
        debug!(paths = paths.len(), "decomposition priced no candidate path");
        return Err(DecompositionError::GraphBuildFailure);
    }

    // defibot ranks the token paths and only then groups them (`order_solver.py:503-514`), so a
    // group's first member — the one whose first hop becomes the shared head — is its heaviest.
    let ranked_sequences = rank_subgraph(sequences, SequentialRoute::weight)?;
    let n_ranked = ranked_sequences.len();
    let mut branches = group_into_branches(ranked_sequences, params.max_routes)?;

    let grouped = branches.len();
    branches.truncate(params.max_routes);

    debug!(
        enumerated_paths = paths.len(),
        n_ranked,
        grouped_branches = grouped,
        kept_branches = branches.len(),
        max_routes = params.max_routes,
        hops = branches
            .iter()
            .map(|branch| branch.hops().count())
            .max()
            .unwrap_or(0),
        "decomposition candidate subgraph built"
    );

    if grouped > branches.len() {
        debug!(
            dropped = grouped - branches.len(),
            "decomposition dropped branches at the cap; kept paths: {}",
            branch_paths(&branches)
        );
    } else {
        debug!("decomposition kept every branch; paths: {}", branch_paths(&branches));
    }
    DecompositionGraph::new(branches, Vec::new())
}

// ===================== Grouping by neighbour token =====================

/// Groups token paths by the first token after the sell token
/// (`_group_by_neighbour_token`, `order_solver.py:517-554`).
///
/// **This is what stops a shared first-hop pool from being allocated more than once.** Each token
/// path owns a private [`PoolRef`] copy of the pools on its first leg, so before grouping, three
/// paths leaving `GNO` through the same `GNO/USDT` pool became three outer splits that the
/// optimizer scored as if each had exclusive access to it. defibot's docstring states the purpose
/// exactly: *"This allows for solving each group as a subgraph of independent paths (i.e. no shared
/// pools)."*
///
/// One group becomes one [`Branch`]: the head is the first member's first hop — the members are in
/// descending weight order, so that is the group's best-ranked view of the shared leg — and the
/// tails are what remains of each member. Tails are ranked and capped at `max_routes` (`:549-550`),
/// then [`remove_duplicated_routes`] drops any pool appearing in two of them (`:551`).
///
/// Groups come back in the order their best member appeared, which is defibot's dict insertion
/// order over the weight-sorted path list.
///
/// # Deviations from defibot
///
/// * defibot keeps only the *first* member of a group whose neighbour token is the buy token
///   (`:535-537`), with no comment or test explaining the drop. All members are kept here: a group
///   on the buy token is a direct pool competing with paths that reach the buy token and then trade
///   on, and discarding them on an unexplained rule would lose liquidity.
/// * Only the first member's first hop survives. A pool that appears on the shared leg of a later
///   member but not of the first is dropped with it, exactly as in defibot.
///
/// # Errors
///
/// Whatever [`SequentialRoute`] and [`Branch`] construction raise, and [`SequentialRoute::weight`]
/// through the ranking.
/// Groups the token paths whichever way yields fewer branches.
///
/// Both groupings remove the same error — a pool held privately by several paths, which the split
/// search then hands its full liquidity to once per path — but each removes it at one end only.
/// Grouping on the token after the sell token leaves the paths' *last* hops duplicated across
/// branches; grouping on the token before the buy token leaves their *first* hops duplicated.
///
/// Fewer branches means fewer duplicates of whatever the ungrouped end holds, because a pool can
/// appear at most once per branch. So the branch count is the thing to minimise, and it is known
/// before any solving: it is the number of distinct neighbour tokens at each end.
///
/// The two are not interchangeable. Selling a thinly-connected token into a well-connected one
/// wants head grouping; the reverse wants tail grouping. On the recorded fixture `GNO->AAVE` has
/// two sell-side neighbours against three buy-side, and `WETH->GNO` has twenty-seven against two —
/// so the choice has to be made per order, not once for the algorithm.
///
/// Ties keep the head grouping, which is defibot's shape (`order_solver.py:517-554`).
fn group_into_branches(
    sequences: Vec<SequentialRoute>,
    max_branches: usize,
) -> Result<Vec<Branch>, DecompositionError> {
    let head_groups = distinct_neighbours(&sequences, NeighbourEnd::Head);
    let tail_groups = distinct_neighbours(&sequences, NeighbourEnd::Tail);
    debug!(
        token_paths = sequences.len(),
        head_groups,
        tail_groups,
        grouping = if tail_groups < head_groups { "tail" } else { "head" },
        "grouping the token paths at whichever end yields fewer branches"
    );
    if tail_groups < head_groups {
        group_by_tail_token(sequences, max_branches)
    } else {
        group_by_head_token(sequences, max_branches)
    }
}

/// Which end of a token path a grouping keys on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NeighbourEnd {
    /// The token immediately after the sell token.
    Head,
    /// The token immediately before the buy token.
    Tail,
}

/// How many groups keying on `end` would produce.
fn distinct_neighbours(routes: &[SequentialRoute], end: NeighbourEnd) -> usize {
    let mut seen: FxHashSet<&Address> = FxHashSet::default();
    for route in routes {
        let hops = route.hops();
        let neighbour = match end {
            NeighbourEnd::Head => &hops[0].token_out().address,
            NeighbourEnd::Tail => &hops[hops.len() - 1].token_in().address,
        };
        seen.insert(neighbour);
    }
    seen.len()
}

/// Groups token paths by the token immediately *before* the buy token.
///
/// The mirror of [`group_by_head_token`]. One group becomes one [`Branch`] whose hop is the
/// group's shared last leg and whose sequences are what precedes it in each member.
///
/// A direct `sell -> buy` path has nothing before its last hop, so it contributes no sequence; a
/// group made only of such paths is a one-hop branch and is built head-sided, where the two shapes
/// coincide.
///
/// # Errors
///
/// Whatever [`SequentialRoute`] and [`Branch`] construction raise, and [`SequentialRoute::weight`]
/// through the ranking.
fn group_by_tail_token(
    routes: Vec<SequentialRoute>,
    max_routes: usize,
) -> Result<Vec<Branch>, DecompositionError> {
    let mut order: Vec<Address> = Vec::new();
    let mut members: FxHashMap<Address, Vec<SequentialRoute>> = FxHashMap::default();
    for route in routes {
        let hops = route.hops();
        let neighbour = hops[hops.len() - 1]
            .token_in()
            .address
            .clone();
        if !members.contains_key(&neighbour) {
            order.push(neighbour.clone());
        }
        members
            .entry(neighbour)
            .or_default()
            .push(route);
    }

    let mut branches = Vec::with_capacity(order.len());
    for neighbour in order {
        let group = members
            .remove(&neighbour)
            .unwrap_or_default();
        branches.push(build_tail_branch(group, max_routes)?);
    }
    Ok(branches)
}

/// Turns one tail-neighbour group into a [`Branch`]. The mirror of [`build_head_branch`].
fn build_tail_branch(
    group: Vec<SequentialRoute>,
    max_routes: usize,
) -> Result<Branch, DecompositionError> {
    let mut shared_hop = None;
    let mut sequences = Vec::with_capacity(group.len());
    for route in group {
        let (tokens, mut hops) = route.into_parts();
        let Some(last_hop) = hops.pop() else {
            continue;
        };
        if shared_hop.is_none() {
            shared_hop = Some(last_hop);
        }
        if hops.is_empty() {
            // The path is the shared hop itself, so it contributes no sequence.
            continue;
        }
        let leading = tokens[..tokens.len() - 1].to_vec();
        sequences.push(SequentialRoute::new(leading, hops)?);
    }

    let Some(shared_hop) = shared_hop else {
        return Err(DecompositionError::InvalidStructure {
            reason: "decomposition grouped an empty tail-neighbour group".to_string(),
        });
    };

    if sequences.is_empty() {
        // Every member was the shared hop alone: a one-hop branch, where both shapes agree.
        return Branch::head(shared_hop, Vec::new(), Vec::new());
    }

    let mut sequences = rank_subgraph(sequences, SequentialRoute::weight)?;
    sequences.truncate(max_routes);
    remove_duplicated_routes(&mut sequences);
    Branch::tail(shared_hop, sequences, Vec::new())
}

fn group_by_head_token(
    routes: Vec<SequentialRoute>,
    max_routes: usize,
) -> Result<Vec<Branch>, DecompositionError> {
    let mut order: Vec<Address> = Vec::new();
    let mut members: FxHashMap<Address, Vec<SequentialRoute>> = FxHashMap::default();
    for route in routes {
        let neighbour = route.hops()[0]
            .token_out()
            .address
            .clone();
        if !members.contains_key(&neighbour) {
            order.push(neighbour.clone());
        }
        members
            .entry(neighbour)
            .or_default()
            .push(route);
    }

    let mut branches = Vec::with_capacity(order.len());
    for neighbour in order {
        let group = members
            .remove(&neighbour)
            .unwrap_or_default();
        branches.push(build_head_branch(group, max_routes)?);
    }
    Ok(branches)
}

/// Turns one neighbour-token group into a [`Branch`] (`order_solver.py:538-553`).
fn build_head_branch(
    group: Vec<SequentialRoute>,
    max_routes: usize,
) -> Result<Branch, DecompositionError> {
    let mut head = None;
    let mut tails = Vec::with_capacity(group.len());
    for route in group {
        let (tokens, mut hops) = route.into_parts();
        let first_hop = hops.remove(0);
        if head.is_none() {
            head = Some(first_hop);
        }
        if hops.is_empty() {
            // The path ends at the neighbour token, so the group's head is the whole of it. Such a
            // member contributes no tail; if every member is like this the branch is one hop.
            continue;
        }
        tails.push(SequentialRoute::new(tokens[1..].to_vec(), hops)?);
    }

    let Some(head) = head else {
        return Err(DecompositionError::InvalidStructure {
            reason: "decomposition grouped an empty neighbour-token group".to_string(),
        });
    };

    let mut tails = rank_subgraph(tails, SequentialRoute::weight)?;
    tails.truncate(max_routes);
    remove_duplicated_routes(&mut tails);
    Branch::head(head, tails, Vec::new())
}

/// Drops pools that appear in more than one tail of the same group
/// (`_remove_duplicated_routes`, `order_solver.py:726-796`).
///
/// A pool with more than two tokens can serve two different legs, so two tails of one branch can
/// hold the same component. They are parallel — the branch's `tail_splits` sends flow down both at
/// once — so leaving the duplicate would let the split search spend that pool's liquidity twice,
/// which is the same error at the tail level that grouping fixes at the head level.
///
/// Tails arrive in descending weight order and are walked in reverse, so the *lowest*-weight tail
/// gives the pool up (`:737-739`). Removing it from a leg holding other pools is enough; a leg it
/// would empty takes the whole tail with it, because a hop with no pools cannot be sold through
/// (`:762-772`).
///
/// defibot closes with an `assert` that the deduplication worked (`:794-796`). That is not ported:
/// asserts vanish under `python -O` and are a hard crash otherwise, and a solver worker must not
/// die over a candidate it could simply have dropped.
fn remove_duplicated_routes(tails: &mut Vec<SequentialRoute>) {
    let mut duplicated: Vec<ComponentId> = Vec::new();
    let mut seen: FxHashSet<ComponentId> = FxHashSet::default();
    for component_id in tails.iter().flat_map(tail_components) {
        if !seen.insert(component_id.clone()) {
            duplicated.push(component_id);
        }
    }

    for component_id in duplicated {
        // The pool may already have gone with a tail removed for an earlier duplicate.
        let Some(index) = tails
            .iter()
            .rposition(|tail| tail_components(tail).contains(&component_id))
        else {
            continue;
        };
        if tails
            .iter()
            .filter(|tail| tail_components(tail).contains(&component_id))
            .count() <
            2
        {
            continue;
        }

        let survives = tails[index]
            .hops_mut()
            .iter_mut()
            .filter(|hop| {
                hop.pools()
                    .iter()
                    .any(|pool| pool.component_id() == &component_id)
            })
            .all(|hop| hop.remove_pool(&component_id));
        if !survives {
            debug!(
                %component_id,
                "duplicated pool would empty a leg; dropping the whole tail"
            );
            tails.remove(index);
        }
    }
}

/// Component ids of every pool in a tail.
fn tail_components(tail: &SequentialRoute) -> Vec<ComponentId> {
    tail.hops()
        .iter()
        .flat_map(Hop::pools)
        .map(|pool| pool.component_id().clone())
        .collect()
}

/// Token symbols of every branch, as `A>B>C | A>B>[C>D | D]`, with the pool count of each hop.
///
/// Diagnostic only: it answers "was this token path considered, did it survive the cap, and which
/// branch did it end up sharing a first hop with" without reasoning backwards from the route.
fn branch_paths(branches: &[Branch]) -> String {
    branches
        .iter()
        .map(|branch| {
            let pools = branch
                .hops()
                .map(|hop| hop.pools().len().to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("{}[{pools}]", branch.token_path_label())
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

// ===================== Filtering and grouping =====================

/// All pool-level alternatives sharing one token sequence — the raw material of one branch.
struct TokenSequenceGroup<'a> {
    /// Tokens of the sequence, resolved against the market's token registry.
    tokens: Vec<Token>,
    /// One component sequence per surviving path, in enumeration order.
    component_paths: Vec<&'a [ComponentId]>,
}

/// Filters the enumerated paths and groups the survivors by token sequence
/// (`order_solver.py:434-444`).
///
/// Groups are returned in first-seen order so a given market always produces the same graph.
fn group_by_token_sequence<'a>(
    market: &MarketState,
    paths: &'a [DirectPath],
    params: &SubgraphParams,
) -> Vec<TokenSequenceGroup<'a>> {
    let mut groups: Vec<TokenSequenceGroup<'a>> = Vec::new();
    // Keyed by a whole token sequence and probed once per enumerated path, so the key is long and
    // the lookup count is high; the addresses come from our own graph, not from an attacker.
    let mut slot_of_sequence: FxHashMap<&'a [Address], usize> = FxHashMap::default();

    for path in paths {
        let Some(tokens) = resolve_tokens(market, &path.tokens) else {
            continue;
        };
        let Some(price) = marginal_price(market, &tokens, &path.components) else {
            continue;
        };
        if !price.is_finite() || price <= 0.0 {
            continue;
        }
        if params.minimum_price > 0.0 && price < params.minimum_price {
            continue;
        }

        match slot_of_sequence.get(path.tokens.as_slice()) {
            Some(&slot) => groups[slot]
                .component_paths
                .push(&path.components),
            None => {
                slot_of_sequence.insert(&path.tokens, groups.len());
                groups.push(TokenSequenceGroup { tokens, component_paths: vec![&path.components] });
            }
        }
    }

    groups
}

/// Resolves path addresses against the market's token registry.
///
/// Returns `None` when any token is unknown: without decimals a hop cannot price or simulate.
fn resolve_tokens(market: &MarketState, addresses: &[Address]) -> Option<Vec<Token>> {
    let mut tokens = Vec::with_capacity(addresses.len());
    for address in addresses {
        let Some(token) = market.get_token(address) else {
            debug!(%address, "token missing from registry; dropping path");
            return None;
        };
        tokens.push(token.clone());
    }
    Some(tokens)
}

/// Marginal price of a path: the product of `spot_price * (1 - fee)` over its hops
/// (`defibot/solver/path.py:141-155`).
///
/// Returns `None` when a component's state is missing or its spot price cannot be computed.
fn marginal_price(
    market: &MarketState,
    tokens: &[Token],
    components: &[ComponentId],
) -> Option<f64> {
    let mut price = 1.0;
    for (leg, component_id) in components.iter().enumerate() {
        let Some(state) = market.get_simulation_state(component_id) else {
            debug!(%component_id, "component state missing; dropping path");
            return None;
        };
        let spot_price = state
            .spot_price(&tokens[leg], &tokens[leg + 1])
            .inspect_err(|error| debug!(%component_id, %error, "spot price failed; dropping path"))
            .ok()?;
        price *= spot_price * (1.0 - state.fee());
    }
    Some(price)
}

// ===================== Branch assembly =====================

/// Turns one token-sequence group into a branch (`order_solver.py:446-500`).
///
/// A pool already used at an earlier leg is skipped at every later one: the two legs would compete
/// for the same liquidity, and their outputs could not be summed from independent simulations. If
/// that leaves a leg with no pools the whole sequence is discarded (`:476-496`), which is why this
/// returns `Option`.
fn build_branch(
    market: &MarketState,
    depths: Option<&ComponentDepths>,
    group: &TokenSequenceGroup<'_>,
    max_routes: usize,
) -> Result<SequentialRoute, DecompositionError> {
    let mut seen_pools: FxHashSet<&ComponentId> = FxHashSet::default();
    let mut hops = Vec::with_capacity(group.tokens.len() - 1);

    for (hop, pair) in group.tokens.windows(2).enumerate() {
        let (token_in, token_out) = (&pair[0], &pair[1]);
        let mut pools = Vec::new();

        for components in &group.component_paths {
            let component_id = &components[hop];
            if seen_pools.contains(component_id) {
                continue;
            }
            let Some(state) = market.get_simulation_state(component_id) else {
                debug!(%component_id, "component state missing; dropping pool from hop");
                continue;
            };
            seen_pools.insert(component_id);
            // A pool whose component is missing is treated as enforcing its limit: that is the
            // conservative reading, and it matches what every non-constant-product pool does.
            let limit_kind = market
                .get_component(component_id)
                .map_or(SellLimitKind::Enforced, |component| {
                    SellLimitKind::for_protocol_system(&component.protocol_system)
                });
            pools.push(PoolRef::new(
                component_id.clone(),
                limit_kind,
                state.clone_box(),
                lookup_depth(depths, component_id, token_in, token_out),
            ));
        }

        if pools.is_empty() {
            // CZ: No pools at the current hop. should never happen.
            //
            // It can: `seen_pools` gives a pool claimed by an earlier leg to that leg only, so a
            // leg whose every pool went that way is left with none. Only this token sequence is
            // affected, which is why the caller drops it and keeps the others.
            return Err(DecompositionError::EmptyHop {
                token_in: token_in.address.clone(),
                token_out: token_out.address.clone(),
            });
        }

        // defibot's `_create_one_hop_route` (`order_solver.py:556-573`): deduplicate, rank, cap.
        // Deduplication already happened above via `seen_pools`.

        // CZ: cant we just continue in case the statement below fails? I think not because this is
        // a hop so it would break the path
        let mut pools = rank_subgraph(pools, |pool| pool.weight(token_in, token_out))?;
        pools.truncate(max_routes);
        hops.push(Hop::new(token_in.clone(), token_out.clone(), pools)?);
    }

    SequentialRoute::new(group.tokens.clone(), hops)
}

/// Depth of `component_id` for the direction `token_in -> token_out`.
///
/// The key is direction-sensitive: the reverse entry is a different quantity in different units,
/// and substituting it would silently misrank the pool rather than fail.
fn lookup_depth(
    depths: Option<&ComponentDepths>,
    component_id: &ComponentId,
    token_in: &Token,
    token_out: &Token,
) -> Option<num_bigint::BigUint> {
    depths?
        .get(&(component_id.clone(), token_in.address.clone(), token_out.address.clone()))
        .cloned()
}

// ===================== Shared helpers =====================

/// Sorts `items` by a score, highest first.
///
/// A NaN score sorts last rather than first, which `f64::total_cmp` alone would do for a positive
/// NaN. Scores are computed once up front because they are simulation calls, not comparisons.
fn rank_subgraph<T, F>(items: Vec<T>, score: F) -> Result<Vec<T>, DecompositionError>
where
    F: Fn(&T) -> Result<f64, DecompositionError>,
{
    let mut scored = Vec::with_capacity(items.len());
    for item in items {
        let value = score(&item)?;
        let value = if value.is_nan() { f64::NEG_INFINITY } else { value };
        scored.push((value, item));
    }
    scored.sort_by(|left, right| right.0.total_cmp(&left.0));
    Ok(scored
        .into_iter()
        .map(|(_, item)| item)
        .collect())
}

#[cfg(test)]
#[path = "tests/graph_build_tests.rs"]
mod tests;
