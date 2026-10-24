//! Replaces parts of a route with one RFQ swap when that raises the net amount out.

use async_trait::async_trait;
use tracing::debug;

use crate::{
    algorithm::paths::read_market,
    feed::{
        events::{EventError, MarketEvent, MarketEventHandler},
        market_data::MarketData,
        protocol_registry::{is_rfq_protocol, strip_fallback_prefix},
    },
    post_process::{
        replace::{
            build_index, can_serve, update_index, ComponentIndex, ReplacementRule, RouteEvaluator,
            Search, Snapshot, Target,
        },
        PostProcessRequest, RoutePostProcessor,
    },
    types::{RouteResult, Swap},
};

/// Tries to replace swaps of the route with one swap through an RFQ component.
///
/// It replaces one swap, or all swaps of a segment: the run of swaps between two tokens that every
/// path of the order passes through. It keeps a replacement only when the net amount out rises.
/// Only sell orders change. It uses only RFQ components that the request
/// does not exclude and the pool does not drop. An RFQ protocol that cannot fill partially replaces
/// only swaps that start at the sell token. The server must index `rfq:` or `fallback:rfq:`
/// protocols for the replacer to find components.
#[derive(Default)]
pub(crate) struct RfqSwapReplacer {
    market: Option<MarketData>,
    index: ComponentIndex,
}

impl RfqSwapReplacer {
    /// The name that a pool configuration lists to run this post-processor.
    pub(crate) const NAME: &'static str = "rfq_swap_replacer";
}

#[async_trait]
impl MarketEventHandler for RfqSwapReplacer {
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError> {
        let Some(market) = &self.market else {
            return Ok(());
        };
        update_index(&mut self.index, market, event, |component| {
            is_rfq_protocol(&component.protocol_system)
        })
        .await;
        Ok(())
    }
}

#[async_trait]
impl RoutePostProcessor for RfqSwapReplacer {
    async fn initialize(&mut self, market: &MarketData) {
        self.index =
            build_index(market, |component| is_rfq_protocol(&component.protocol_system)).await;
        self.market = Some(market.clone());
    }

    async fn process(
        &self,
        request: &PostProcessRequest<'_>,
        route: &RouteResult,
    ) -> Option<RouteResult> {
        if self.index.is_empty() || !request.order().is_sell() {
            return None;
        }
        let snapshot = match read_market(request.market(), request.label().cloned()).await {
            Ok(view) => Snapshot::take(&self.index, &view, request, route.route()),
            Err(error) => {
                debug!(%error, "cannot read the market; keeping the route");
                return None;
            }
        };
        Search::new(snapshot, request, route, RfqRule).and_then(Search::run)
    }
}

/// RFQ protocols whose signed quotes support input amounts smaller than the quoted amount. The
/// replacer uses other RFQ protocols only for swaps starting at the sell token, where the input
/// amount matches the quoted amount exactly.
const PARTIAL_FILL_PROTOCOLS: &[&str] = &["rfq:bebop", "rfq:hashflow", "rfq:native"];

/// True when `protocol_system` is in [`PARTIAL_FILL_PROTOCOLS`], through its own executor or its
/// fallback router.
fn fills_partially(protocol_system: &str) -> bool {
    PARTIAL_FILL_PROTOCOLS.contains(&strip_fallback_prefix(protocol_system))
}

/// Replaces a target with one swap through an RFQ component.
struct RfqRule;

impl ReplacementRule for RfqRule {
    /// A leg whose protocol cannot fill partially fits only a target that starts at the input
    /// token of the order.
    fn fits(
        &self,
        leg: &Swap,
        target: &Target,
        swaps: &[Swap],
        evaluator: &RouteEvaluator<'_>,
    ) -> bool {
        (fills_partially(leg.protocol()) || target.token_in == *evaluator.input_token()) &&
            can_serve(leg, target, swaps)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use num_bigint::{BigInt, BigUint};
    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, token::Token};

