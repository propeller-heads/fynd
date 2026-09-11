//! Re-ranking a request's candidates once a market maker has signed a quote for the best one.
//!
//! An indicatively priced leg — bebop, hashflow and the other RFQ venues — is solved against
//! streamed price levels, but only the maker's signed quote is executable. Ranking on the level
//! price hands the order to the route that overstates its output the most, and the AMM route it
//! beat would have delivered more. That winner then reaches encoding, where the maker signs for
//! less, and the calldata carries a floor from the level price that the signed quote cannot meet.
//!
//! So the best candidate's RFQ legs are requoted here, before the winner is fixed, and the
//! candidates are ranked again on the signed amounts.
//!
//! A requote costs a live request to the maker, and the encoder makes its own when it builds the
//! calldata, so a requoted candidate is quoted twice. This runs only when the outcome can change:
//! an order with one candidate has nothing to switch to, and a next-best candidate too far behind
//! cannot overtake the best one.

use std::collections::HashMap;

use metrics::{counter, histogram};
use num_bigint::BigUint;
use num_traits::{CheckedSub, ToPrimitive};
use tycho_simulation::tycho_common::models::protocol::GetAmountOutParams;

use crate::{
    replay::replay_route_at_quote_time,
    types::{OrderQuote, QuoteStatus},
    worker_pool_router::BPS_DENOMINATOR,
};

/// How far behind the best candidate the next-best one may sit and still make requoting the best
/// one worthwhile, in basis points.
///
/// A requote moves the best candidate down, never up, so a next-best candidate further behind
/// than the largest level-to-signed gap cannot overtake it, and the maker request is wasted. 1000
/// bps sits well above the gaps `rfq_signed_quote_deviation_bps` records: skipping a requote that
/// would have changed the winner costs the user the difference, while making one that changes
/// nothing costs a request.
const NEXT_BEST_MARGIN_BPS: u32 = 1_000;

/// Requotes each order's best candidate against its market makers and re-ranks on the result.
///
/// Each inner list of `per_order` is ranked best first, on entry and on return. A candidate whose
/// requote fails keeps its level-priced amounts and its place, so a failed requote never drops an
/// order from the quote path.
pub(super) async fn rerank_on_signed_quotes(per_order: &mut [Vec<OrderQuote>]) {
    for candidates in per_order.iter_mut() {
        rerank_order(candidates, NEXT_BEST_MARGIN_BPS).await;
    }
}

/// Requotes one order's best candidate and re-sorts its candidates.
async fn rerank_order(candidates: &mut [OrderQuote], margin_bps: u32) {
    if !worth_requoting(candidates, margin_bps) {
        return;
    }

    let Some(signed) = request_signed_amounts(&candidates[0]).await else {
        return;
    };
    let Some(best) = candidates.first_mut() else {
        return;
    };
    if !apply_signed_amounts(best, &signed) {
        return;
    }

    let requoted_pool = best.worker_pool().to_string();
    candidates.sort_by(|a, b| {
        b.amount_out_net_gas()
            .cmp(a.amount_out_net_gas())
    });
    let outcome = if candidates[0].worker_pool() == requoted_pool { "held" } else { "changed" };
    counter!("rfq_rerank_total", "outcome" => outcome, "pool" => requoted_pool).increment(1);
}

/// Whether requoting this order's best candidate can change which candidate wins.
///
/// Four things have to hold: a next-best candidate exists, both it and the best candidate
/// succeeded, the best candidate has at least one indicatively priced swap, and the next-best one
/// sits within `margin_bps` of it.
fn worth_requoting(candidates: &[OrderQuote], margin_bps: u32) -> bool {
    let [best, next_best, ..] = candidates else {
        return false;
    };
    if best.status() != QuoteStatus::Success || next_best.status() != QuoteStatus::Success {
        return false;
    }
    if indicatively_priced_swaps(best).is_empty() {
        return false;
    }
    // `next_best * BPS_DENOMINATOR >= best * (BPS_DENOMINATOR - margin)`, in integer arithmetic.
    next_best.amount_out_net_gas() * BigUint::from(BPS_DENOMINATOR) >=
        best.amount_out_net_gas() *
            BigUint::from(BPS_DENOMINATOR - margin_bps.min(BPS_DENOMINATOR))
}

