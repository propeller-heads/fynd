//! Replaces parts of a route with one swap through an exclusive component.

use async_trait::async_trait;
use num_bigint::BigInt;
use tracing::debug;

use crate::{
    algorithm::paths::read_market,
    exclusive_route::{
        exclusive_route_shape, matched_commitment, routes_through_exclusive, ExclusiveRouteShape,
        RouteOutput, USER_IMPROVEMENT_SHARE_BPS,
    },
    feed::{
        events::{EventError, MarketEvent, MarketEventHandler},
        exclusivity::is_exclusive,
        market_data::MarketData,
    },
    post_process::{
        replace::{
            build_index, can_replace, replace_with_swap, update_index, CandidateRoute,
            ReplacementComponentIndex, ReplacementFinder, ReplacementLiquidity, ReplacementSolver,
            ReplacementTarget, RouteEvaluator,
        },
        PostProcessRequest, RoutePostProcessor,
    },
    types::{RouteResult, Swap},
    worker_pool_router::ExclusiveAccess,
};

/// Replaces one swap or a segment of a sell order's route with one swap through an exclusive
/// component, or splits one swap or a segment between its swaps and the exclusive component. It
/// keeps a replacement only when the net amount out rises and the route can commit to a price.
///
/// The route gets at most one exclusive leg, and a route that already has one does not change. It
/// acts only on a `public_only` pool, for a request with exclusive access: a pool that includes
/// exclusive liquidity already routes through it.
///
/// The result commits to the amount the router would commit to for an exclusive route against the
/// route it replaces: see `matched_commitment`. A replacement the finder rejects is dropped.
#[derive(Default)]
pub(crate) struct ExclusiveSwapReplacer {
    market: Option<MarketData>,
    index: ReplacementComponentIndex,
}

impl ExclusiveSwapReplacer {
    /// The name that a pool configuration lists to run this post-processor.
    pub(crate) const NAME: &'static str = "exclusive_swap_replacer";
}

#[async_trait]
impl MarketEventHandler for ExclusiveSwapReplacer {
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError> {
        let Some(market) = &self.market else {
            return Ok(());
        };
        update_index(&mut self.index, market, event, is_exclusive).await;
        Ok(())
    }
}

#[async_trait]
impl RoutePostProcessor for ExclusiveSwapReplacer {
    async fn initialize(&mut self, market: &MarketData) {
        self.index = build_index(market, is_exclusive).await;
        self.market = Some(market.clone());
    }

    async fn process(
        &self,
        request: &PostProcessRequest<'_>,
        route: &RouteResult,
    ) -> Option<RouteResult> {
        if request.exclusive_access() != ExclusiveAccess::Granted ||
            self.index.is_empty() ||
            !request.order().is_sell() ||
            routes_through_exclusive(route.route().swaps())
        {
            return None;
        }
        let liquidity = match read_market(request.market(), request.label().cloned()).await {
            Ok(view) => ReplacementLiquidity::from_market_for_route(
                &self.index,
                &view,
                request,
                route.route(),
            ),
            Err(error) => {
                debug!(%error, "cannot read the market; keeping the route");
                return None;
            }
        };
        ReplacementSolver::new(liquidity, request, route, ExclusiveReplacementFinder)
            .and_then(ReplacementSolver::apply_best_replacements)
    }
}

/// Replaces a target with one swap through an exclusive component, or splits a target, one swap or
/// a segment, between its swaps and the exclusive component.
struct ExclusiveReplacementFinder;

impl ReplacementFinder for ExclusiveReplacementFinder {
    /// An exclusive component fills partially, so it accepts a target at any index of the route. It
    /// does not accept a route that already has an exclusive leg.
    fn can_replace_target(
        &self,
        swap: &Swap,
        target: &ReplacementTarget,
        route_swaps: &[Swap],
        _evaluator: &RouteEvaluator<'_>,
    ) -> bool {
        !routes_through_exclusive(route_swaps) && can_replace(swap, target, route_swaps)
    }

