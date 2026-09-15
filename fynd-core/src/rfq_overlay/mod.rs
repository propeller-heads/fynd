//! Replaces hops of a solved route with an RFQ quote when the RFQ pays more.
//!
//! RFQ components stay out of every worker's graph (`is_rfq_component` is part of the worker's
//! drop rule), so the algorithms solve over on-chain liquidity alone. After the solve, and before
//! the router ranks the candidates, [`RfqOverlay::improve`] takes each hop of a candidate route
//! (the swaps sharing one `token_in → token_out` pair), asks every RFQ component quoting that
//! pair for the hop's whole `amount_in`, and puts the best RFQ leg in place of the hop when it pays
//! more. The downstream swaps are then re-priced from their own quote-time states. The candidate
//! keeps the RFQ legs only when the route as a whole nets more after gas.
//!
//! Hops are tried in path order and each replacement is kept for the next hop's evaluation, so a
//! route can end up with several RFQ legs. The decision at the end is all or nothing: a route whose
//! RFQ legs win per hop but lose after gas is returned unchanged.
//!
//! Two kinds of route are left alone: one carrying an exclusive leg (its committed amount is tied
//! to the leg), and one with a pAMM fallback amount (the amount describes the original legs).

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use metrics::counter;
use num_bigint::BigUint;
use num_traits::{CheckedSub, Zero};
use rustc_hash::FxHashMap;
use tokio::{
    sync::broadcast::{self, error::RecvError},
    task::JoinHandle,
};
use tracing::{debug, warn};
use tycho_simulation::tycho_common::{
    models::{protocol::ProtocolComponent, token::Token, Address},
    simulation::protocol_sim::{GetAmountOutResult, ProtocolSim},
    Bytes,
};

use crate::{
    algorithm::{sim_guard::GuardedProtocolSim, split_primitives::split_amount},
    feed::{
        events::MarketEvent,
        market_data::{MarketData, MarketDataView, MarketState},
    },
    replay::ReplayError,
    types::{ComponentId, OrderQuote, QuoteStatus, Route, Swap},
};

/// Whether `component` is quoted by an RFQ maker: its simulation state implements
/// `IndicativelyPriced`, the trait every RFQ state in tycho-simulation implements and no on-chain
/// state does. A component without a state in `market` is not an RFQ.
pub(crate) fn is_rfq_component(market: &MarketState, component: &ProtocolComponent) -> bool {
    market
        .get_simulation_state(&component.id)
        .is_some_and(|state| state.as_indicatively_priced().is_ok())
}

/// RFQ components by token pair, so the overlay finds the makers quoting a hop with one lookup.
///
/// Membership follows the market's components, not their states: an RFQ component is one for its
/// whole life, so `apply_event` only reads `added_components` and `removed_components`.
#[derive(Debug, Default)]
pub struct RfqIndex {
    by_pair: FxHashMap<(Address, Address), Vec<ComponentId>>,
    /// Reverse map, so a removed component is found without scanning `by_pair`.
    keys: FxHashMap<ComponentId, (Address, Address)>,
}

impl RfqIndex {
    /// Indexes every RFQ component in the market. Call once, then keep current with
    /// `apply_event`.
    pub fn build(market: &MarketDataView<'_>) -> Self {
        let mut index = Self::default();
        for component_id in market.component_topology().into_keys() {
            index.insert(market, component_id);
        }
        index
    }

    /// Adds the RFQ components among `event`'s additions and drops the removed ones.
    pub fn apply_event(&mut self, market: &MarketDataView<'_>, event: &MarketEvent) {
        let MarketEvent::MarketUpdated { added_components, removed_components, .. } = event;
        for component_id in removed_components {
            self.remove(component_id);
        }
        for component_id in added_components.keys() {
            self.insert(market, component_id.clone());
        }
    }

    /// The RFQ components quoting this pair, in either direction.
    pub fn components_for(&self, token_a: &Address, token_b: &Address) -> &[ComponentId] {
        self.by_pair
            .get(&sorted_pair(token_a, token_b))
            .map_or(&[], Vec::as_slice)
    }

