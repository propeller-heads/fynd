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
            build_index, can_serve, find_whole_candidate, update_index, Candidate, ComponentIndex,
            Evaluation, ReplacementRule, RouteEvaluator, Search, Snapshot, Target,
        },
        PostProcessRequest, RoutePostProcessor,
    },
    types::{RouteResult, Swap},
    worker_pool_router::ExclusiveAccess,
};

/// Replaces one swap or a segment of a sell order's route with one swap through an exclusive
/// component, or splits one swap between its pool and the exclusive component. It keeps a
/// replacement only when the net amount out rises and the route can commit to a price.
///
/// The route gets at most one exclusive leg, and a route that already has one does not change. It
/// acts only on a `public_only` pool, for a request with exclusive access: a pool that includes
/// exclusive liquidity already routes through it.
///
/// The result commits to the amount the router would commit to for an exclusive route against the
/// route it replaces: see `matched_commitment`. A replacement the rule rejects is dropped.
#[derive(Default)]
pub(crate) struct ExclusiveSwapReplacer {
    market: Option<MarketData>,
    index: ComponentIndex,
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
        let snapshot = match read_market(request.market(), request.label().cloned()).await {
            Ok(view) => Snapshot::take(&self.index, &view, request, route.route()),
            Err(error) => {
                debug!(%error, "cannot read the market; keeping the route");
                return None;
            }
        };
        Search::new(snapshot, request, route, ExclusiveRule).and_then(Search::run)
    }
}

/// Replaces a target with one swap through an exclusive component, or splits a swap between its
/// pool and the exclusive component.
struct ExclusiveRule;

impl ReplacementRule for ExclusiveRule {
    /// An exclusive component fills partially, so it fits a target at any position. It does not
    /// fit a route that already has an exclusive leg.
    fn fits(
        &self,
        leg: &Swap,
        target: &Target,
        swaps: &[Swap],
        _evaluator: &RouteEvaluator<'_>,
    ) -> bool {
        !routes_through_exclusive(swaps) && can_serve(leg, target, swaps)
    }

    fn find_candidate(
        &self,
        leg: &Swap,
        target: &Target,
        swaps: &[Swap],
        evaluator: &RouteEvaluator<'_>,
        min_net_amount_out: &BigInt,
    ) -> Option<Candidate> {
        let whole = find_whole_candidate(leg, target, swaps, evaluator, min_net_amount_out)
            .and_then(|whole| ExclusiveRule.commit(whole, evaluator));
        let split_floor = whole
            .as_ref()
            .map_or(min_net_amount_out, Candidate::net_amount_out);
        let split =
            SplitSearch::new(leg, target, swaps, evaluator, split_floor).and_then(SplitSearch::run);
        split.or(whole)
    }
}

impl ExclusiveRule {
    /// Commits `candidate` to the amount that matches the original route. `None` when the route
    /// has no valid exclusive leg, or the commitment rule rejects it: the candidate is below the
    /// original route net of gas or in amount out.
    fn commit(&self, candidate: Candidate, evaluator: &RouteEvaluator<'_>) -> Option<Candidate> {
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

/// The part of the balance of the input token that the original swap of a split search holds.
#[derive(Clone, Copy)]
enum TargetSplit {
    /// The original swap holds this split of the input token balance.
    Fixed(f64),
    /// The original swap takes what remains of the input token balance, worth this split.
    Remainder(f64),
}

impl TargetSplit {
    fn value(self) -> f64 {
        match self {
            Self::Fixed(split) | Self::Remainder(split) => split,
        }
    }
}

/// Searches the exclusive fraction of the target split that the exclusive component takes while
/// the original pool keeps the rest. It assumes the net amount out has one peak over the exclusive
/// fraction, and it counts a fraction that the exclusive component cannot fill as the lowest
/// value.
struct SplitSearch<'a> {
    target: &'a Target,
    swaps: &'a [Swap],
    evaluator: &'a RouteEvaluator<'a>,
    /// The exclusive swap, then the original swap with the token pair of the target.
    split_swaps: [Swap; 2],
    target_split: TargetSplit,
    /// The exclusive fraction with the best evaluation so far, above `floor`.
    best: Option<(f64, Evaluation)>,
    floor: &'a BigInt,
}

impl<'a> SplitSearch<'a> {
    /// Prepares a split search for the swap at the single position of `target`, with `leg` as the
    /// exclusive swap. `None` when the target is a segment or the original swap holds no part of
    /// the input token balance. A split must have a net amount out above `floor`.
    fn new(
        leg: &Swap,
        target: &'a Target,
        swaps: &'a [Swap],
        evaluator: &'a RouteEvaluator<'a>,
        floor: &'a BigInt,
    ) -> Option<Self> {
        let [position] = target.replaced.as_slice() else {
            return None;
        };
        let original = swaps.get(*position)?;
        let target_split = if *original.split() <= 0.0 {
            let others: f64 = swaps
                .iter()
                .enumerate()
                .filter(|(other, swap)| other != position && swap.token_in() == original.token_in())
                .map(|(_, swap)| *swap.split())
                .sum();
            TargetSplit::Remainder(1.0 - others)
        } else {
            TargetSplit::Fixed(*original.split())
        };
        if target_split.value() <= 0.0 {
            return None;
        }
        Some(Self {
            target,
            swaps,
            evaluator,
            split_swaps: [leg.clone(), original.clone()],
            target_split,
            best: None,
            floor,
        })
    }

