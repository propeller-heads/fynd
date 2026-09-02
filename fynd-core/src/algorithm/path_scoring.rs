//! Ranking token paths by what a single unsplit route through them actually buys.
//!
//! A token path names the tokens, not the pools. Scoring one means choosing pools: at each leg,
//! whichever pool pays best net of its own gas, with that leg's output becoming the next leg's
//! input. What comes out the far end is the path's score.
//!
//! For `most_liquid` this walk *is* the algorithm: it picks one route and returns it. It lives in
//! a module of its own because it guarantees three things that are worth stating once:
//!
//! * the per-leg choice made on output *net of that pool's gas*, not on gross output;
//! * a pool the path already crossed withheld from its later legs, so one pool's liquidity cannot
//!   be spent twice inside a single path's score;
//! * the winner of a pair remembered, so paths sharing a leg do not each rescan it.
//!
//! What is deliberately *not* here is the swap itself. The caller passes a closure that answers
//! what one pool pays for an amount, so whether that reaches the pool or is served from amounts
//! already asked stays the caller's business — `most_liquid` answers it through a
//! [`SwapCache`](crate::algorithm::swap_cache::SwapCache), and does not have to say so here.

use num_bigint::{BigInt, BigUint};
use petgraph::stable_graph::NodeIndex;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use tracing::trace;

use crate::{
    algorithm::{most_liquid::DepthAndPrice, swap_cache::SwapResult},
    graph::{EdgeData, TokenPath, TopologyGraph, INLINE_EDGES},
    types::ComponentId,
};

/// What one pool paid for one input amount, on one token pair.
///
/// Holds no pool state and no component: a candidate is scored to compare it, and only the winner
/// is built into swaps.
#[derive(Clone)]
pub(crate) struct HopResult {
    /// Where the pool these numbers came from sits in the pair's pool list.
    pub(crate) pool_ix: usize,
    /// What that pool paid out, before gas.
    pub(crate) amount_out: BigUint,
    /// What that swap costs in gas, in wei.
    pub(crate) gas: BigUint,
}

/// What one pool pays for an amount, and what that is worth once its own gas is taken off.
///
/// The net is what the choice is made on, and it is the caller's to compute: only the caller knows
/// what gas costs in the token this leg pays out.
pub(crate) struct PoolQuote {
    pub(crate) paid: SwapResult,
    pub(crate) net: BigInt,
}

/// Which leg of a token path had no pool able to trade what reached it.
///
/// Carries the leg's position, which the caller turns into whatever its own error wants to say.
pub(crate) struct FailedLegIx(pub(crate) usize);

/// The pool on `pools` that pays best net of gas, or `None` if none of them can serve the amount.
///
/// `usable` withholds pools the caller will not accept — the path has already crossed them.
fn best_paying_pool<'g, D>(
    pools: &'g [EdgeData<D>],
    mut usable: impl FnMut(&ComponentId) -> bool,
    mut simulate: impl FnMut(&'g ComponentId) -> Option<PoolQuote>,
) -> Option<(HopResult, BigInt)> {
    let mut best: Option<(HopResult, BigInt)> = None;
    for (pool_ix, edge) in pools.iter().enumerate() {
        if !usable(&edge.component_id) {
            continue;
        }
        let Some(quote) = simulate(&edge.component_id) else {
            trace!(component_id = edge.component_id, "simulation failed, skipping pool");
            continue;
        };
        // Strictly greater, so the first pool on the leg keeps a tie.
        if best
            .as_ref()
            .is_none_or(|(_, best_net)| &quote.net > best_net)
        {
            let hop = HopResult { pool_ix, amount_out: quote.paid.amount_out, gas: quote.paid.gas };
            best = Some((hop, quote.net));
        }
    }
    best
}

/// One leg of a token path: the token pair, every pool that trades it in that direction, and
/// whatever the caller needs to answer a swap on it.
///
/// `data` is the caller's own — `most_liquid` puts the leg's two tokens and its gas price there —
/// so the closure [`simulate_token_path`] calls is handed the leg itself rather than an index into
/// an array it does not own.
pub(crate) struct LegPools<'g, D, T> {
    pub(crate) pair: (NodeIndex, NodeIndex),
    pub(crate) pools: &'g [EdgeData<D>],
    pub(crate) data: T,
}

/// What an unsplit route through a token path paid, and the pools it went through.
///
/// The output is gross. Turning it into a figure net of gas needs the price of gas in the path's
/// output token, which is the caller's to hold.
pub(crate) struct WalkedPath {
    pub(crate) hops: SmallVec<[HopResult; INLINE_EDGES]>,
    pub(crate) amount_out: BigUint,
    pub(crate) gas: BigUint,
}