    /// Whether the market holds no RFQ component at all.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Indexes `component_id` when it is an RFQ component over exactly two tokens.
    fn insert(&mut self, market: &MarketDataView<'_>, component_id: ComponentId) {
        let Some(component) = market.get_component(&component_id) else { return };
        if !is_rfq_component(market.base_market_state(), component) {
            return;
        }
        let [token_a, token_b] = component.tokens.as_slice() else { return };
        let pair = sorted_pair(token_a, token_b);
        if self.keys.contains_key(&component_id) {
            return;
        }
        self.keys
            .insert(component_id.clone(), pair.clone());
        self.by_pair
            .entry(pair)
            .or_default()
            .push(component_id);
    }

    fn remove(&mut self, component_id: &ComponentId) {
        let Some(pair) = self.keys.remove(component_id) else { return };
        let Some(members) = self.by_pair.get_mut(&pair) else { return };
        members.retain(|id| id != component_id);
        if members.is_empty() {
            self.by_pair.remove(&pair);
        }
    }
}

fn sorted_pair(token_a: &Address, token_b: &Address) -> (Address, Address) {
    if token_a <= token_b {
        (token_a.clone(), token_b.clone())
    } else {
        (token_b.clone(), token_a.clone())
    }
}

/// The router's RFQ post-processing step: owns the [`RfqIndex`] and the task that keeps it current.
pub struct RfqOverlay {
    market: MarketData,
    index: Arc<RwLock<RfqIndex>>,
    index_task: JoinHandle<()>,
}

impl std::fmt::Debug for RfqOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RfqOverlay")
            .finish_non_exhaustive()
    }
}

impl RfqOverlay {
    /// Builds the index from `market` and keeps it current from `events` on a background task.
    ///
    /// The task ends when the event sender is dropped, and is aborted when the overlay is dropped.
    pub fn start(market: MarketData, events: broadcast::Receiver<MarketEvent>) -> Self {
        let index = Arc::new(RwLock::new(RfqIndex::default()));
        let index_task = tokio::spawn(maintain_index(market.clone(), Arc::clone(&index), events));
        Self { market, index, index_task }
    }

    /// Puts RFQ legs into `quote`'s route where they net more than the hops they replace, and
    /// rewrites the quote's amounts and gas to match. Leaves every other quote untouched.
    pub async fn improve(&self, quote: &mut OrderQuote) {
        if quote.status() != QuoteStatus::Success {
            return;
        }
        let Some(route) = quote.route() else { return };
        let outcome = {
            // The market view first: the index lock is synchronous and must not be held across
            // an await.
            let market = self.market.read().await;
            let index = self
                .index
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if index.is_empty() {
                return;
            }
            improve_route(route, &market, &index)
        };
        match outcome {
            Ok(Some(improved)) => apply_improvement(quote, improved),
            Ok(None) => record_outcome("kept"),
            Err(error) => {
                warn!(order_id = %quote.order_id(), %error, "RFQ overlay could not re-price the route");
                record_outcome("failed");
            }
        }
    }
}

impl Drop for RfqOverlay {
    fn drop(&mut self) {
        self.index_task.abort();
    }
}