    fn find_best_replacement_route(
        &self,
        swap: &Swap,
        target: &ReplacementTarget,
        route_swaps: &[Swap],
        evaluator: &RouteEvaluator<'_>,
        min_net_amount_out: &BigInt,
    ) -> Option<CandidateRoute> {
        let full_replacement =
            replace_with_swap(swap, target, route_swaps, evaluator, min_net_amount_out)
                .and_then(|route| ExclusiveReplacementFinder.commit(route, evaluator));
        let min_split_net_amount_out = full_replacement
            .as_ref()
            .map_or(min_net_amount_out, CandidateRoute::net_amount_out);
        let split_replacement =
            SplitSearcher::new(swap, target, route_swaps, evaluator, min_split_net_amount_out)
                .and_then(SplitSearcher::find_best_split_route);
        split_replacement.or(full_replacement)
    }
}

impl ExclusiveReplacementFinder {
    /// Commits `candidate` to the amount that matches the original route. `None` when the route
    /// has no valid exclusive leg, or the commitment rule rejects it: the candidate is below the
    /// original route net of gas or in amount out.
    fn commit(
        &self,
        candidate: CandidateRoute,
        evaluator: &RouteEvaluator<'_>,
    ) -> Option<CandidateRoute> {
        if exclusive_route_shape(candidate.swaps(), evaluator.output_token()) !=
            ExclusiveRouteShape::Valid
        {
            return None;
        }
        let original_net_amount_out = evaluator
            .original_net_amount_out()
            .to_biguint()
            .unwrap_or_default();
        let candidate_net_amount_out = candidate
            .net_amount_out()
            .to_biguint()
            .unwrap_or_default();
        let committed_amount_out = matched_commitment(
            RouteOutput {
                amount_out: evaluator.original_amount_out(),
                amount_out_net_gas: &original_net_amount_out,
            },
            RouteOutput {
                amount_out: candidate.amount_out(),
                amount_out_net_gas: &candidate_net_amount_out,
            },
            *USER_IMPROVEMENT_SHARE_BPS,
        )?;
        Some(candidate.with_commitment(committed_amount_out))
    }
}

/// The number of golden-section steps for the exclusive fraction of a split. The search simulates
/// the route `SPLIT_SEARCH_STEPS + 2` times.
const SPLIT_SEARCH_STEPS: usize = 8;

/// The ratio of the golden section: `1 / phi`.
const GOLDEN_RATIO_INVERSE: f64 = 0.618_033_988_749_895;

/// A swap of the target that takes `token_in` of the target, with its split in the route.
struct EntrySwapSplit {
    /// The index of the swap in `SplitSearcher::split_swaps`.
    swap_index: usize,
    /// The split of the swap in the route; `0.0` takes what remains of the balance.
    original_split: f64,
}

/// The part of the balance of the target's input token that the entry swaps hold: the sum of their
/// splits, or what the swaps outside the target leave when one of them takes the remainder.
fn entries_share(
    entries: &[EntrySwapSplit],
    target: &ReplacementTarget,
    route_swaps: &[Swap],
) -> f64 {
    if entries
        .iter()
        .all(|entry| entry.original_split > 0.0)
    {
        let mut share = 0.0;
        for entry in entries {
            share += entry.original_split;
        }
        return share;
    }
    let mut others = 0.0;
    for (index, route_swap) in route_swaps.iter().enumerate() {
        if !target.replaced_indices.contains(&index) && *route_swap.token_in() == target.token_in {
            others += *route_swap.split();
        }
    }
    1.0 - others
}

/// Finds the share of a target that the exclusive pool should take. The target is one swap or a
/// segment: the exclusive swap takes a fraction of the share of the input token that the target
/// holds, and the swaps of the target keep the rest. It assumes the net amount out has one peak
/// over the exclusive fraction, and it counts a fraction that the exclusive component cannot fill
/// as the lowest value.
struct SplitSearcher<'a> {
    target: &'a ReplacementTarget,
    swaps: &'a [Swap],
    evaluator: &'a RouteEvaluator<'a>,
    /// The exclusive swap, then the swaps of the target in route order.
    split_swaps: Vec<Swap>,
    /// The swaps of the target that take the input token of the target.
    entries: Vec<EntrySwapSplit>,
    /// The part of the input token balance that the target holds.
    share: f64,
    /// The candidate with the best net amount out so far, above `min_split_net_amount_out`.
    best: Option<CandidateRoute>,
    min_split_net_amount_out: &'a BigInt,
}

