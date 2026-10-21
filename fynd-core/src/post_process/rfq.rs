//! Replaces parts of a route with one RFQ swap when that raises the net amount out.

use async_trait::async_trait;
use tracing::debug;
use tycho_simulation::tycho_common::models::Address;

use crate::{
    algorithm::paths::read_market,
    feed::{
        events::{EventError, MarketEvent, MarketEventHandler},
        market_data::MarketData,
        protocol_registry::{is_rfq_protocol, strip_fallback_prefix},
    },
    post_process::{
        replace::{ComponentIndex, Search, Snapshot, Target},
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
        let MarketEvent::MarketUpdated { added_components, removed_components, .. } = event;
        for id in removed_components {
            self.index.remove(id);
        }
        if added_components.is_empty() {
            return Ok(());
        }
        let view = market.read().await;
        for (id, topology) in added_components {
            self.index.remove(id);
            if view
                .get_component(id)
                .is_some_and(|component| is_rfq_protocol(&component.protocol_system))
            {
                self.index
                    .insert(id.clone(), topology.tokens.clone());
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RoutePostProcessor for RfqSwapReplacer {
    async fn initialize(&mut self, market: &MarketData) {
        let mut index = ComponentIndex::default();
        {
            let view = market.read().await;
            let state = view.base_market_state();
            for system in state
                .component_counts_by_protocol()
                .keys()
                .filter(|system| is_rfq_protocol(system))
            {
                for id in state.components_by_protocol(system) {
                    if let Some(component) = state.get_component(id) {
                        index.insert(id.clone(), component.tokens.clone());
                    }
                }
            }
        }
        self.index = index;
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
        Search::new(snapshot, request, route).and_then(Search::run)
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

/// Whether the RFQ swap `leg` holds both tokens of `target` and `swaps` do not use its component.
/// A leg whose protocol cannot fill partially fits only a target that starts at `sell_token`.
pub(super) fn fits(leg: &Swap, target: &Target, swaps: &[Swap], sell_token: &Address) -> bool {
    let tokens = &leg.protocol_component().tokens;
    (fills_partially(leg.protocol()) || target.token_in == *sell_token) &&
        tokens.contains(&target.token_in) &&
        tokens.contains(&target.token_out) &&
        !swaps
            .iter()
            .any(|swap| swap.component_id() == leg.component_id())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use num_bigint::{BigInt, BigUint};
    use rstest::rstest;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Address},
        simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{
            component, component_with_protocol, order, setup_market_weighted, token,
            MockProtocolSim,
        },
        derived::DerivedData,
        types::{Order, OrderSide, Route, RouteExclusions},
    };

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn tokens() -> (Token, Token, Token) {
        (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"))
    }

    fn market() -> MarketData {
        setup_market_weighted(Vec::new()).0
    }

    async fn add_component(
        market: &MarketData,
        id: &str,
        protocol: &str,
        component_tokens: &[&Token],
        sim: MockProtocolSim,
    ) {
        let component_tokens: Vec<Token> = component_tokens
            .iter()
            .map(|token| (*token).clone())
            .collect();
        let mut state = market.write().await;
        state.upsert_components(std::iter::once(component_with_protocol(
            id,
            protocol,
            &component_tokens,
        )));
        state.update_states([(id.to_string(), Box::new(sim) as Box<dyn ProtocolSim>)]);
        state.upsert_tokens(component_tokens);
    }

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

    /// A swap as returned by the algorithm, with gas priced at one output-token unit per gas unit.
    fn swap(id: &str, pair: (&Token, &Token), amounts: (u64, u64), split: f64, price: f64) -> Swap {
        Swap::new(
            id.to_string(),
            "uniswap_v2".to_string(),
            pair.0.address.clone(),
            pair.1.address.clone(),
            BigUint::from(amounts.0),
            BigUint::from(amounts.1),
            BigUint::from(10u64),
            component(id, &[pair.0.clone(), pair.1.clone()]),
            Box::new(MockProtocolSim::new(price).with_gas(10)),
        )
        .with_split(split)
    }

    fn result(swaps: Vec<Swap>, all_tokens: &[&Token]) -> RouteResult {
        let gas: u64 = swaps.len() as u64 * 10;
        let output = swaps
            .last()
            .expect("the test route has swaps")
            .token_out()
            .clone();
        let route = Route::new(
            swaps,
            all_tokens
                .iter()
                .map(|token| (token.address.clone(), (*token).clone())),
        )
        .expect("the test swaps form a route");
        let amount_out = BigInt::from(route.amount_out(&output));
        RouteResult::new(route, amount_out - gas, BigUint::from(1u64))
    }

    fn sequential_route() -> RouteResult {
        let (a, b, c) = tokens();
        result(
            vec![
                swap("ab", (&a, &b), (1000, 2000), 0.0, 2.0),
                swap("bc", (&b, &c), (2000, 4000), 0.0, 2.0),
            ],
            &[&a, &b, &c],
        )
    }

    fn sell_order(from: &Token, to: &Token) -> Order {
        order(from, to, 1000, OrderSide::Sell)
    }

    async fn run(
        replacer: &RfqSwapReplacer,
        market: &MarketData,
        order: &Order,
        exclusions: &RouteExclusions,
        route: RouteResult,
    ) -> Option<RouteResult> {
        let derived = DerivedData::new_shared();
        let request = PostProcessRequest::new(order, market, &derived, exclusions, far_deadline());
        replacer.process(&request, &route).await
    }

    fn replaced(out: Option<RouteResult>) -> RouteResult {
        out.expect("the replacer replaces the route")
    }

    /// The component ids of the replacement, or none when the replacer keeps the route.
    fn replaced_ids(out: &Option<RouteResult>) -> Vec<&str> {
        out.as_ref()
            .map(component_ids)
            .unwrap_or_default()
    }

    fn component_ids(result: &RouteResult) -> Vec<&str> {
        result
            .route()
            .swaps()
            .iter()
            .map(Swap::component_id)
            .collect()
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

    fn added(id: &str, pair: &[&Token]) -> MarketEvent {
        MarketEvent::MarketUpdated {
            added_components: [(
                id.to_string(),
                pair.iter()
                    .map(|token| token.address.clone())
                    .collect::<Vec<_>>()
                    .into(),
            )]
            .into_iter()
            .collect(),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        }
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