async fn maintain_index(
    market: MarketData,
    index: Arc<RwLock<RfqIndex>>,
    mut events: broadcast::Receiver<MarketEvent>,
) {
    let rebuild = |index: &Arc<RwLock<RfqIndex>>, market: &MarketDataView<'_>| {
        let built = RfqIndex::build(market);
        *index
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = built;
    };
    rebuild(&index, &market.read().await);
    loop {
        match events.recv().await {
            Ok(event) => {
                let market = market.read().await;
                index
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .apply_event(&market, &event);
            }
            Err(RecvError::Lagged(skipped)) => {
                warn!(skipped, "RFQ index missed market events; rebuilding from the market");
                rebuild(&index, &market.read().await);
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// A route with at least one RFQ leg in place of a hop, and the output it now delivers.
struct ImprovedRoute {
    route: Route,
    amount_out: BigUint,
}

/// The RFQ that pays the most for `amount_in` on this pair, with the component and state to build
/// the leg from. `None` when no RFQ quotes the pair or the amount.
fn best_rfq_quote<'m>(
    market: &'m MarketDataView<'m>,
    index: &RfqIndex,
    amount_in: &BigUint,
    token_in: &Token,
    token_out: &Token,
) -> Option<(&'m ProtocolComponent, &'m dyn ProtocolSim, GetAmountOutResult)> {
    let mut best: Option<(&ProtocolComponent, &dyn ProtocolSim, GetAmountOutResult)> = None;
    for component_id in index.components_for(&token_in.address, &token_out.address) {
        let (Some(component), Some(state)) =
            (market.get_component(component_id), market.get_simulation_state(component_id))
        else {
            continue;
        };
        let result = match state.get_amount_out_guarded(amount_in.clone(), token_in, token_out) {
            Ok(result) => result,
            Err(error) => {
                debug!(component_id, %error, "RFQ does not quote this hop");
                continue;
            }
        };
        if best
            .as_ref()
            .is_none_or(|(_, _, current)| result.amount > current.amount)
        {
            best = Some((component, state, result));
        }
    }
    best
}

/// Tries every hop of `route` against the RFQ index. `Ok(None)` when no hop is replaced.
///
/// # Errors
///
/// Returns [`ReplayError`] when the route cannot be re-priced after a replacement: a token the
/// route does not carry, or a downstream state that no longer accepts its new input.
fn improve_route(
    route: &Route,
    market: &MarketDataView<'_>,
    index: &RfqIndex,
) -> Result<Option<ImprovedRoute>, ReplayError> {
    if route.fallback_amount_out().is_some() ||
        route
            .swaps()
            .iter()
            .any(|swap| swap.committed_amount_out().is_some())
    {
        return Ok(None);
    }
    let (Some(input_token), Some(output_token)) = (route.input_token(), route.output_token())
    else {
        return Ok(None);
    };
    let total_in: BigUint = route
        .swaps()
        .iter()
        .filter(|swap| *swap.token_in() == input_token)
        .map(Swap::amount_in)
        .sum();
    let tokens = route.tokens();

    let mut swaps: Vec<Swap> = route.swaps().to_vec();
    let mut replaced = false;
    let mut hop = 0;
    while hop < swaps.len() {
        let token_in = swaps[hop].token_in().clone();
        let token_out = swaps[hop].token_out().clone();
        let members: Vec<usize> = (hop..swaps.len())
            .filter(|&i| *swaps[i].token_in() == token_in && *swaps[i].token_out() == token_out)
            .collect();
        let hop_amount_in: BigUint = members
            .iter()
            .map(|&i| swaps[i].amount_in())
            .sum();
        let hop_amount_out: BigUint = members
            .iter()
            .map(|&i| swaps[i].amount_out())
            .sum();
        let (Some(token_in_meta), Some(token_out_meta)) =
            (lookup_token(tokens, market, &token_in), lookup_token(tokens, market, &token_out))
        else {
            hop += 1;
            continue;
        };
        let Some((component, state, result)) =
            best_rfq_quote(market, index, &hop_amount_in, token_in_meta, token_out_meta)
        else {
            hop += 1;
            continue;
        };
        if result.amount <= hop_amount_out {
            hop += 1;
            continue;
        }
        let split = merged_split(&swaps, &members);
        let rfq_swap = Swap::new(
            component.id.clone(),
            component.protocol_system.clone(),
            token_in,
            token_out,
            hop_amount_in,
            result.amount,
            result.gas,
            component.clone(),
            state.clone_box(),
        )
        .with_split(split);
        for &i in members.iter().rev() {
            swaps.remove(i);
        }
        swaps.insert(hop, rfq_swap);
        reprice(&mut swaps, tokens, market, &input_token, &total_in)?;
        replaced = true;
        hop += 1;
    }
    if !replaced {
        return Ok(None);
    }

    let amount_out = swaps
        .iter()
        .filter(|swap| *swap.token_out() == output_token)
        .map(Swap::amount_out)
        .sum();
    let route = Route::new(swaps, tokens.clone()).map_err(|_| ReplayError::EmptyRoute)?;
    Ok(Some(ImprovedRoute { route, amount_out }))
}

/// The split the one RFQ leg takes over from the swaps it replaces: the remainder when one of them
/// was the remainder swap, the sum of their fractions otherwise.
fn merged_split(swaps: &[Swap], members: &[usize]) -> f64 {
    if members
        .iter()
        .any(|&i| *swaps[i].split() == 0.0)
    {
        return 0.0;
    }
    members
        .iter()
        .map(|&i| *swaps[i].split())
        .sum()
}

fn lookup_token<'a>(
    tokens: &'a FxHashMap<Bytes, Token>,
    market: &'a MarketDataView<'a>,
    address: &Address,
) -> Option<&'a Token> {
    tokens
        .get(address)
        .or_else(|| market.get_token(address))
}