/// Which pool won each token pair, for one order only.
///
/// Paths share pairs: every path through `WBTC -> WETH` asks the same pools the same question. What
/// each pool *paid* is not kept here — that belongs in the caller's swap cache, keyed by pool, so
/// two nearby amounts on one pool's curve can be read across. Keeping amounts per pair could not do
/// that: a later amount can be won by a different pool, and a line drawn between two amounts won by
/// two pools joins two unrelated curves.
///
/// What is left is the choice, which is the part worth reusing: try the pool that won this pair
/// last before asking every pool on it again.
///
/// Remembering nothing is a valid state: [`PairWinners::new`] takes a flag, and with it off every
/// leg asks every pool, which is the answer this is an approximation of.
pub(crate) struct PairWinners {
    /// The pool that won the pair on the one full scan of it.
    ///
    /// Recorded only on a full scan, which is the first path to reach the pair — later ones take
    /// the remembered winner and never ask the rest. A narrowed scan, where the path has
    /// already crossed some of these pools, is not recorded: the best of a field with pools
    /// removed from it is not this pair's answer.
    winner_by_pair: FxHashMap<(NodeIndex, NodeIndex), usize>,
    /// Whether the pool that won a pair is tried first on later paths.
    reuse_winners: bool,
}

impl PairWinners {
    pub(crate) fn new(reuse_winners: bool) -> Self {
        Self { winner_by_pair: FxHashMap::default(), reuse_winners }
    }

    /// Chooses which pool on the pair to go through, and returns what that pool paid.
    ///
    /// Tries the pool that won this pair before asking every pool. Asking is the caller's
    /// `simulate` closure; nothing here trades.
    ///
    /// Returns `None` when no pool on the pair can trade the amount.
    fn choose_pool_for_pair<'g, D>(
        &mut self,
        pair: (NodeIndex, NodeIndex),
        pools: &'g [EdgeData<D>],
        mut simulate: impl FnMut(&'g ComponentId) -> Option<PoolQuote>,
    ) -> Option<HopResult> {
        // The winner is assumed to hold at a nearby amount. Where it does not, it still pays what
        // it pays, and the next full scan of this pair moves the choice on.
        if self.reuse_winners {
            if let Some(&pool_ix) = self.winner_by_pair.get(&pair) {
                if let Some(quote) = pools
                    .get(pool_ix)
                    .and_then(|edge| simulate(&edge.component_id))
                {
                    return Some(HopResult {
                        pool_ix,
                        amount_out: quote.paid.amount_out,
                        gas: quote.paid.gas,
                    });
                }
            }
        }

        let (hop, _) = best_paying_pool(pools, |_| true, simulate)?;
        if self.reuse_winners {
            self.winner_by_pair
                .insert(pair, hop.pool_ix);
        }
        Some(hop)
    }
}