/// Positions, within the quote's route, of the swaps a market maker prices.
fn indicatively_priced_swaps(quote: &OrderQuote) -> Vec<usize> {
    quote
        .route()
        .map_or_else(Vec::new, |route| {
            route
                .swaps()
                .iter()
                .enumerate()
                .filter(|(_, swap)| {
                    swap.protocol_state()
                        .as_indicatively_priced()
                        .is_ok()
                })
                .map(|(index, _)| index)
                .collect()
        })
}

/// Asks every market maker in `quote`'s route to sign for the amount the route sends it, keyed by
/// the swap's position in `route.swaps()`.
///
/// Returns `None` when the quote carries no route, when the route has no indicatively priced
/// swap, or when a maker refuses: a partly signed route still carries a level price, so
/// re-ranking on it would repeat the bug.
async fn request_signed_amounts(quote: &OrderQuote) -> Option<HashMap<usize, BigUint>> {
    let route = quote.route()?;
    let mut signed = HashMap::new();
    for index in indicatively_priced_swaps(quote) {
        let swap = route.swaps().get(index)?;
        let params = GetAmountOutParams {
            amount_in: swap.amount_in().clone(),
            token_in: swap.token_in().clone(),
            token_out: swap.token_out().clone(),
            sender: quote.sender().clone(),
            receiver: quote.receiver().clone(),
        };
        let quote_result = swap
            .protocol_state()
            .as_indicatively_priced()
            .ok()?
            .request_signed_quote(params)
            .await;
        match quote_result {
            Ok(signed_quote) => {
                record_signed_quote_deviation(
                    swap.protocol(),
                    swap.amount_out(),
                    &signed_quote.amount_out,
                );
                signed.insert(index, signed_quote.amount_out);
            }
            Err(error) => {
                counter!(
                    "rfq_requote_failures_total",
                    "stage" => "signing",
                    "protocol" => swap.protocol().to_string()
                )
                .increment(1);
                tracing::debug!(
                    protocol = swap.protocol(),
                    component = swap.component_id(),
                    %error,
                    "market maker did not sign a quote for the best candidate"
                );
                return None;
            }
        }
    }
    (!signed.is_empty()).then_some(signed)
}

/// Replaces `best`'s level-priced output with what the route yields on the signed amounts.
///
/// Returns whether it replaced them. It keeps the quote's existing gas cost in output-token
/// units, because the route and its pools are unchanged — only its indicative legs repriced.
fn apply_signed_amounts(best: &mut OrderQuote, signed: &HashMap<usize, BigUint>) -> bool {
    let Some(route) = best.route() else {
        return false;
    };
    let replayed = match replay_route_at_quote_time(route, signed) {
        Ok(replayed) => replayed,
        Err(error) => {
            counter!("rfq_requote_failures_total", "stage" => "replay").increment(1);
            tracing::debug!(%error, "could not re-price the best candidate on its signed quotes");
            return false;
        }
    };

    let gas_cost = best
        .amount_out()
        .checked_sub(best.amount_out_net_gas())
        .unwrap_or(BigUint::ZERO);
    let net = replayed
        .amount_out
        .checked_sub(&gas_cost)
        .unwrap_or(BigUint::ZERO);
    best.set_amount_out(replayed.amount_out);
    best.set_amount_out_net_gas(net);
    true
}