impl<'a> SplitSearcher<'a> {
    /// Prepares a split search for `target`, with `swap` as the exclusive swap. `None` when the
    /// target holds no part of the input token balance. A split must have a net amount out above
    /// `min_split_net_amount_out`.
    fn new(
        swap: &Swap,
        target: &'a ReplacementTarget,
        route_swaps: &'a [Swap],
        evaluator: &'a RouteEvaluator<'a>,
        min_split_net_amount_out: &'a BigInt,
    ) -> Option<Self> {
        let mut split_swaps = vec![swap.clone()];
        let mut entries = Vec::new();
        for index in &target.replaced_indices {
            let original = route_swaps.get(*index)?;
            if *original.token_in() == target.token_in {
                entries.push(EntrySwapSplit {
                    swap_index: split_swaps.len(),
                    original_split: *original.split(),
                });
            }
            split_swaps.push(original.clone());
        }
        let share = entries_share(&entries, target, route_swaps);
        if share <= 0.0 {
            return None;
        }
        Some(Self {
            target,
            swaps: route_swaps,
            evaluator,
            split_swaps,
            entries,
            share,
            best: None,
            min_split_net_amount_out,
        })
    }

    /// Returns the split with the highest net amount out above `min_split_net_amount_out`, if any.
    fn find_best_split_route(mut self) -> Option<CandidateRoute> {
        let (mut min_fraction, mut max_fraction) = (0.0_f64, 1.0_f64);
        let mut lower_fraction =
            max_fraction - GOLDEN_RATIO_INVERSE * (max_fraction - min_fraction);
        let mut upper_fraction =
            min_fraction + GOLDEN_RATIO_INVERSE * (max_fraction - min_fraction);
        let mut lower_net = self.simulate_split(lower_fraction);
        let mut upper_net = self.simulate_split(upper_fraction);
        for _ in 0..SPLIT_SEARCH_STEPS {
            // A fraction that cannot be filled has no net amount out: `None` orders below every
            // `Some`, so the search moves away from it.
            if lower_net >= upper_net {
                max_fraction = upper_fraction;
                (upper_fraction, upper_net) = (lower_fraction, lower_net);
                lower_fraction =
                    max_fraction - GOLDEN_RATIO_INVERSE * (max_fraction - min_fraction);
                lower_net = self.simulate_split(lower_fraction);
            } else {
                min_fraction = lower_fraction;
                (lower_fraction, lower_net) = (upper_fraction, upper_net);
                upper_fraction =
                    min_fraction + GOLDEN_RATIO_INVERSE * (max_fraction - min_fraction);
                upper_net = self.simulate_split(upper_fraction);
            }
        }
        let candidate = self.best.take()?;
        ExclusiveReplacementFinder.commit(candidate, self.evaluator)
    }

    /// Simulates the split at `exclusive_fraction` and records it if it is the best so far.
    /// Returns its net amount out, or `None` when the simulation fails or the deadline has passed.
    fn simulate_split(&mut self, exclusive_fraction: f64) -> Option<BigInt> {
        if self.evaluator.deadline_passed() {
            return None;
        }
        self.retarget_split_swaps(exclusive_fraction);
        let candidate =
            self.evaluator
                .simulate_replacement(self.swaps, self.target, &self.split_swaps)?;
        let net_amount_out = candidate.net_amount_out().clone();
        let best_net = self
            .best
            .as_ref()
            .map_or(self.min_split_net_amount_out, CandidateRoute::net_amount_out);
        if net_amount_out > *best_net {
            self.best = Some(candidate);
        }
        Some(net_amount_out)
    }

