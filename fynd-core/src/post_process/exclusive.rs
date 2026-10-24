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
            ReplacementRule, RouteEvaluator, Search, Snapshot, Target,
        },
        PostProcessRequest, RoutePostProcessor,
    },
    types::{RouteResult, Swap},
    worker_pool_router::ExclusiveAccess,
};

/// Replaces one swap or a segment of a sell order's route with one swap through an exclusive
/// component. It keeps a replacement only when the net amount out rises and the route can commit
/// to a price.
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

/// Replaces a target with one swap through an exclusive component.
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
        find_whole_candidate(leg, target, swaps, evaluator, min_net_amount_out)
            .and_then(|whole| ExclusiveRule.commit(whole, evaluator))
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