/// Records how far a signed quote landed from the price levels the leg was solved on.
///
/// The deviation separates a maker requoting lower from the route drifting between quote and
/// execution, which need different fixes.
fn record_signed_quote_deviation(protocol: &str, level_amount: &BigUint, signed_amount: &BigUint) {
    let (Some(level), Some(signed)) = (level_amount.to_f64(), signed_amount.to_f64()) else {
        return;
    };
    if level <= 0.0 {
        return;
    }
    let bps = (signed - level) / level * f64::from(BPS_DENOMINATOR);
    if bps.is_finite() {
        histogram!("rfq_signed_quote_deviation_bps", "protocol" => protocol.to_string())
            .record(bps);
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;

    use async_trait::async_trait;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::{
        dto::ProtocolStateDelta,
        models::token::Token,
        simulation::{
            errors::{SimulationError, TransitionError},
            indicatively_priced::{IndicativelyPriced, SignedQuote},
            protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        },
        Bytes,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component, token, MockProtocolSim},
        types::{BlockInfo, Route, Swap},
    };

    /// A pool that prices indicatively and signs for `signs` when asked to commit.
    ///
    /// A `signs` of `None` makes the pool refuse, standing in for a maker that withdrew its
    /// quote.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct SigningSim {
        inner: MockProtocolSim,
        signs: Option<u64>,
    }

    impl SigningSim {
        fn new(spot_price: f64, signs: Option<u64>) -> Self {
            Self { inner: MockProtocolSim::new(spot_price), signs }
        }
    }

    #[typetag::serde]
    impl ProtocolSim for SigningSim {
        fn fee(&self) -> f64 {
            self.inner.fee()
        }
        fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
            self.inner.spot_price(base, quote)
        }
        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            self.inner
                .get_amount_out(amount_in, token_in, token_out)
        }
        fn get_limits(
            &self,
            sell_token: Bytes,
            buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            self.inner
                .get_limits(sell_token, buy_token)
        }
        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &std::collections::HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("SigningSim holds a fixed state")
        }
        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn eq(&self, _other: &dyn ProtocolSim) -> bool {
            false
        }
        fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
            Ok(self)
        }
    }

    #[async_trait]
    impl IndicativelyPriced for SigningSim {
        async fn request_signed_quote(
            &self,
            params: GetAmountOutParams,
        ) -> Result<SignedQuote, SimulationError> {
            let signs = self.signs.ok_or_else(|| {
                SimulationError::FatalError("maker withdrew its quote".to_string())
            })?;
            Ok(SignedQuote {
                base_token: params.token_in,
                quote_token: params.token_out,
                amount_in: params.amount_in,
                amount_out: BigUint::from(signs),
                quote_attributes: std::collections::HashMap::new(),
            })
        }
    }

    /// A single-swap candidate quoting `level_out`, on a pool that signs for `signs`.
    fn candidate(pool: &str, level_out: u64, signs: Option<u64>, indicative: bool) -> OrderQuote {
        let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
        let state: Box<dyn ProtocolSim> = if indicative {
            Box::new(SigningSim::new(level_out as f64 / 1_000.0, signs))
        } else {
            Box::new(MockProtocolSim::new(level_out as f64 / 1_000.0))
        };
        let swap = Swap::new(
            "pool".to_string(),
            "rfq:mock".to_string(),
            a.address.clone(),
            b.address.clone(),
            BigUint::from(1_000u64),
            BigUint::from(level_out),
            BigUint::ZERO,
            component("pool", &[a.clone(), b.clone()]),
            state,
        );
        let tokens: FxHashMap<Bytes, Token> =
            [(a.address.clone(), a.clone()), (b.address.clone(), b.clone())]
                .into_iter()
                .collect();
        let route = Route::new(vec![swap], tokens).expect("test route must not be empty");

        let quote = OrderQuote::new(
            "o1".to_string(),
            QuoteStatus::Success,
            BigUint::from(1_000u64),
            BigUint::from(level_out),
            BigUint::ZERO,
            BigUint::from(level_out),
            BlockInfo::new(1, "0x1".to_string(), 1),
            "algo".to_string(),
            Bytes::from(vec![0xAA; 20]),
            Bytes::from(vec![0xBB; 20]),
            "1".to_string(),
        );
        let mut quote = quote.with_route(route);
        quote.set_worker_pool(pool.to_string());
        quote
    }

    fn pools(candidates: &[OrderQuote]) -> Vec<&str> {
        candidates
            .iter()
            .map(OrderQuote::worker_pool)
            .collect()
    }

    /// The best candidate wins on a level price, the maker signs for less, and the next-best
    /// candidate takes the order.
    #[tokio::test]
    async fn test_rerank_order_signed_below_next_best() {
        let mut candidates =
            vec![candidate("rfq", 2_000, Some(1_500), true), candidate("amm", 1_800, None, false)];

        rerank_order(&mut candidates, NEXT_BEST_MARGIN_BPS).await;

        assert_eq!(pools(&candidates), vec!["amm", "rfq"], "1_800 beats the signed 1_500");
        assert_eq!(
            candidates[1].amount_out(),
            &BigUint::from(1_500u64),
            "the requoted candidate carries what the maker signed, not what its levels promised"
        );
    }

    /// A maker that signs close to its levels keeps the order, at the signed amount.
    #[tokio::test]
    async fn test_rerank_order_signed_above_next_best() {
        let mut candidates =
            vec![candidate("rfq", 2_000, Some(1_950), true), candidate("amm", 1_800, None, false)];

        rerank_order(&mut candidates, NEXT_BEST_MARGIN_BPS).await;

        assert_eq!(pools(&candidates), vec!["rfq", "amm"]);
        assert_eq!(candidates[0].amount_out(), &BigUint::from(1_950u64));
    }

    /// A maker that will not sign leaves the ranking as it was: the quote path must not lose an
    /// order because a requote failed.
    #[tokio::test]
    async fn test_rerank_order_refused_signature() {
        let mut candidates =
            vec![candidate("rfq", 2_000, None, true), candidate("amm", 1_800, None, false)];

        rerank_order(&mut candidates, NEXT_BEST_MARGIN_BPS).await;

        assert_eq!(pools(&candidates), vec!["rfq", "amm"]);
        assert_eq!(candidates[0].amount_out(), &BigUint::from(2_000u64));
    }

    /// An order with one candidate has nothing to switch to, so no maker is asked.
    #[tokio::test]
    async fn test_rerank_order_single_candidate() {
        let mut candidates = vec![candidate("rfq", 2_000, Some(1_000), true)];

        rerank_order(&mut candidates, NEXT_BEST_MARGIN_BPS).await;

        assert_eq!(candidates[0].amount_out(), &BigUint::from(2_000u64));
    }

    /// An all-AMM best candidate is priced on executed liquidity, so there is nothing to
    /// requote.
    #[tokio::test]
    async fn test_rerank_order_amm_best_candidate() {
        let mut candidates =
            vec![candidate("amm", 2_000, None, false), candidate("amm2", 1_900, None, false)];

        rerank_order(&mut candidates, NEXT_BEST_MARGIN_BPS).await;

        assert_eq!(pools(&candidates), vec!["amm", "amm2"]);
        assert_eq!(candidates[0].amount_out(), &BigUint::from(2_000u64));
    }

    /// A next-best candidate too far behind cannot win however badly the maker requotes, so the
    /// request is not made.
    #[tokio::test]
    async fn test_rerank_order_distant_next_best() {
        let mut candidates =
            vec![candidate("rfq", 2_000, Some(100), true), candidate("amm", 500, None, false)];

        rerank_order(&mut candidates, NEXT_BEST_MARGIN_BPS).await;

        assert_eq!(pools(&candidates), vec!["rfq", "amm"]);
        assert_eq!(candidates[0].amount_out(), &BigUint::from(2_000u64), "no maker was asked");
    }
}