    /// Returns the split with the highest net amount out above `floor`, if any.
    fn run(mut self) -> Option<Candidate> {
        let (mut low, mut high) = (0.0_f64, 1.0_f64);
        let mut left = high - GOLDEN_RATIO_INVERSE * (high - low);
        let mut right = low + GOLDEN_RATIO_INVERSE * (high - low);
        let mut left_net = self.evaluate(left);
        let mut right_net = self.evaluate(right);
        for _ in 0..SPLIT_SEARCH_STEPS {
            // A fraction that cannot be filled has no net amount out: `None` orders below every
            // `Some`, so the search moves away from it.
            if left_net >= right_net {
                high = right;
                (right, right_net) = (left, left_net);
                left = high - GOLDEN_RATIO_INVERSE * (high - low);
                left_net = self.evaluate(left);
            } else {
                low = left;
                (left, left_net) = (right, right_net);
                right = low + GOLDEN_RATIO_INVERSE * (high - low);
                right_net = self.evaluate(right);
            }
        }
        let (exclusive_fraction, evaluation) = self.best.take()?;
        self.retarget_split_swaps(exclusive_fraction);
        let candidate = evaluation.into_candidate(self.swaps, self.target, &self.split_swaps);
        ExclusiveRule.commit(candidate, self.evaluator)
    }

    /// Simulates the split at `exclusive_fraction` and records it if it is the best so far.
    /// Returns its net amount out, or `None` when the simulation fails or the deadline has passed.
    fn evaluate(&mut self, exclusive_fraction: f64) -> Option<BigInt> {
        if self.evaluator.deadline_passed() {
            return None;
        }
        self.retarget_split_swaps(exclusive_fraction);
        let evaluation = self
            .evaluator
            .evaluate(self.swaps, self.target, &self.split_swaps)?;
        let net_amount_out = evaluation.net_amount_out().clone();
        let best_net = self
            .best
            .as_ref()
            .map_or(self.floor, |(_, best)| best.net_amount_out());
        if net_amount_out > *best_net {
            self.best = Some((exclusive_fraction, evaluation));
        }
        Some(net_amount_out)
    }

    /// Gives the exclusive swap the fraction `exclusive_fraction` of the target split. The
    /// original swap keeps the rest, or takes the remainder when it did before.
    fn retarget_split_swaps(&mut self, exclusive_fraction: f64) {
        let (token_in, token_out) = (&self.target.token_in, &self.target.token_out);
        let target_split = self.target_split.value();
        let original_split = match self.target_split {
            TargetSplit::Fixed(_) => target_split * (1.0 - exclusive_fraction),
            TargetSplit::Remainder(_) => 0.0,
        };
        let [exclusive, original] = &mut self.split_swaps;
        exclusive.retarget(token_in.clone(), token_out.clone(), target_split * exclusive_fraction);
        original.retarget(token_in.clone(), token_out.clone(), original_split);
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::{BigInt, BigUint};
    use rstest::rstest;
    use tycho_simulation::tycho_common::{
        models::{protocol::ProtocolComponent, token::Token},
        simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component_with_protocol, ConstantProductSim, MockProtocolSim},
        exclusive_route::is_exclusive_leg,
        feed::exclusivity::mark_exclusive,
        post_process::test_utils::{
            add_component, added, component_ids, insert_component, market, replaced, result,
            run_with_access, sell_order, sequential_route, swap, swap_on, swap_through_pool,
            tokens, with_net_amount_out,
        },
        types::Order,
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

        let out = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );

        assert_eq!(component_ids(&out), expected_ids);
        if let Some(amount_out) = downstream_amount_out {
            assert_eq!(out.route().swaps()[1].amount_out(), &BigUint::from(amount_out));
        }
        assert_eq!(out.net_amount_out(), &BigInt::from(expected_net));
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

        let out =
            run_with_access(&replacer, &market, &sell_order(&a, &c), access, sequential_route())
                .await;

        assert_eq!(out.is_some(), replaces);
    }