/// Recomputes every swap's `amount_in`, `amount_out` and gas from its own state, threading the
/// balances through the route the way [`crate::replay::replay_route`] does: a positive `split`
/// takes that fraction of the token's collected total, the remainder swap takes what is left, and
/// a state shared by two swaps sees the first swap's output state on the second.
fn reprice(
    swaps: &mut [Swap],
    tokens: &FxHashMap<Bytes, Token>,
    market: &MarketDataView<'_>,
    input_token: &Address,
    total_in: &BigUint,
) -> Result<(), ReplayError> {
    let mut available: HashMap<Address, BigUint> = HashMap::new();
    available.insert(input_token.clone(), total_in.clone());
    let mut branch_totals: HashMap<Address, BigUint> = HashMap::new();
    let mut post_swap: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();

    for swap in swaps.iter_mut() {
        let token_in = lookup_token(tokens, market, swap.token_in())
            .ok_or_else(|| ReplayError::MissingToken(swap.token_in().clone()))?;
        let token_out = lookup_token(tokens, market, swap.token_out())
            .ok_or_else(|| ReplayError::MissingToken(swap.token_out().clone()))?;

        let branch_total = branch_totals
            .entry(swap.token_in().clone())
            .or_insert_with(|| {
                available
                    .get(swap.token_in())
                    .cloned()
                    .unwrap_or_default()
            })
            .clone();
        let remaining = available
            .entry(swap.token_in().clone())
            .or_default();
        let amount_in = if *swap.split() > 0.0 {
            let (part, _) = split_amount(&branch_total, *swap.split());
            part.min(remaining.clone())
        } else {
            remaining.clone()
        };
        *remaining -= &amount_in;

        let state = post_swap
            .get(swap.component_id())
            .map_or(swap.protocol_state(), Box::as_ref);
        let result = state
            .get_amount_out_guarded(amount_in.clone(), token_in, token_out)
            .map_err(|e| ReplayError::Simulation {
                component_id: swap.component_id().to_string(),
                error: e.to_string(),
            })?;

        *available
            .entry(swap.token_out().clone())
            .or_default() += &result.amount;
        post_swap.insert(swap.component_id().to_string(), result.new_state);
        swap.set_amounts(amount_in, result.amount, result.gas);
    }
    Ok(())
}

/// Writes `improved` onto `quote` when it nets more after gas than the route the quote holds.
///
/// The quote prices gas in its output token through one rate, `(amount_out - amount_out_net_gas)
/// / gas_estimate`; the new gas cost is that rate applied to the new gas. The gas estimate moves
/// by the route's own gas difference, so a refined estimate the quote already carries keeps its
/// refinement.
fn apply_improvement(quote: &mut OrderQuote, improved: ImprovedRoute) {
    let old_out = quote.amount_out().clone();
    let old_net = quote.amount_out_net_gas().clone();
    let old_gas = quote.gas_estimate().clone();
    let old_route_gas = quote
        .route()
        .map(Route::total_gas)
        .unwrap_or_default();
    let new_route_gas = improved.route.total_gas();

    let new_gas = (&old_gas + &new_route_gas)
        .checked_sub(&old_route_gas)
        .unwrap_or(new_route_gas);
    let old_gas_cost = old_out
        .checked_sub(&old_net)
        .unwrap_or_default();
    let new_gas_cost =
        if old_gas.is_zero() { BigUint::ZERO } else { old_gas_cost * &new_gas / &old_gas };
    let new_net = improved
        .amount_out
        .checked_sub(&new_gas_cost)
        .unwrap_or_default();

    if new_net <= old_net {
        record_outcome("kept");
        return;
    }
    debug!(
        order_id = %quote.order_id(),
        %old_net,
        %new_net,
        rfq_legs = improved
            .route
            .swaps()
            .iter()
            .filter(|swap| swap.protocol_state().as_indicatively_priced().is_ok())
            .count(),
        "RFQ overlay replaced hops of the route"
    );
    record_outcome("replaced");
    if let Some(route) = quote.route_mut() {
        *route = improved.route;
    }
    quote.set_amount_out(improved.amount_out);
    quote.set_gas_estimate(new_gas);
    quote.set_amount_out_net_gas(new_net);
}

