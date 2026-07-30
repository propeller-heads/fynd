//! Re-execute an already-built [`Route`] against a (possibly newer)
//! [`MarketState`](crate::feed::market_data::MarketState).
//!
//! A route emitted by a solving algorithm pins pools, token order, and split fractions. Replaying
//! it against a later block's pool states answers "what would this exact route have produced at
//! that state" — the in-process equivalent of submitting the already-encoded transaction at that
//! block. Used by tooling (e.g. `hindsight`) to measure slippage between quote time and
//! execution time.

use std::collections::HashMap;

use num_bigint::BigUint;
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim};

use crate::{
    algorithm::{sim_guard::GuardedProtocolSim, split_primitives::split_amount},
    feed::market_data::MarketState,
    types::{ComponentId, Route, Swap},
};

/// The outcome of replaying a route: final output and summed per-swap gas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteReplay {
    /// Amount of the route's output token produced.
    pub amount_out: BigUint,
    /// Sum of the per-swap gas estimates reported by the simulations.
    pub gas: BigUint,
}

/// Why a route could not be replayed against a market state.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The route carries no swaps (only reachable via deserialization — [`Route::new`] rejects
    /// an empty swap list).
    #[error("route has no swaps")]
    EmptyRoute,
    /// The market state carries no simulation state for a pool in the route (removed by the feed
    /// or filtered out since the route was built).
    #[error("no simulation state for component {0}")]
    MissingState(ComponentId),
    /// A token in the route is missing from the market state's token registry.
    #[error("token {0} missing from the market state")]
    MissingToken(Address),
    /// A pool simulation failed (e.g. the pool was paused or its liquidity vanished).
    #[error("simulation failed on component {component_id}: {error}")]
    Simulation {
        /// The pool whose simulation failed.
        component_id: ComponentId,
        /// The underlying simulation error.
        error: String,
    },
}