    use super::*;
    use crate::{
        algorithm::test_utils::{component, MockProtocolSim},
        derived::DerivedData,
        post_process::test_utils::{
            add_component, added, component_ids, far_deadline, market, replaced, replaced_ids,
            result, run, sell_order, sequential_route, swap, tokens,
        },
        types::{Route, RouteExclusions},
    };

    async fn add_rfq(
        market: &MarketData,
        id: &str,
        component_tokens: &[&Token],
        sim: MockProtocolSim,
    ) {
        add_component(market, id, "rfq:bebop", component_tokens, sim).await;
    }

    async fn replacer(market: &MarketData) -> RfqSwapReplacer {
        let mut replacer = RfqSwapReplacer::default();
        replacer.initialize(market).await;
        replacer
    }

    #[tokio::test]
    async fn test_rfq_hop_with_downstream_swaps() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &c),
                &RouteExclusions::default(),
                sequential_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_ab", "bc"]);
        let swaps = out.route().swaps();
        assert_eq!(swaps[0].amount_out(), &BigUint::from(3000u64));
        assert_eq!(swaps[1].amount_in(), &BigUint::from(3000u64));
        assert_eq!(swaps[1].amount_out(), &BigUint::from(6000u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(5980));
    }

    #[tokio::test]
    async fn test_rfq_lower_amount_out() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(1.5).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = run(
            &replacer,
            &market,
            &sell_order(&a, &c),
            &RouteExclusions::default(),
            sequential_route(),
        )
        .await;

        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_rfq_for_whole_order() {
        let (a, _, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ac", &[&a, &c], MockProtocolSim::new(5.0).with_gas(5)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &c),
                &RouteExclusions::default(),
                sequential_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_ac"]);
        assert_eq!(out.route().swaps()[0].amount_out(), &BigUint::from(5000u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(4995));
    }

    fn split_route() -> RouteResult {
        let (a, b, _) = tokens();
        result(
            vec![
                swap("p1", (&a, &b), (600, 1200), 0.6, 2.0),
                swap("p2", (&a, &b), (400, 800), 0.0, 2.0),
            ],
            &[&a, &b],
        )
    }

    #[tokio::test]
    async fn test_rfq_for_parallel_swaps() {
        let (a, b, _) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &b),
                &RouteExclusions::default(),
                split_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_ab"]);
        assert_eq!(out.route().swaps()[0].amount_in(), &BigUint::from(1000u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(2990));
    }

    #[tokio::test]
    async fn test_rfq_for_one_leg_with_split() {
        let (a, b, _) = tokens();
        let market = market();
        let limited = MockProtocolSim::new(3.0)
            .with_gas(10)
            .with_liquidity(1800);
        add_rfq(&market, "rfq_ab", &[&a, &b], limited).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &b),
                &RouteExclusions::default(),
                split_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_ab", "p2"]);
        let swaps = out.route().swaps();
        assert_eq!(*swaps[0].split(), 0.6);
        assert_eq!(swaps[0].amount_out(), &BigUint::from(1800u64));
        assert_eq!(swaps[1].amount_out(), &BigUint::from(800u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(2580));
    }

    #[tokio::test]
    async fn test_rfq_excluded_component() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let exclusions = RouteExclusions::default().with_pools(["rfq_ab".to_string()]);

        let out =
            run(&replacer, &market, &sell_order(&a, &c), &exclusions, sequential_route()).await;

        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_rfq_component_added_after_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        let mut replacer = replacer(&market).await;
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;

        replacer
            .handle_event(&added("rfq_ab", &[&a, &b]))
            .await
            .expect("adding an RFQ component");

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &c),
                &RouteExclusions::default(),
                sequential_route(),
            )
            .await,
        );
        assert_eq!(component_ids(&out), ["rfq_ab", "bc"]);
    }

    #[tokio::test]
    async fn test_rfq_component_removed_after_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let mut replacer = replacer(&market).await;
        let removed = MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: vec!["rfq_ab".to_string()],
            updated_components: Vec::new(),
        };

        replacer
            .handle_event(&removed)
            .await
            .expect("removing an RFQ component");

        let out = run(
            &replacer,
            &market,
            &sell_order(&a, &c),
            &RouteExclusions::default(),
            sequential_route(),
        )
        .await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_non_rfq_component_added_after_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        let mut replacer = replacer(&market).await;
        add_component(
            &market,
            "v2_ab",
            "uniswap_v2",
            &[&a, &b],
            MockProtocolSim::new(3.0).with_gas(10),
        )
        .await;

        replacer
            .handle_event(&added("v2_ab", &[&a, &b]))
            .await
            .expect("adding a non-RFQ component");

        let out = run(
            &replacer,
            &market,
            &sell_order(&a, &c),
            &RouteExclusions::default(),
            sequential_route(),
        )
        .await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_event_before_initialize() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let mut replacer = RfqSwapReplacer::default();

        replacer
            .handle_event(&added("rfq_ab", &[&a, &b]))
            .await
            .expect("an event before initialize");

        let out = run(
            &replacer,
            &market,
            &sell_order(&a, &c),
            &RouteExclusions::default(),
            sequential_route(),
        )
        .await;
        assert!(out.is_none());
    }

    #[rstest]
    #[case::cheap_gas(50, Some(2450))]
    #[case::costly_gas(600, None)]
    #[tokio::test]
    async fn test_rfq_gas_cost(#[case] rfq_gas: u64, #[case] expected_net: Option<i64>) {
        let (a, b, _) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(2.5).with_gas(rfq_gas)).await;
        let replacer = replacer(&market).await;
        let route = result(vec![swap("ab", (&a, &b), (1000, 2000), 0.0, 2.0)], &[&a, &b]);

        let out =
            run(&replacer, &market, &sell_order(&a, &b), &RouteExclusions::default(), route).await;

        match expected_net {
            Some(net) => {
                let out = replaced(out);
                assert_eq!(component_ids(&out), ["rfq_ab"]);
                assert_eq!(out.net_amount_out(), &BigInt::from(net));
            }
            None => assert!(out.is_none()),
        }
    }

    #[tokio::test]
    async fn test_rfq_for_two_hops() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        add_rfq(&market, "rfq_bc", &[&b, &c], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &c),
                &RouteExclusions::default(),
                sequential_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_ab", "rfq_bc"]);
        let swaps = out.route().swaps();
        assert_eq!(swaps[0].amount_out(), &BigUint::from(3000u64));
        assert_eq!(swaps[1].amount_in(), &BigUint::from(3000u64));
        assert_eq!(swaps[1].amount_out(), &BigUint::from(9000u64));
        assert_eq!(out.net_amount_out(), &BigInt::from(8980));
    }

    #[tokio::test]
    async fn test_rfq_component_covering_two_legs() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_abc", &[&a, &b, &c], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &c),
                &RouteExclusions::default(),
                sequential_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_abc", "bc"]);
    }

    #[tokio::test]
    async fn test_rfq_deadline_in_the_past() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let derived = DerivedData::new_shared();
        let order = sell_order(&a, &c);
        let exclusions = RouteExclusions::default();
        let request =
            PostProcessRequest::new(&order, &market, &derived, &exclusions, Instant::now());

        let out = replacer
            .process(&request, &sequential_route())
            .await;

        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_rfq_unknown_state_label() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let derived = DerivedData::new_shared();
        let order = sell_order(&a, &c);
        let exclusions = RouteExclusions::default();
        let label = "missing".to_string();
        let request =
            PostProcessRequest::new(&order, &market, &derived, &exclusions, far_deadline())
                .with_label(Some(&label));

        let out = replacer
            .process(&request, &sequential_route())
            .await;

        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_route_without_gas() {
        let (a, b, _) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let free = Swap::new(
            "ab".to_string(),
            "uniswap_v2".to_string(),
            a.address.clone(),
            b.address.clone(),
            BigUint::from(1000u64),
            BigUint::from(2000u64),
            BigUint::ZERO,
            component("ab", &[a.clone(), b.clone()]),
            Box::new(MockProtocolSim::new(2.0)),
        );
        let route = Route::new(vec![free], [(a.address.clone(), a.clone())]).expect("one swap");
        let route = RouteResult::new(route, BigInt::from(2000), BigUint::from(1u64));

        let out =
            run(&replacer, &market, &sell_order(&a, &b), &RouteExclusions::default(), route).await;

        assert!(out.is_none());
    }

    /// Only a protocol that fills partially replaces a swap after the sell token.
    #[rstest]
    #[case::partial_fill_after_sell_token("rfq:bebop", false, &["ab", "rfq"])]
    #[case::partial_fill_via_fallback_router("fallback:rfq:bebop", false, &["ab", "rfq"])]
    #[case::partial_fill_hashflow("rfq:hashflow", false, &["ab", "rfq"])]
    #[case::partial_fill_native("rfq:native", false, &["ab", "rfq"])]
    #[case::exact_fill_after_sell_token("rfq:metric", false, &[])]
    #[case::exact_fill_via_fallback_router("fallback:rfq:metric", false, &[])]
    #[case::exact_fill_at_sell_token("rfq:metric", true, &["rfq", "bc"])]
    #[tokio::test]
    async fn test_rfq_partial_fill(
        #[case] protocol: &str,
        #[case] at_sell_token: bool,
        #[case] expected: &[&str],
    ) {
        let (a, b, c) = tokens();
        let pair = if at_sell_token { [&a, &b] } else { [&b, &c] };
        let market = market();
        add_component(&market, "rfq", protocol, &pair, MockProtocolSim::new(3.0).with_gas(10))
            .await;
        let replacer = replacer(&market).await;

        let out = run(
            &replacer,
            &market,
            &sell_order(&a, &c),
            &RouteExclusions::default(),
            sequential_route(),
        )
        .await;

        assert_eq!(replaced_ids(&out), expected);
    }

    #[tokio::test]
    async fn test_rfq_two_components_same_pair() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_1", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        add_rfq(&market, "rfq_2", &[&a, &b], MockProtocolSim::new(2.5).with_gas(10)).await;
        let replacer = replacer(&market).await;

        let out = replaced(
            run(
                &replacer,
                &market,
                &sell_order(&a, &c),
                &RouteExclusions::default(),
                sequential_route(),
            )
            .await,
        );

        assert_eq!(component_ids(&out), ["rfq_1", "bc"]);
    }

    #[tokio::test]
    async fn test_rfq_component_dropped_by_pool() {
        let (a, b, c) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(3.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let derived = DerivedData::new_shared();
        let order = sell_order(&a, &c);
        let exclusions = RouteExclusions::default();
        let drops = |component: &ProtocolComponent| component.id == "rfq_ab";
        let request =
            PostProcessRequest::new(&order, &market, &derived, &exclusions, far_deadline())
                .with_pool_drops(&drops);

        let out = replacer
            .process(&request, &sequential_route())
            .await;

        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_rfq_stale_original_route() {
        let (a, b, _) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(2.0).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let stale = result(vec![swap("ab", (&a, &b), (1000, 1500), 0.0, 2.0)], &[&a, &b]);

        let out =
            run(&replacer, &market, &sell_order(&a, &b), &RouteExclusions::default(), stale).await;

        assert!(out.is_none());
    }

    /// The original route claims a net above what a new simulation gives, so a replacement that
    /// beats the simulation can still pay less than the original route.
    #[tokio::test]
    async fn test_rfq_below_original_net() {
        let (a, b, _) = tokens();
        let market = market();
        add_rfq(&market, "rfq_ab", &[&a, &b], MockProtocolSim::new(2.2).with_gas(10)).await;
        let replacer = replacer(&market).await;
        let optimistic = result(vec![swap("ab", (&a, &b), (1000, 2500), 0.0, 2.0)], &[&a, &b]);

        let out =
            run(&replacer, &market, &sell_order(&a, &b), &RouteExclusions::default(), optimistic)
                .await;

        assert!(out.is_none());
    }
}