    /// The committed amount follows the user share of the improvement over the received route:
    /// 10% of 2000 on top of its net amount out of 3980, plus the 20 of gas.
    #[tokio::test]
    async fn test_exclusive_commitment() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );

        let commitment = out
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

        let out = run_granted(&replacer, &market, &sell_order(&a, &c), route).await;

        let committed_amount_out = out
            .as_ref()
            .map(|out| {
                out.commitment()
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

        let out = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );

        let exclusive_legs = out
            .route()
            .swaps()
            .iter()
            .filter(|swap| is_exclusive_leg(swap))
            .count();
        assert_eq!(exclusive_legs, 1);
        assert_eq!(component_ids(&out), ["excl_ab", "bc"]);
        assert_eq!(out.net_amount_out(), &BigInt::from(5980));
    }

    #[tokio::test]
    async fn test_exclusive_lower_amount_out() {
        let (a, b, c) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), MockProtocolSim::new(1.5).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;

        assert!(out.is_none());
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

        let out = run_granted(&replacer, &market, &sell_order(&a, &c), route).await;

        assert!(out.is_none());
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

        let out = run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;

        assert!(out.is_none());
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

        let out = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await,
        );
        assert_eq!(component_ids(&out), ["excl_ab", "bc"]);
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

        let out = run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;
        assert!(out.is_none());
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

        let out = run_granted(&replacer, &market, &sell_order(&a, &c), sequential_route()).await;
        assert!(out.is_none());
    }

    fn pool(reserve_in: u64, reserve_out: u64) -> ConstantProductSim {
        ConstantProductSim {
            reserve_0: BigUint::from(reserve_in),
            reserve_1: BigUint::from(reserve_out),
            gas: 10,
        }
    }

    /// The total amount in of the swaps that start at `token`.
    fn amount_in_of(result: &RouteResult, token: &Token) -> BigUint {
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
        result(vec![swap_on("ab", (a, b), (1000, 1980), 0.0, pool(100_000, 200_000))], &[a, b])
    }

    #[tokio::test]
    async fn test_exclusive_split_when_price_falls_with_amount() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), pool(1000, 2500)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run_granted(&replacer, &market, &sell_order(&a, &b), deep_route(&a, &b)).await,
        );

        assert_eq!(component_ids(&out), ["excl_ab", "ab"]);
        assert_eq!(splits_of(&out), [0.14589803375031532, 0.0]);
        assert_eq!(amount_in_of(&out, &a), BigUint::from(1000u64));
        // The original route's net amount out is 1970. The exclusive pool alone gives 1250 minus
        // 10 gas.
        assert_eq!(out.net_amount_out(), &BigInt::from(1991));
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

        let out = replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);

        assert_eq!(component_ids(&out), ["excl_ab", "ab"]);
        assert_eq!(splits_of(&out), [0.4984471899924292, 0.0]);
        assert_eq!(amount_in_of(&out, &a), BigUint::from(1000u64));
        // The original route's net amount out is 1990. The split gives about 2500 minus 20 gas.
        assert_eq!(out.net_amount_out(), &BigInt::from(2478));
    }

    #[tokio::test]
    async fn test_exclusive_split_of_a_swap_with_a_split() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), pool(1000, 2500)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap_on("p1", (&a, &b), (600, 1188), 0.6, pool(100_000, 200_000)),
                swap_on("p2", (&a, &b), (400, 792), 0.0, pool(100_000, 200_000)),
            ],
            &[&a, &b],
        );
        let original_net = original.net_amount_out().clone();

        let out = replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);

        assert_eq!(component_ids(&out), ["excl_ab", "p1", "p2"]);
        assert_eq!(splits_of(&out), [0.1209756742506939, 0.4790243257493061, 0.0]);
        assert_eq!(amount_in_of(&out, &a), BigUint::from(1000u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(1988));
        assert!(out.net_amount_out() > &original_net);
    }

    #[tokio::test]
    async fn test_exclusive_split_of_the_remainder_swap() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), pool(1000, 2500)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap("p1", (&a, &b), (600, 1800), 0.6, 3.0),
                swap_on("p2", (&a, &b), (400, 792), 0.0, pool(100_000, 200_000)),
            ],
            &[&a, &b],
        );
        let original_net = original.net_amount_out().clone();

        let out = replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);

        assert_eq!(component_ids(&out), ["p1", "excl_ab", "p2"]);
        assert_eq!(splits_of(&out), [0.6, 0.11671842700025233, 0.0]);
        assert_eq!(amount_in_of(&out, &a), BigUint::from(1000u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(2595));
        assert!(out.net_amount_out() > &original_net);
    }

    #[tokio::test]
    async fn test_exclusive_split_remainder_without_share() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), pool(1000, 2500)).await;
        let replacer = replacer(&market).await;
        let original = result(
            vec![
                swap_on("p1", (&a, &b), (600, 1188), 0.6, pool(100_000, 200_000)),
                swap_on("p2", (&a, &b), (0, 0), 0.0, pool(100_000, 200_000)),
                swap_on("p3", (&a, &b), (400, 792), 0.4, pool(100_000, 200_000)),
            ],
            &[&a, &b],
        );

        let out = replaced(run_granted(&replacer, &market, &sell_order(&a, &b), original).await);
        assert_eq!(component_ids(&out), ["excl_ab", "p1", "p2", "p3"]);
        assert_eq!(splits_of(&out), [0.1209756742506939, 0.4790243257493061, 0.0, 0.4]);
        assert_eq!(out.net_amount_out(), &BigInt::from(1978));
    }

    #[tokio::test]
    async fn test_exclusive_split_worse_price() {
        let (a, b, _) = tokens();
        let market = market();
        add_exclusive(&market, "excl_ab", (&a, &b), pool(1000, 1500)).await;
        let replacer = replacer(&market).await;

        let out = run_granted(&replacer, &market, &sell_order(&a, &b), deep_route(&a, &b)).await;

        assert!(out.is_none());
    }
}