/// Replay `route` against `market`, producing the output its swaps yield at that state.
///
/// Swaps execute in the route's emitted order, which is topological: every swap producing a token
/// runs before any swap consuming it. Split fractions are interpreted exactly as routes encode
/// them — within the swaps consuming one token, each positive `split` takes that fraction of the
/// token's collected balance, and the final swap of the group (split `0.0`) takes the remainder.
/// Post-swap pool states are threaded, so a pool shared by two swaps sees depleted reserves on
/// the second. Each swap's embedded quote-time `protocol_state` is deliberately ignored: pools
/// and tokens are resolved from `market`, the state being replayed against.
///
/// # Errors
///
/// Returns [`ReplayError`] when a pool's simulation state or a token is missing from `market`,
/// or a pool simulation fails.
pub fn replay_route(route: &Route, market: &MarketState) -> Result<RouteReplay, ReplayError> {
    let swaps = route.swaps();
    let (Some(input_token), Some(output_token)) = (route.input_token(), route.output_token())
    else {
        return Err(ReplayError::EmptyRoute);
    };
    let total_in: BigUint = swaps
        .iter()
        .filter(|swap| *swap.token_in() == input_token)
        .map(Swap::amount_in)
        .sum();

    // Collected balance per token. A token's balance is complete before its first consuming swap
    // runs (topological order), and `branch_totals` snapshots it at that moment: positive splits
    // are fractions of that total, not of the running remainder.
    let mut available: HashMap<Address, BigUint> = HashMap::new();
    available.insert(input_token, total_in);
    let mut branch_totals: HashMap<Address, BigUint> = HashMap::new();
    let mut post_swap: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
    let mut total_gas = BigUint::ZERO;

    for swap in swaps {
        let token_in = market
            .get_token(swap.token_in())
            .ok_or_else(|| ReplayError::MissingToken(swap.token_in().clone()))?;
        let token_out = market
            .get_token(swap.token_out())
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
            // Flooring in split_amount keeps the sum of parts at or under the total, but cap at
            // the remainder anyway so a malformed split can never underflow the balance.
            let (part, _) = split_amount(&branch_total, *swap.split());
            part.min(remaining.clone())
        } else {
            remaining.clone()
        };
        *remaining -= &amount_in;

        let sim = post_swap
            .get(swap.component_id())
            .map(|state| state.as_ref())
            .or_else(|| market.get_simulation_state(swap.component_id()))
            .ok_or_else(|| ReplayError::MissingState(swap.component_id().to_string()))?;
        let result = sim
            .get_amount_out_guarded(amount_in, token_in, token_out)
            .map_err(|e| ReplayError::Simulation {
                component_id: swap.component_id().to_string(),
                error: e.to_string(),
            })?;

        total_gas += &result.gas;
        *available
            .entry(swap.token_out().clone())
            .or_default() += &result.amount;
        post_swap.insert(swap.component_id().to_string(), result.new_state);
    }

    let amount_out = available
        .remove(&output_token)
        .unwrap_or_default();
    Ok(RouteReplay { amount_out, gas: total_gas })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::{component, token, ConstantProductSim, MockProtocolSim};

    fn make_market(
        pools: Vec<(
            &str,
            Vec<tycho_simulation::tycho_common::models::token::Token>,
            Box<dyn ProtocolSim>,
        )>,
    ) -> MarketState {
        let mut market = MarketState::new();
        for (pool_id, tokens, sim) in pools {
            market.upsert_components(std::iter::once(component(pool_id, &tokens)));
            market.update_states([(pool_id.to_string(), sim)]);
            market.upsert_tokens(tokens);
        }
        market
    }

    /// A swap as a route would carry it. The embedded protocol state is a decoy with an absurd
    /// price so any test that accidentally simulates against it fails loudly.
    fn route_swap(
        pool_id: &str,
        token_in: &tycho_simulation::tycho_common::models::token::Token,
        token_out: &tycho_simulation::tycho_common::models::token::Token,
        amount_in: u64,
        split: f64,
    ) -> Swap {
        Swap::new(
            pool_id.to_string(),
            "mock".to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(amount_in),
            BigUint::ZERO,
            BigUint::ZERO,
            component(pool_id, &[token_in.clone(), token_out.clone()]),
            Box::new(MockProtocolSim::new(1_000_000.0)),
        )
        .with_split(split)
    }

    fn route(swaps: Vec<Swap>) -> Route {
        Route::new(swaps, HashMap::new()).expect("test route must not be empty")
    }

    #[test]
    fn sequential_route_threads_amounts_through_market_state() {
        // A→B→C at market prices 2.0 then 3.0: 1000 → 2000 → 6000. The swaps embed a decoy
        // state, so this also proves replay reads the market, not the route's quote-time states.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(50_000)),
            ),
            (
                "pool_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0).with_gas(70_000)),
            ),
        ]);
        let route = route(vec![
            route_swap("pool_ab", &token_a, &token_b, 1_000, 0.0),
            route_swap("pool_bc", &token_b, &token_c, 2_000, 0.0),
        ]);

        let replay = replay_route(&route, &market).unwrap();
        assert_eq!(replay.amount_out, BigUint::from(6_000u64));
        assert_eq!(replay.gas, BigUint::from(120_000u64));
    }

    #[test]
    fn split_route_divides_by_fraction_with_remainder() {
        // 1000 split 60/40 across two parallel pools: 600*2 + 400*3 = 2400. The second swap
        // carries split 0.0 (remainder convention).
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            ("pool_1", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("pool_2", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(3.0))),
        ]);
        let route = route(vec![
            route_swap("pool_1", &token_a, &token_b, 600, 0.6),
            route_swap("pool_2", &token_a, &token_b, 400, 0.0),
        ]);

        let replay = replay_route(&route, &market).unwrap();
        assert_eq!(replay.amount_out, BigUint::from(2_400u64));
    }

    #[test]
    fn splits_are_fractions_of_the_collected_total_not_the_remainder() {
        // Three-way split 0.5 / 0.3 / remainder of 1000: 500, 300, 200 — the 0.3 applies to the
        // full 1000, not to the 500 left after the first swap.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            ("pool_1", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(1.0))),
            ("pool_2", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("pool_3", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(4.0))),
        ]);
        let route = route(vec![
            route_swap("pool_1", &token_a, &token_b, 500, 0.5),
            route_swap("pool_2", &token_a, &token_b, 300, 0.3),
            route_swap("pool_3", &token_a, &token_b, 200, 0.0),
        ]);

        // 500*1 + 300*2 + 200*4 = 1900.
        let replay = replay_route(&route, &market).unwrap();
        assert_eq!(replay.amount_out, BigUint::from(1_900u64));
    }

    #[test]
    fn shared_pool_sees_depleted_reserves() {
        // Two swaps through the same constant-product pool: the second must run on the first's
        // post-swap reserves, matching one full-amount swap up to rounding.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let cp = ConstantProductSim {
            reserve_0: BigUint::from(10_000u64),
            reserve_1: BigUint::from(10_000u64),
            gas: 50_000,
        };
        let market = make_market(vec![(
            "pool",
            vec![token_a.clone(), token_b.clone()],
            Box::new(cp.clone()),
        )]);
        let route = route(vec![
            route_swap("pool", &token_a, &token_b, 500, 0.5),
            route_swap("pool", &token_a, &token_b, 500, 0.0),
        ]);

        let replay = replay_route(&route, &market).unwrap();
        let full_swap = cp
            .get_amount_out(BigUint::from(1_000u64), &token_a, &token_b)
            .unwrap()
            .amount;
        let diff = if replay.amount_out > full_swap {
            &replay.amount_out - &full_swap
        } else {
            &full_swap - &replay.amount_out
        };
        assert!(diff <= BigUint::from(2u32), "split through one pool must match one full swap");
    }

    #[test]
    fn changed_market_state_changes_the_output() {
        // The pool moved in the trade's favor between quote and replay: same route, more output.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(2.5)),
        )]);
        // The route was quoted at price 2.0 (amount_out recorded then was 2000).
        let route = route(vec![route_swap("pool", &token_a, &token_b, 1_000, 0.0)]);

        let replay = replay_route(&route, &market).unwrap();
        assert_eq!(replay.amount_out, BigUint::from(2_500u64));
    }

    #[test]
    fn missing_simulation_state_errors() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let mut market = make_market(vec![]);
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);
        let route = route(vec![route_swap("gone", &token_a, &token_b, 1_000, 0.0)]);

        let err = replay_route(&route, &market).unwrap_err();
        assert!(matches!(err, ReplayError::MissingState(id) if id == "gone"));
    }

    #[test]
    fn missing_token_errors() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(2.0)),
        )]);
        let unknown = token(0x42, "X");
        let route = route(vec![route_swap("pool", &unknown, &token_b, 1_000, 0.0)]);

        let err = replay_route(&route, &market).unwrap_err();
        assert!(matches!(err, ReplayError::MissingToken(addr) if addr == unknown.address));
    }
}