/// Walks `legs` in order, selling `amount_in` through the pool that pays best at each one.
///
/// `simulate` is asked what one pool pays for one amount on one leg; it receives the leg, so the
/// caller can reach whatever it hung on [`LegPools::data`]. Returning `None` from it withholds the
/// pool.
///
/// `Err` when some leg has no pool that can trade what reached it.
pub(crate) fn simulate_token_path<'g, D, T>(
    legs: &[LegPools<'g, D, T>],
    amount_in: &BigUint,
    winners: &mut PairWinners,
    mut simulate: impl FnMut(&LegPools<'g, D, T>, &BigUint, &'g ComponentId) -> Option<PoolQuote>,
) -> Result<WalkedPath, FailedLegIx> {
    let mut amount = amount_in.clone();
    let mut gas = BigUint::ZERO;
    let mut hops: SmallVec<[HopResult; INLINE_EDGES]> = SmallVec::new();
    let mut crossed: SmallVec<[&'g ComponentId; INLINE_EDGES]> = SmallVec::new();

    for (leg_ix, leg) in legs.iter().enumerate() {
        let at_amount = amount.clone();
        let simulate_pool = |component_id: &'g ComponentId| simulate(leg, &at_amount, component_id);

        // A pool this path already crossed cannot be offered again. Where that bites, the pool is
        // picked here and the remembered winner is left alone in both directions: what it holds was
        // chosen over every pool, and the best of a narrowed field is not the answer the next path
        // to ask this pair at this amount should be handed.
        let narrowed = leg
            .pools
            .iter()
            .any(|edge| crossed.contains(&&edge.component_id));
        let hop_result = if narrowed {
            best_paying_pool(leg.pools, |id| !crossed.contains(&id), simulate_pool)
                .map(|(hop, _)| hop)
        } else {
            winners.choose_pool_for_pair(leg.pair, leg.pools, simulate_pool)
        };
        let Some(hop_result) = hop_result else {
            return Err(FailedLegIx(leg_ix));
        };

        let chosen = leg
            .pools
            .get(hop_result.pool_ix)
            .expect("the chosen pool came from this leg's own list");
        crossed.push(&chosen.component_id);

        gas += &hop_result.gas;
        amount = hop_result.amount_out.clone();
        hops.push(hop_result);
    }

    Ok(WalkedPath { hops, amount_out: amount, gas })
}

/// A token path's score before any pool is asked anything, from the graph's own edge weights.
///
/// Each leg is read at its best pool: the highest spot price any pool on it quotes, and the depth
/// of its deepest pool. The score is the product of the leg prices times the depth of the thinnest
/// leg, which is an upper bound on what any single route through the path could pay.
///
/// This is the cheap half of ranking: it costs a walk over edge weights, and it is what narrows a
/// field of candidates down to the few worth simulating. `None` when the path is too short to have
/// a leg, or when some leg has no pool at all.
///
/// A leg whose pools carry no measurement is not fatal. It leaves the price alone and takes the
/// depth to zero, so the path sinks to the bottom of the ranking rather than out of it — an
/// unmeasured leg is unknown, not known to be unusable.
fn heuristic_score(graph: &TopologyGraph<DepthAndPrice>, token_path: &[NodeIndex]) -> Option<f64> {
    if token_path.len() < 2 {
        return None;
    }

    let mut price = 1.0;
    let mut min_depth = f64::MAX;

    for pair in token_path.windows(2) {
        let pools = graph.pools_between(pair[0], pair[1]);
        if pools.is_empty() {
            return None;
        }

        let mut best_price = f64::MIN;
        let mut best_depth = f64::MIN;
        for pool in pools {
            if let Some(data) = pool.data.as_ref() {
                best_price = best_price.max(data.spot_price);
                best_depth = best_depth.max(data.depth);
            }
        }

        if best_price == f64::MIN {
            // Nothing measured on this leg. Neutral on price, thinnest possible on depth, so the
            // path sinks to the bottom of the ranking rather than out of it.
            min_depth = 0.0;
        } else {
            price *= best_price;
            min_depth = min_depth.min(best_depth);
        }
    }

    Some(price * min_depth)
}

/// `token_paths` ranked best-first by [`heuristic_score`], dropping any the score rejects.
pub(crate) fn rank_by_heuristic(
    graph: &TopologyGraph<DepthAndPrice>,
    token_paths: Vec<TokenPath>,
) -> Vec<(TokenPath, f64)> {
    let mut scored: Vec<(TokenPath, f64)> = token_paths
        .into_iter()
        .filter_map(|path| {
            let score = heuristic_score(graph, &path)?;
            Some((path, score))
        })
        .collect();
    scored.sort_by(|(_, left), (_, right)| right.total_cmp(left));
    scored
}

#[cfg(test)]
mod tests {
    use num_bigint::BigInt;
    use tycho_simulation::tycho_core::models::Address;

    use super::*;
    use crate::{
        algorithm::test_utils::fixtures::{addrs, linear_graph},
        graph::GraphManager,
    };

    // ==================== heuristic_score Tests ====================

    /// A hop with no measured pool cannot be placed, so the sequence sinks to the bottom of the
    /// queue rather than out of it: it still scores, and the score is the lowest there is.
    #[test]
    fn test_heuristic_score_sinks_a_sequence_with_an_unmeasured_hop() {
        let (a, b, c, _) = addrs();
        let mut manager = linear_graph();
        // A->B measured, B->C left without derived data.
        manager
            .set_pool_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(2.0, 1000.0), false)
            .unwrap();
        let graph = manager.graph();
        let node = |address: &Address| graph.get_token_ix(address).unwrap();

        let unmeasured = heuristic_score(graph, &[node(&a), node(&b), node(&c)]);

        assert_eq!(unmeasured, Some(0.0), "an unmeasured hop scores zero, not None");
        assert!(
            heuristic_score(graph, &[node(&a), node(&b)]).is_some_and(|measured| measured > 0.0),
            "a fully measured sequence still outranks it"
        );
    }

    /// A sequence naming a pair the graph has no pool for is the two indexes disagreeing, not a
    /// routing outcome, so it is dropped rather than ranked.
    #[test]
    fn test_heuristic_score_drops_a_sequence_with_an_unconnected_pair() {
        let (a, _, c, _) = addrs();
        let manager = linear_graph();
        let graph = manager.graph();
        let node = |address: &Address| graph.get_token_ix(address).unwrap();

        assert_eq!(heuristic_score(graph, &[node(&a), node(&c)]), None);
        assert_eq!(heuristic_score(graph, &[node(&a)]), None);
    }

    // ==================== PairWinners Tests ====================

    /// `count` pools on one pair, named `pool0`, `pool1`, ... for [`simulator`] to answer as.
    fn pools(count: usize) -> Vec<EdgeData<()>> {
        (0..count)
            .map(|i| EdgeData::new(format!("pool{i}")))
            .collect()
    }

    /// Answers as pool `i` would: out is `amount_in * multipliers[i]`, gas is flat.
    fn simulator(
        multipliers: [u64; 2],
        amount_in: u64,
    ) -> impl FnMut(&ComponentId) -> Option<PoolQuote> {
        move |component_id: &ComponentId| {
            let index: usize = component_id
                .trim_start_matches("pool")
                .parse()
                .ok()?;
            let amount = BigUint::from(amount_in * multipliers[index]);
            let paid = SwapResult { amount_out: amount.clone(), gas: BigUint::from(10u64) };
            Some(PoolQuote { paid, net: BigInt::from(amount) })
        }
    }

    /// The one token pair every case works on.
    fn pair() -> (NodeIndex, NodeIndex) {
        (NodeIndex::new(0), NodeIndex::new(1))
    }

    #[test]
    fn test_winners_takes_the_pool_that_pays_most() {
        let mut winners = PairWinners::new(true);
        let multipliers = [2u64, 5u64];
        let pools = pools(multipliers.len());

        let outcome = winners
            .choose_pool_for_pair(pair(), &pools, simulator(multipliers, 100))
            .unwrap();

        assert_eq!(outcome.pool_ix, 1, "pool1 pays 500 against pool0's 200");
        assert_eq!(outcome.amount_out, BigUint::from(500u64));
    }

    /// Once a pair has a winner, only that pool is asked. Whether the ask reaches the pool or is
    /// answered from an amount already asked is the swap cache's business, not this one's.
    #[test]
    fn test_winners_asks_only_the_remembered_winner() {
        let mut winners = PairWinners::new(true);
        let multipliers = [2u64, 5u64];
        let pools = pools(multipliers.len());
        winners
            .choose_pool_for_pair(pair(), &pools, simulator(multipliers, 100))
            .unwrap();

        let mut asked = Vec::new();
        let outcome = winners
            .choose_pool_for_pair(pair(), &pools, |component_id: &ComponentId| {
                asked.push(component_id.clone());
                simulator(multipliers, 100)(component_id)
            })
            .unwrap();

        assert_eq!(asked, vec!["pool1".to_string()], "only the winner should be asked");
        assert_eq!(outcome.pool_ix, 1);
    }

    /// A winner that cannot trade the amount does not end the hop: every pool is asked, and the
    /// one that answers becomes the pair's winner.
    #[test]
    fn test_winners_falls_back_to_every_pool_when_the_winner_cannot_trade() {
        let mut winners = PairWinners::new(true);
        let pools = pools(2);
        winners
            .choose_pool_for_pair(pair(), &pools, simulator([2, 5], 100))
            .unwrap();

        let outcome = winners
            .choose_pool_for_pair(pair(), &pools, |component_id: &ComponentId| {
                (component_id == "pool0").then(|| {
                    let amount = BigUint::from(50u64);
                    PoolQuote {
                        paid: SwapResult { amount_out: amount.clone(), gas: BigUint::from(10u64) },
                        net: BigInt::from(amount),
                    }
                })
            })
            .unwrap();

        assert_eq!(outcome.pool_ix, 0, "pool1 refused, so pool0 takes the pair");
        assert_eq!(outcome.amount_out, BigUint::from(50u64));
    }

    /// Off, no winner is remembered, so every pool is asked every time.
    #[test]
    fn test_winners_disabled_scans_every_pool_each_time() {
        let mut winners = PairWinners::new(false);
        let multipliers = [2u64, 5u64];
        let pools = pools(multipliers.len());

        let first = winners
            .choose_pool_for_pair(pair(), &pools, simulator(multipliers, 100))
            .unwrap();
        let mut asked = 0usize;
        let second = winners
            .choose_pool_for_pair(pair(), &pools, |component_id: &ComponentId| {
                asked += 1;
                simulator(multipliers, 100)(component_id)
            })
            .unwrap();

        assert_eq!(asked, 2, "both pools are asked again, so no winner was remembered");
        assert_eq!(first.amount_out, second.amount_out);
    }

    /// No pool on the pair can trade the amount.
    #[test]
    fn test_winners_returns_none_when_no_pool_trades() {
        let mut winners = PairWinners::new(true);
        let pools = pools(2);

        let outcome = winners.choose_pool_for_pair(pair(), &pools, |_| None);

        assert!(outcome.is_none());
    }

    // ==================== simulate_token_path Tests ====================

    /// Two legs over the same pool list, so the pool that wins the first leg is on offer again at
    /// the second.
    fn two_legs_sharing_pools(pools: &[EdgeData<()>]) -> Vec<LegPools<'_, (), ()>> {
        vec![
            LegPools { pair: (NodeIndex::new(0), NodeIndex::new(1)), pools, data: () },
            LegPools { pair: (NodeIndex::new(1), NodeIndex::new(2)), pools, data: () },
        ]
    }

    /// A pool the path already crossed cannot be sold through twice, so the second leg takes the
    /// pool that pays less rather than the one that won the first.
    #[test]
    fn test_simulate_token_path_withholds_a_pool_the_path_already_crossed() {
        let pools = pools(2);
        let legs = two_legs_sharing_pools(&pools);
        let mut winners = PairWinners::new(true);

        let walked =
            simulate_token_path(&legs, &BigUint::from(100u64), &mut winners, |_, amount, id| {
                let multiplier = if id == "pool1" { 5u64 } else { 2u64 };
                let out = amount * BigUint::from(multiplier);
                Some(PoolQuote {
                    paid: SwapResult { amount_out: out.clone(), gas: BigUint::from(10u64) },
                    net: BigInt::from(out),
                })
            })
            .ok()
            .expect("both legs have a pool left to trade");

        let crossed: Vec<usize> = walked
            .hops
            .iter()
            .map(|hop| hop.pool_ix)
            .collect();
        assert_eq!(crossed, vec![1, 0], "the second leg cannot reuse pool1");
        assert_eq!(walked.amount_out, BigUint::from(1000u64), "100 * 5 through pool1, then * 2");
    }

    /// A narrowed scan picks the best of a field with pools removed from it, which is not the
    /// pair's own answer, so it must not overwrite what a full scan recorded.
    #[test]
    fn test_simulate_token_path_leaves_the_remembered_winner_after_a_narrowed_scan() {
        let pools = pools(2);
        let legs = two_legs_sharing_pools(&pools);
        let mut winners = PairWinners::new(true);
        let quote = |amount: &BigUint, multiplier: u64| {
            let out = amount * BigUint::from(multiplier);
            Some(PoolQuote {
                paid: SwapResult { amount_out: out.clone(), gas: BigUint::from(10u64) },
                net: BigInt::from(out),
            })
        };

        simulate_token_path(&legs, &BigUint::from(100u64), &mut winners, |_, amount, id| {
            quote(amount, if id == "pool1" { 5 } else { 2 })
        })
        .ok()
        .expect("both legs have a pool left to trade");

        assert_eq!(
            winners
                .winner_by_pair
                .get(&(NodeIndex::new(1), NodeIndex::new(2))),
            None,
            "the second leg scanned a narrowed field, so it recorded nothing"
        );
        assert_eq!(
            winners
                .winner_by_pair
                .get(&(NodeIndex::new(0), NodeIndex::new(1))),
            Some(&1),
            "the first leg scanned every pool, so its winner stands"
        );
    }

    /// A leg no pool can trade names itself, so the caller can say which hop stopped the path.
    #[test]
    fn test_simulate_token_path_names_the_leg_that_could_not_trade() {
        let pools = pools(2);
        let legs = two_legs_sharing_pools(&pools);
        let mut winners = PairWinners::new(true);

        let failed =
            simulate_token_path(&legs, &BigUint::from(100u64), &mut winners, |leg, amount, _| {
                (leg.pair.0 == NodeIndex::new(0)).then(|| {
                    let out = amount * BigUint::from(2u64);
                    PoolQuote {
                        paid: SwapResult { amount_out: out.clone(), gas: BigUint::from(10u64) },
                        net: BigInt::from(out),
                    }
                })
            })
            .err()
            .expect("the second leg has no pool that trades");

        assert_eq!(failed.0, 1);
    }
}