fn record_outcome(outcome: &'static str) {
    counter!("rfq_overlay_quotes_total", "outcome" => outcome).increment(1);
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_common::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            component, setup_market_weighted_boxed, token, MockProtocolSim, MockRfqSim,
        },
        types::BlockInfo,
    };

    fn view(market: &MarketData) -> MarketDataView<'_> {
        market
            .try_read_blocking()
            .expect("uncontended")
    }

    /// A swap as a worker's route carries it, priced by its own embedded state.
    fn pool_swap(
        pool_id: &str,
        token_in: &Token,
        token_out: &Token,
        amount_in: u64,
        amount_out: u64,
        state: MockProtocolSim,
    ) -> Swap {
        Swap::new(
            pool_id.to_string(),
            "mock".to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(amount_in),
            BigUint::from(amount_out),
            BigUint::from(state.gas),
            component(pool_id, &[token_in.clone(), token_out.clone()]),
            Box::new(state),
        )
    }

    fn route(swaps: Vec<Swap>, tokens: &[&Token]) -> Route {
        let tokens: FxHashMap<Bytes, Token> = tokens
            .iter()
            .map(|t| (t.address.clone(), (*t).clone()))
            .collect();
        Route::new(swaps, tokens).expect("test route must not be empty")
    }

    fn quote_with(route: Route, amount_out: u64, amount_out_net_gas: u64, gas: u64) -> OrderQuote {
        OrderQuote::new(
            "order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(gas),
            BigUint::from(amount_out_net_gas),
            BlockInfo::new(1, "0x01".to_string(), 0),
            "mock".to_string(),
            Bytes::default(),
            Bytes::default(),
            "1".to_string(),
        )
        .with_route(route)
    }

    fn rfq_ids(route: &Route) -> Vec<&str> {
        route
            .swaps()
            .iter()
            .filter(|swap| {
                swap.protocol_state()
                    .as_indicatively_priced()
                    .is_ok()
            })
            .map(Swap::component_id)
            .collect()
    }

    #[test]
    fn test_is_rfq_component() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) = setup_market_weighted_boxed(vec![
            ("uni", &a, &b, Box::new(MockProtocolSim::new(2.0))),
            ("rfq", &a, &b, Box::new(MockRfqSim::new(2.0))),
        ]);
        let view = view(&market);
        let market = view.base_market_state();

        assert!(is_rfq_component(market, market.get_component("rfq").unwrap()));
        assert!(!is_rfq_component(market, market.get_component("uni").unwrap()));
        assert!(!is_rfq_component(market, &component("unknown", &[a, b])));
    }

    #[test]
    fn test_rfq_index_apply_event() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) = setup_market_weighted_boxed(vec![
            ("uni", &a, &b, Box::new(MockProtocolSim::new(2.0))),
            ("rfq", &a, &b, Box::new(MockRfqSim::new(2.0))),
        ]);
        let view = view(&market);
        let mut index = RfqIndex::build(&view);
        assert_eq!(index.components_for(&b.address, &a.address), ["rfq".to_string()]);

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::default(),
                removed_components: vec!["rfq".to_string()],
                updated_components: vec![],
            },
        );
        assert!(index.is_empty());

        index.apply_event(
            &view,
            &MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([
                    ("rfq".to_string(), vec![]),
                    ("uni".to_string(), vec![]),
                ]),
                removed_components: vec![],
                updated_components: vec![],
            },
        );
        assert_eq!(index.components_for(&a.address, &b.address), ["rfq".to_string()]);
    }

    #[test]
    fn test_improve_route_single_hop() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) = setup_market_weighted_boxed(vec![
            ("rfq_low", &a, &b, Box::new(MockRfqSim::new(2.5).with_gas(30_000))),
            ("rfq_high", &a, &b, Box::new(MockRfqSim::new(3.0).with_gas(30_000))),
        ]);
        let view = view(&market);
        let index = RfqIndex::build(&view);
        let route =
            route(vec![pool_swap("uni", &a, &b, 1000, 2000, MockProtocolSim::new(2.0))], &[&a, &b]);

        let improved = improve_route(&route, &view, &index)
            .unwrap()
            .expect("the RFQ pays more than the pool");

        assert_eq!(improved.amount_out, BigUint::from(3000u64));
        let [swap] = improved.route.swaps() else { panic!("one RFQ leg") };
        assert_eq!(swap.component_id(), "rfq_high");
        assert_eq!(swap.amount_in(), &BigUint::from(1000u64));
        assert_eq!(swap.amount_out(), &BigUint::from(3000u64));
        assert_eq!(swap.gas_estimate(), &BigUint::from(30_000u64));
        assert_eq!(*swap.split(), 0.0);
    }

    #[test]
    fn test_improve_route_rfq_pays_less() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) =
            setup_market_weighted_boxed(vec![("rfq", &a, &b, Box::new(MockRfqSim::new(1.5)))]);
        let view = view(&market);
        let index = RfqIndex::build(&view);
        let route =
            route(vec![pool_swap("uni", &a, &b, 1000, 2000, MockProtocolSim::new(2.0))], &[&a, &b]);

        assert!(improve_route(&route, &view, &index)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_improve_route_reprices_downstream_hops() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let c = token(0x0C, "C");
        let (market, _) =
            setup_market_weighted_boxed(vec![("rfq", &a, &b, Box::new(MockRfqSim::new(4.0)))]);
        let view = view(&market);
        let index = RfqIndex::build(&view);
        let route = route(
            vec![
                pool_swap("pool_ab", &a, &b, 1000, 2000, MockProtocolSim::new(2.0)),
                pool_swap("pool_bc", &b, &c, 2000, 6000, MockProtocolSim::new(3.0)),
            ],
            &[&a, &b, &c],
        );

        let improved = improve_route(&route, &view, &index)
            .unwrap()
            .expect("the RFQ pays more on the first hop");

        assert_eq!(rfq_ids(&improved.route), ["rfq"]);
        let [first, second] = improved.route.swaps() else { panic!("two legs") };
        assert_eq!(first.amount_out(), &BigUint::from(4000u64));
        assert_eq!(second.component_id(), "pool_bc");
        assert_eq!(second.amount_in(), &BigUint::from(4000u64));
        assert_eq!(second.amount_out(), &BigUint::from(12_000u64));
        assert_eq!(improved.amount_out, BigUint::from(12_000u64));
    }

    #[test]
    fn test_improve_route_merges_split_hop() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) =
            setup_market_weighted_boxed(vec![("rfq", &a, &b, Box::new(MockRfqSim::new(3.0)))]);
        let view = view(&market);
        let index = RfqIndex::build(&view);
        let route = route(
            vec![
                pool_swap("pool_1", &a, &b, 600, 1200, MockProtocolSim::new(2.0)).with_split(0.6),
                pool_swap("pool_2", &a, &b, 400, 800, MockProtocolSim::new(2.0)),
            ],
            &[&a, &b],
        );

        let improved = improve_route(&route, &view, &index)
            .unwrap()
            .expect("the RFQ pays more than both pools");

        let [swap] = improved.route.swaps() else { panic!("one RFQ leg for the whole hop") };
        assert_eq!(swap.component_id(), "rfq");
        assert_eq!(swap.amount_in(), &BigUint::from(1000u64));
        assert_eq!(swap.amount_out(), &BigUint::from(3000u64));
        assert_eq!(*swap.split(), 0.0);
        assert!(improved.route.validate().is_ok());
    }

    #[test]
    fn test_improve_route_with_exclusive_leg() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) =
            setup_market_weighted_boxed(vec![("rfq", &a, &b, Box::new(MockRfqSim::new(3.0)))]);
        let view = view(&market);
        let index = RfqIndex::build(&view);
        let mut swap = pool_swap("excl", &a, &b, 1000, 2000, MockProtocolSim::new(2.0));
        swap.set_committed_amount_out(BigUint::from(1900u64));
        let route = route(vec![swap], &[&a, &b]);

        assert!(improve_route(&route, &view, &index)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_improve_route_with_pamm_fallback() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) =
            setup_market_weighted_boxed(vec![("rfq", &a, &b, Box::new(MockRfqSim::new(3.0)))]);
        let view = view(&market);
        let index = RfqIndex::build(&view);
        let mut route = route(
            vec![pool_swap("pamm", &a, &b, 1000, 2000, MockProtocolSim::new(2.0))],
            &[&a, &b],
        );
        route.set_fallback_amount_out(BigUint::from(1500u64));

        assert!(improve_route(&route, &view, &index)
            .unwrap()
            .is_none());
    }

    /// Gas is priced through the quote's own rate: 1000 of output for 50_000 gas here, so an RFQ
    /// leg costing 200_000 gas eats a 100-unit gain four times over.
    #[test]
    fn test_apply_improvement_below_net_gas() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let old =
            route(vec![pool_swap("uni", &a, &b, 1000, 2000, MockProtocolSim::new(2.0))], &[&a, &b]);
        let mut quote = quote_with(old, 2000, 1000, 50_000);
        let improved = ImprovedRoute {
            route: route(
                vec![pool_swap(
                    "rfq",
                    &a,
                    &b,
                    1000,
                    2100,
                    MockProtocolSim::new(2.1).with_gas(200_000),
                )],
                &[&a, &b],
            ),
            amount_out: BigUint::from(2100u64),
        };

        apply_improvement(&mut quote, improved);

        assert_eq!(quote.amount_out(), &BigUint::from(2000u64));
        assert_eq!(quote.amount_out_net_gas(), &BigUint::from(1000u64));
        assert_eq!(quote.route().unwrap().swaps()[0].component_id(), "uni");
    }

    #[test]
    fn test_apply_improvement_above_net_gas() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let old =
            route(vec![pool_swap("uni", &a, &b, 1000, 2000, MockProtocolSim::new(2.0))], &[&a, &b]);
        let mut quote = quote_with(old, 2000, 1000, 50_000);
        let improved = ImprovedRoute {
            route: route(
                vec![pool_swap(
                    "rfq",
                    &a,
                    &b,
                    1000,
                    3000,
                    MockProtocolSim::new(3.0).with_gas(30_000),
                )],
                &[&a, &b],
            ),
            amount_out: BigUint::from(3000u64),
        };

        apply_improvement(&mut quote, improved);

        assert_eq!(quote.amount_out(), &BigUint::from(3000u64));
        assert_eq!(quote.gas_estimate(), &BigUint::from(30_000u64));
        assert_eq!(quote.amount_out_net_gas(), &BigUint::from(2400u64));
        assert_eq!(quote.route().unwrap().swaps()[0].component_id(), "rfq");
    }

    #[tokio::test]
    async fn test_overlay_improve() {
        let a = token(0x0A, "A");
        let b = token(0x0B, "B");
        let (market, _) = setup_market_weighted_boxed(vec![
            ("uni", &a, &b, Box::new(MockProtocolSim::new(2.0))),
            ("rfq", &a, &b, Box::new(MockRfqSim::new(3.0).with_gas(30_000))),
        ]);
        let (_events, receiver) = broadcast::channel(4);
        let overlay = RfqOverlay::start(market.clone(), receiver);
        for _ in 0..200 {
            if !overlay.index.read().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(!overlay.index.read().unwrap().is_empty(), "index task must build the index");
        let route =
            route(vec![pool_swap("uni", &a, &b, 1000, 2000, MockProtocolSim::new(2.0))], &[&a, &b]);
        let mut quote = quote_with(route, 2000, 1500, 50_000);

        overlay.improve(&mut quote).await;

        assert_eq!(rfq_ids(quote.route().unwrap()), ["rfq"]);
        assert_eq!(quote.amount_out(), &BigUint::from(3000u64));
        assert_eq!(quote.gas_estimate(), &BigUint::from(30_000u64));
        assert_eq!(quote.amount_out_net_gas(), &BigUint::from(2700u64));
    }
}
