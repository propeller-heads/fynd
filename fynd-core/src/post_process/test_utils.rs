//! Test doubles for post-processors.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use num_bigint::{BigInt, BigUint};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::{protocol::ProtocolComponent, token::Token},
};

use crate::{
    algorithm::test_utils::{
        component, component_with_protocol, order, setup_market_weighted, token, MockProtocolSim,
    },
    derived::DerivedData,
    feed::{
        events::{EventError, MarketEvent, MarketEventHandler},
        market_data::MarketData,
    },
    post_process::{PostProcessRequest, RoutePostProcessor},
    types::{Order, OrderSide, Route, RouteExclusions, RouteResult, Swap},
};

type Rewrite =
    Box<dyn Fn(&PostProcessRequest<'_>, RouteResult) -> Option<RouteResult> + Send + Sync>;

/// A post-processor that applies `rewrite` to a copy of the route after `delay`. It counts the
/// calls it receives and can fail every market event.
pub(crate) struct MockPostProcessor {
    rewrite: Rewrite,
    delay: Duration,
    fail_events: bool,
    pub(crate) initialize_calls: Arc<AtomicUsize>,
    pub(crate) event_count: Arc<AtomicUsize>,
    /// IDs of components added by market events received by this mock.
    pub(crate) added_component_ids: Arc<Mutex<Vec<String>>>,
}

impl MockPostProcessor {
    pub(crate) fn new(
        rewrite: impl Fn(RouteResult) -> RouteResult + Send + Sync + 'static,
    ) -> Self {
        Self::with_request(move |_, route| rewrite(route))
    }

    /// Like [`new`](Self::new), for a rewrite that reads the request.
    pub(crate) fn with_request(
        rewrite: impl Fn(&PostProcessRequest<'_>, RouteResult) -> RouteResult + Send + Sync + 'static,
    ) -> Self {
        Self::with_rewrite(Box::new(move |request, route| Some(rewrite(request, route))))
    }

    /// A post-processor that keeps every route.
    pub(crate) fn unchanged() -> Self {
        Self::with_rewrite(Box::new(|_, _| None))
    }

    fn with_rewrite(rewrite: Rewrite) -> Self {
        Self {
            rewrite,
            delay: Duration::ZERO,
            fail_events: false,
            initialize_calls: Arc::default(),
            event_count: Arc::default(),
            added_component_ids: Arc::default(),
        }
    }

    pub(crate) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub(crate) fn failing_events(mut self) -> Self {
        self.fail_events = true;
        self
    }
}

#[async_trait]
impl MarketEventHandler for MockPostProcessor {
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError> {
        let MarketEvent::MarketUpdated { added_components, .. } = event;
        self.added_component_ids
            .lock()
            .expect("the test holds no other lock")
            .extend(added_components.keys().cloned());
        self.event_count
            .fetch_add(1, Ordering::SeqCst);
        if self.fail_events {
            return Err(EventError::GraphErrors(Vec::new()));
        }
        Ok(())
    }
}

#[async_trait]
impl RoutePostProcessor for MockPostProcessor {
    async fn initialize(&mut self, _market: &MarketData) {
        self.initialize_calls
            .fetch_add(1, Ordering::SeqCst);
    }

    async fn process(
        &self,
        request: &PostProcessRequest<'_>,
        route: &RouteResult,
    ) -> Option<RouteResult> {
        tokio::time::sleep(self.delay).await;
        (self.rewrite)(request, route.clone())
    }
}

/// A single token 1 to token 2 swap through `component` that pays `amount_out`.
pub(crate) fn swap_through(component: ProtocolComponent, amount_out: u64) -> Swap {
    Swap::new(
        component.id.to_string(),
        component.protocol_system.clone(),
        token(0x01, "A").address,
        token(0x02, "B").address,
        BigUint::from(100u64),
        BigUint::from(amount_out),
        BigUint::from(1u64),
        component,
        Box::new(MockProtocolSim::new(2.0)),
    )
}

/// A single token 1 to token 2 swap through a pool of `protocol` that pays `amount_out`.
pub(crate) fn direct_swap(protocol: &str, amount_out: u64) -> Swap {
    let tokens = [token(0x01, "A"), token(0x02, "B")];
    swap_through(component_with_protocol("rfq-leg", protocol, &tokens), amount_out)
}

/// A route result of one swap that pays 100, with `net_amount_out`.
pub(crate) fn route_result(net_amount_out: i64) -> RouteResult {
    let route = Route::new(vec![direct_swap("uniswap_v2", 100)], []).expect("one swap");
    RouteResult::new(route, BigInt::from(net_amount_out), BigUint::from(1u64))
}

/// `result` with its net amount out replaced.
pub(crate) fn with_net_amount_out(result: RouteResult, net_amount_out: i64) -> RouteResult {
    RouteResult::new(
        result.route().clone(),
        BigInt::from(net_amount_out),
        result.gas_price().clone(),
    )
}

/// A deadline that no replacer test reaches.
pub(crate) fn far_deadline() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

/// The tokens A, B and C of the replacer tests.
pub(crate) fn tokens() -> (Token, Token, Token) {
    (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"))
}

/// An empty market.
pub(crate) fn market() -> MarketData {
    setup_market_weighted(Vec::new()).0
}

/// Adds the component `id` of `protocol` over `component_tokens` to `market`, with `sim` as its
/// simulation state.
pub(crate) async fn add_component(
    market: &MarketData,
    id: &str,
    protocol: &str,
    component_tokens: &[&Token],
    sim: impl ProtocolSim + 'static,
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

/// A swap as returned by the algorithm, with gas priced at one output-token unit per gas unit.
pub(crate) fn swap(
    id: &str,
    pair: (&Token, &Token),
    amounts: (u64, u64),
    split: f64,
    price: f64,
) -> Swap {
    swap_on(id, pair, amounts, split, MockProtocolSim::new(price).with_gas(10))
}

/// Like [`swap`], on the pool state `sim`.
pub(crate) fn swap_on(
    id: &str,
    pair: (&Token, &Token),
    amounts: (u64, u64),
    split: f64,
    sim: impl ProtocolSim + 'static,
) -> Swap {
    Swap::new(
        id.to_string(),
        "uniswap_v2".to_string(),
        pair.0.address.clone(),
        pair.1.address.clone(),
        BigUint::from(amounts.0),
        BigUint::from(amounts.1),
        BigUint::from(10u64),
        component(id, &[pair.0.clone(), pair.1.clone()]),
        Box::new(sim),
    )
    .with_split(split)
}

/// A route result of `swaps` that nets its amount out minus 10 for each swap.
pub(crate) fn result(swaps: Vec<Swap>, all_tokens: &[&Token]) -> RouteResult {
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

/// A sell order of 1000 `from` for `to`.
pub(crate) fn sell_order(from: &Token, to: &Token) -> Order {
    order(from, to, 1000, OrderSide::Sell)
}

/// Runs `processor` on `route` with a far deadline.
pub(crate) async fn run(
    processor: &impl RoutePostProcessor,
    market: &MarketData,
    order: &Order,
    exclusions: &RouteExclusions,
    route: RouteResult,
) -> Option<RouteResult> {
    let derived = DerivedData::new_shared();
    let request = PostProcessRequest::new(order, market, &derived, exclusions, far_deadline());
    processor
        .process(&request, &route)
        .await
}

/// The route of the replacement, which the test expects.
pub(crate) fn replaced(out: Option<RouteResult>) -> RouteResult {
    out.expect("the replacer replaces the route")
}

/// The component ids of the replacement, or none when the replacer keeps the route.
pub(crate) fn replaced_ids(out: &Option<RouteResult>) -> Vec<&str> {
    out.as_ref()
        .map(component_ids)
        .unwrap_or_default()
}

/// The component ids of the swaps of `result`, in route order.
pub(crate) fn component_ids(result: &RouteResult) -> Vec<&str> {
    result
        .route()
        .swaps()
        .iter()
        .map(Swap::component_id)
        .collect()
}

/// A market event that adds the component `id` over `pair`.
pub(crate) fn added(id: &str, pair: &[&Token]) -> MarketEvent {
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

/// The route A to B to C over the pools `ab` and `bc`, at a price of 2 each.
pub(crate) fn sequential_route() -> RouteResult {
    let (a, b, c) = tokens();
    result(
        vec![
            swap("ab", (&a, &b), (1000, 2000), 0.0, 2.0),
            swap("bc", (&b, &c), (2000, 4000), 0.0, 2.0),
        ],
        &[&a, &b, &c],
    )
}