    /// Gives the exclusive swap the fraction `exclusive_fraction` of the share of the target. Each
    /// entry swap keeps the rest of its split, or takes the remainder when it did before. Swaps
    /// after the entry swaps keep their splits: they route what reaches them.
    fn retarget_split_swaps(&mut self, exclusive_fraction: f64) {
        let (token_in, token_out) = (&self.target.token_in, &self.target.token_out);
        if let Some(exclusive) = self.split_swaps.first_mut() {
            exclusive.retarget(
                token_in.clone(),
                token_out.clone(),
                self.share * exclusive_fraction,
            );
        }
        for entry in &self.entries {
            if entry.original_split <= 0.0 {
                continue;
            }
            let Some(original) = self
                .split_swaps
                .get_mut(entry.swap_index)
            else {
                continue;
            };
            let (original_in, original_out) =
                (original.token_in().clone(), original.token_out().clone());
            original.retarget(
                original_in,
                original_out,
                entry.original_split * (1.0 - exclusive_fraction),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use num_bigint::{BigInt, BigUint};
    use rstest::rstest;
    use tycho_simulation::tycho_common::{
        models::{protocol::ProtocolComponent, token::Token},
        simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component_with_protocol, ConstantProductSim, MockProtocolSim},
        derived::DerivedData,
        exclusive_route::is_exclusive_leg,
        feed::exclusivity::mark_exclusive,
        post_process::test_utils::{
            add_component, added, component_ids, insert_component, market, replaced, result,
            run_with_access, sell_order, sequential_route, swap, swap_on, swap_through_pool,
            tokens, with_net_amount_out,
        },
        types::{Order, ResolvedRouteConstraints},
    };

    async fn run_granted(
        replacer: &ExclusiveSwapReplacer,
        market: &MarketData,
        order: &Order,
        route: RouteResult,
    ) -> Option<RouteResult> {
        run_with_access(replacer, market, order, ExclusiveAccess::Granted, route).await
    }

    fn exclusive_component(id: &str, pair: (&Token, &Token)) -> ProtocolComponent {
        let mut component =
            component_with_protocol(id, "exclusive:ekubo_v3", &[pair.0.clone(), pair.1.clone()]);
        mark_exclusive(&mut component);
        component
    }

    async fn add_exclusive(
        market: &MarketData,
        id: &str,
        pair: (&Token, &Token),
        sim: impl ProtocolSim + 'static,
    ) {
        insert_component(market, exclusive_component(id, pair), &[pair.0, pair.1], sim).await;
    }

    /// A swap of the route that goes through an exclusive component.
    fn exclusive_swap(id: &str, pair: (&Token, &Token), amounts: (u64, u64)) -> Swap {
        swap_through_pool(
            exclusive_component(id, pair),
            pair,
            amounts,
            0.0,
            MockProtocolSim::new(2.0).with_gas(10),
        )
    }

    async fn replacer(market: &MarketData) -> ExclusiveSwapReplacer {
        let mut replacer = ExclusiveSwapReplacer::default();
        replacer.initialize(market).await;
        replacer
    }

    #[rstest]
    #[case::first_hop("excl_ab", (0, 1), 3.0, 10, &["excl_ab", "bc"], Some(6000), 5980)]
    #[case::second_hop("excl_bc", (1, 2), 3.0, 10, &["ab", "excl_bc"], None, 5980)]
    #[case::whole_order("excl_ac", (0, 2), 5.0, 5, &["excl_ac"], None, 4995)]
    #[tokio::test]
    async fn test_exclusive_whole_replacement(
        #[case] id: &str,
        #[case] ends: (usize, usize),
        #[case] price: f64,
        #[case] gas: u64,
        #[case] expected_ids: &[&str],
        #[case] downstream_amount_out: Option<u64>,
        #[case] expected_net: i64,
    ) {
        let (a, b, c) = tokens();
        let held = [&a, &b, &c];
        let market = market();
        let sim = MockProtocolSim::new(price).with_gas(gas);
        add_exclusive(&market, id, (held[ends.0], held[ends.1]), sim).await;
        let replacer = replacer(&market).await;

        let replaced_route = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );

        assert_eq!(component_ids(&replaced_route), expected_ids);
        if let Some(amount_out) = downstream_amount_out {
            assert_eq!(replaced_route.route().swaps()[1].amount_out(), &BigUint::from(amount_out));
        }
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(expected_net));
    }

    #[rstest]
    #[case::granted(ExclusiveAccess::Granted, true)]
    #[case::denied(ExclusiveAccess::Denied, false)]
    #[tokio::test]
    async fn test_exclusive_requires_access(
        #[case] access: ExclusiveAccess,
        #[case] replaces: bool,
    ) {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let replacement =
            run_with_access(&replacer, &market, &sell_order(&a, &c), access, sequential_route())
                .await;

        assert_eq!(replacement.is_some(), replaces);
    }

    /// The committed amount follows the user share of the improvement over the received route:
    /// 10% of 2000 on top of its net amount out of 3980, plus the 20 of gas.
    #[tokio::test]
    async fn test_exclusive_commitment() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let replaced_route = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );

        let commitment = replaced_route
            .commitment()
            .expect("the replacement commits");
        assert_eq!(commitment.committed_amount_out(), &BigUint::from(4200u64));
        assert_eq!(commitment.reference_amount_out(), &BigUint::from(4000u64));
    }

    /// The net amount out of the original route is 3000 on an amount out of 4000. A replacement
    /// that pays less than 4000 has the higher net amount out, but cannot keep the promise that
    /// the user gets at least what the received route paid. One that pays more commits to the
    /// 4000, because the user share of its improvement is below that.
    #[rstest]
    #[case::amount_out_below_the_route(3.9, None)]
    #[case::amount_out_above_the_route(4.1, Some(4000))]
    #[tokio::test]
    async fn test_exclusive_commitment_gate(#[case] price: f64, #[case] committed: Option<u64>) {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ac", (&a, &c), MockProtocolSim::new(price).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let route = with_net_amount_out(sequential_route(), 3000);
        let _ = b;

        let replacement = run_granted(&replacer, &market, &sell_order(&a, &c), route).await;

        let committed_amount_out = replacement
            .as_ref()
            .map(|replaced_route| {
                replaced_route
                    .commitment()
                    .expect("the replacement commits")
            })
            .map(|commitment| {
                commitment
                    .committed_amount_out()
                    .clone()
            });
        assert_eq!(committed_amount_out, committed.map(BigUint::from));
    }

    #[tokio::test]
    async fn test_exclusive_one_leg() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;
        add_exclusive(&market, "excl_bc", (&b, &c), MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let replaced_route = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );

        let exclusive_legs = replaced_route
            .route()
            .swaps()
            .iter()
            .filter(|swap| is_exclusive_leg(swap))
            .count();
        assert_eq!(exclusive_legs, 1);
        assert_eq!(component_ids(&replaced_route), ["excl_ab", "bc"]);
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(5980));
    }

    #[tokio::test]
    async fn test_exclusive_lower_amount_out() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(1.5).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let replacement =
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;

        assert!(replacement.is_none());
    }

    #[tokio::test]
    async fn test_exclusive_route_with_exclusive_leg() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let route = result(
            vec![
                swap("ab", (&a, &b), (1000, 2000), 0.0, 2.0),
                exclusive_swap("excl_bc", (&b, &c), (2000, 4000)),
            ],
            &[&a, &b, &c],
        );

        let replacement = run_granted(&replacer, &market, &sell_order(&a, &c), route).await;

        assert!(replacement.is_none());
    }

    #[tokio::test]
    async fn test_exclusive_ignores_public_component() {
        let (a, b, c) = tokens();
        let market = market();
        add_component(
            &market,
            "public_ab",
            "uniswap_v2",
            &[&a, &b],
            MockProtocolSim::new(3.0).with_gas(10),
        )
        .await;
        let replacer = replacer(&market).await;

        let replacement =
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;

        assert!(replacement.is_none());
    }

    #[tokio::test]
    async fn test_exclusive_component_added_after_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        let mut replacer = replacer(&market).await;
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;

        replacer
            .handle_event(&added("excl_ab", &[&a, &b]))
            .await
            .expect("adding an exclusive component");

        let replaced_route = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );
        assert_eq!(component_ids(&replaced_route), ["excl_ab", "bc"]);
    }

    #[tokio::test]
    async fn test_public_component_added_after_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        let mut replacer = replacer(&market).await;
        add_component(
            &market,
            "public_ab",
            "uniswap_v2",
            &[&a, &b],
            MockProtocolSim::new(3.0).with_gas(10),
        )
        .await;

        replacer
            .handle_event(&added("public_ab", &[&a, &b]))
            .await
            .expect("adding a public component");

        let replacement =
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;
        assert!(replacement.is_none());
    }

    #[tokio::test]
    async fn test_exclusive_event_before_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;
        let mut replacer = ExclusiveSwapReplacer::default();

        replacer
            .handle_event(&added("excl_ab", &[&a, &b]))
            .await
            .expect("an event before initialize");

        let replacement =
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;
        assert!(replacement.is_none());
    }

    fn constant_product_state(reserve_in: u64, reserve_out: u64) -> ConstantProductSim {
        ConstantProductSim {
            reserve_0: BigUint::from(reserve_in),
            reserve_1: BigUint::from(reserve_out),
            gas: 10,
        }
    }

    /// The total amount in of the swaps that start at `token`.
    fn total_amount_in_for_token(result: &RouteResult, token: &Token) -> BigUint {
        result
            .route()
            .swaps()
            .iter()
            .filter(|swap| *swap.token_in() == token.address)
            .map(|swap| swap.amount_in())
            .sum()
    }

    fn splits_of(result: &RouteResult) -> Vec<f64> {
        result
            .route()
            .swaps()
            .iter()
            .map(|swap| *swap.split())
            .collect()
    }

    /// A route of one swap from A to B through a deep pool at a price of about 2.
    fn deep_route(a: &Token, b: &Token) -> RouteResult {
        result(
            vec![swap_on(
                "ab",
                (a, b),
                (1000, 1980),
                0.0,
                constant_product_state(100_000, 200_000),
            )],
            &[a, b],
        )
    }

    #[tokio::test]
    async fn test_exclusive_split_when_price_falls_with_amount() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), constant_product_state(1000, 2500)).await;
        let replacer = replacer(&market).await;

        let replaced_route = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &b), deep_route(&a, &b)).await,
        );

        assert_eq!(component_ids(&replaced_route), ["excl_ab", "ab"]);
        assert_eq!(splits_of(&replaced_route), [0.14589803375031532, 0.0]);
        assert_eq!(total_amount_in_for_token(&replaced_route, &a), BigUint::from(1000u64));
        // The original route's net amount out is 1970. The exclusive pool alone gives 1250 minus
        // 10 gas.
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(1991));
    }

    #[tokio::test]
    async fn test_exclusive_split_limited_liquidity() {
        let (a, b, _) = tokens();
        let market = market();
        let limited_pool = MockProtocolSim::new(3.0)
            .with_gas(10)
            .with_liquidity(1500);
        add_exclusive(&market, "excl_ab", (&a, &b), limited_pool).await;
        let replacer = replacer(&market).await;
        let original = result(vec![swap("ab", (&a, &b), (1000, 2000), 0.0, 2.0)], &[&a, &b]);

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);

        assert_eq!(component_ids(&replaced_route), ["excl_ab", "ab"]);
        assert_eq!(splits_of(&replaced_route), [0.4984471899924292, 0.0]);
        assert_eq!(total_amount_in_for_token(&replaced_route, &a), BigUint::from(1000u64));
        // The original route's net amount out is 1990. The split gives about 2500 minus 20 gas.
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(2478));
    }

    #[tokio::test]
    async fn test_exclusive_split_of_a_swap_with_a_split() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), constant_product_state(1000, 2500)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap_on("p1", (&a, &b), (600, 1188), 0.6, constant_product_state(100_000, 200_000)),
                swap_on("p2", (&a, &b), (400, 792), 0.0, constant_product_state(100_000, 200_000)),
            ],
            &[&a, &b],
        );
        let original_net = original.net_amount_out().clone();

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);

        assert_eq!(component_ids(&replaced_route), ["excl_ab", "p1", "p2"]);
        assert_eq!(splits_of(&replaced_route), [0.1209756742506939, 0.4790243257493061, 0.0]);
        assert_eq!(total_amount_in_for_token(&replaced_route, &a), BigUint::from(1000u64));
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(1988));
        assert!(replaced_route.net_amount_out() > &original_net);
    }

    #[tokio::test]
    async fn test_exclusive_split_of_the_remainder_swap() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), constant_product_state(1000, 2500)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap("p1", (&a, &b), (600, 1800), 0.6, 3.0),
                swap_on("p2", (&a, &b), (400, 792), 0.0, constant_product_state(100_000, 200_000)),
            ],
            &[&a, &b],
        );
        let original_net = original.net_amount_out().clone();

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);

        assert_eq!(component_ids(&replaced_route), ["p1", "excl_ab", "p2"]);
        assert_eq!(splits_of(&replaced_route), [0.6, 0.11671842700025233, 0.0]);
        assert_eq!(total_amount_in_for_token(&replaced_route, &a), BigUint::from(1000u64));
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(2595));
        assert!(replaced_route.net_amount_out() > &original_net);
    }

    #[tokio::test]
    async fn test_exclusive_split_remainder_without_share() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), constant_product_state(1000, 2500)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap_on("p1", (&a, &b), (600, 1188), 0.6, constant_product_state(100_000, 200_000)),
                swap_on("p2", (&a, &b), (0, 0), 0.0, constant_product_state(100_000, 200_000)),
                swap_on("p3", (&a, &b), (400, 792), 0.4, constant_product_state(100_000, 200_000)),
            ],
            &[&a, &b],
        );

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);
        assert_eq!(component_ids(&replaced_route), ["excl_ab", "p1", "p2", "p3"]);
        assert_eq!(splits_of(&replaced_route), [0.1209756742506939, 0.4790243257493061, 0.0, 0.4]);
        assert_eq!(replaced_route.net_amount_out(), &BigInt::from(1978));
    }

    #[tokio::test]
    async fn test_exclusive_split_worse_price() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), constant_product_state(1000, 1500)).await;
        let replacer = replacer(&market).await;

        let replacement =
            run_granted(&replacer, &market, &sell_order(&a, &b), deep_route(&a, &b)).await;

        assert!(replacement.is_none());
    }

    /// A route from A to C through two parallel pools to B, where the second takes `second_split`,
    /// then one pool to C. Every pool is deep, at a price of about 2.
    fn parallel_segment_route(
        (a, b, c): (&Token, &Token, &Token),
        second_split: f64,
    ) -> RouteResult {
        result(
            vec![
                swap_on("p1", (a, b), (600, 1188), 0.6, constant_product_state(100_000, 200_000)),
                swap_on(
                    "p2",
                    (a, b),
                    (400, 792),
                    second_split,
                    constant_product_state(100_000, 200_000),
                ),
                swap_on("bc", (b, c), (1980, 3883), 0.0, constant_product_state(100_000, 200_000)),
            ],
            &[a, b, c],
        )
    }

    /// The exclusive pool trades A for C and holds 1000 A against `reserve_out` C. Selling all of
    /// the order to it pays 0.5 * `reserve_out`, which is below the 3883 of the route, so only a
    /// split can use it.
    #[rstest]
    #[case::remainder_entry(0.0, 0.0)]
    #[case::fixed_entries(0.4, 0.4)]
    #[tokio::test]
    async fn test_exclusive_split_of_a_segment(
        #[case] second_split: f64,
        #[case] second_share: f64,
    ) {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ac", (&a, &c), constant_product_state(1000, 5000)).await;
        let replacer = replacer(&market).await;
        let original = parallel_segment_route((&a, &b, &c), second_split);
        let original_net = original.net_amount_out().clone();

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &c), original).await);

        assert_eq!(component_ids(&replaced_route), ["excl_ac", "p1", "p2", "bc"]);
        let splits = splits_of(&replaced_route);
        let exclusive = splits[0];
        assert!(exclusive > 0.0 && exclusive < 1.0);
        assert_eq!(splits[1..], [0.6 * (1.0 - exclusive), second_share * (1.0 - exclusive), 0.0]);
        // Fixed splits floor each part, so they may leave one unit of the order unspent.
        let amount_in = total_amount_in_for_token(&replaced_route, &a);
        assert!(amount_in >= BigUint::from(999u64) && amount_in <= BigUint::from(1000u64));
        assert!(replaced_route.net_amount_out() > &original_net);
    }

    /// The segment A to C is the two swaps in a row; the exclusive swap and the first swap split
    /// the order, and the second swap routes what the first one pays.
    #[tokio::test]
    async fn test_exclusive_split_of_a_sequential_segment() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ac", (&a, &c), constant_product_state(1000, 5000)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap_on(
                    "ab",
                    (&a, &b),
                    (1000, 1980),
                    0.0,
                    constant_product_state(100_000, 200_000),
                ),
                swap_on(
                    "bc",
                    (&b, &c),
                    (1980, 3883),
                    0.0,
                    constant_product_state(100_000, 200_000),
                ),
            ],
            &[&a, &b, &c],
        );
        let original_net = original.net_amount_out().clone();

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &c), original).await);

        assert_eq!(component_ids(&replaced_route), ["excl_ac", "ab", "bc"]);
        let splits = splits_of(&replaced_route);
        assert!(splits[0] > 0.0 && splits[0] < 1.0);
        assert_eq!(splits[1..], [0.0, 0.0]);
        assert_eq!(total_amount_in_for_token(&replaced_route, &a), BigUint::from(1000u64));
        assert_eq!(
            total_amount_in_for_token(&replaced_route, &b),
            replaced_route.route().swaps()[1]
                .amount_out()
                .clone()
        );
        assert!(replaced_route.net_amount_out() > &original_net);
    }

    #[tokio::test]
    async fn test_exclusive_split_of_a_segment_worse_price() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ac", (&a, &c), constant_product_state(1000, 3000)).await;
        let replacer = replacer(&market).await;
        let original = parallel_segment_route((&a, &b, &c), 0.0);

        let replacement = run_granted(&replacer, &market, &sell_order(&a, &c), original).await;

        assert!(replacement.is_none());
    }

    #[tokio::test]
    async fn test_exclusive_split_of_a_segment_commits() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ac", (&a, &c), constant_product_state(1000, 5000)).await;
        let replacer = replacer(&market).await;
        let original = parallel_segment_route((&a, &b, &c), 0.0);
        let original_amount_out = original.route().amount_out(&c.address);

        let replaced_route =
            replaced(run_granted(&replacer, &market, &sell_order(&a, &c), original).await);

        let commitment = replaced_route
            .commitment()
            .expect("the split commits");
        assert_eq!(commitment.reference_amount_out(), &original_amount_out);
        assert!(commitment.committed_amount_out() >= &original_amount_out);
        assert!(
            commitment.committed_amount_out() <=
                &replaced_route
                    .route()
                    .amount_out(&c.address)
        );
    }

    #[tokio::test]
    async fn test_exclusive_split_of_a_segment_deadline_passed() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ac", (&a, &c), constant_product_state(1000, 5000)).await;
        let replacer = replacer(&market).await;
        let derived = DerivedData::new_shared();
        let order = sell_order(&a, &c);
        let exclusions = ResolvedRouteConstraints::default();
        let request =
            PostProcessRequest::new(&order, &market, &derived, &exclusions, Instant::now())
                .with_exclusive_access(ExclusiveAccess::Granted);

        let replacement = replacer
            .process(&request, &parallel_segment_route((&a, &b, &c), 0.0))
            .await;

        assert!(replacement.is_none());
    }
}
