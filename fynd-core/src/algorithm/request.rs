//! What an algorithm is given to solve one order.

use std::{sync::Arc, time::Instant};

use crate::{
    derived::SharedDerivedDataRef,
    feed::market_data::{MarketData, StateLabel},
    types::{quote::RouteExclusions, Order, Variation},
};

/// A [`SolveRequest`] taken apart, so an algorithm owns the market and the derived data.
pub struct SolveParts<'a, G> {
    /// The graph to search, in the algorithm's own `GraphType`.
    pub graph: &'a G,
    /// The order to solve.
    pub order: &'a Order,
    /// The market to read state from. An algorithm takes its own lock.
    pub market: MarketData,
    /// The overlay to read market state through, if the request named one.
    pub label: Option<StateLabel>,
    /// The derived data the algorithm declared it needs.
    pub derived: Option<SharedDerivedDataRef>,
    /// The pools and tokens this solve must not use.
    pub exclusions: Arc<RouteExclusions>,
    /// The variations to solve beside the main route, in request order, each with the pools and
    /// tokens it must not use.
    pub variations: Arc<[(Variation, Arc<RouteExclusions>)]>,
    /// The caller's deadline for receiving the result, if set.
    pub deadline: Option<Instant>,
}

/// One order to solve, and everything the algorithm reads to solve it.
pub struct SolveRequest<'a, G> {
    graph: &'a G,
    market: MarketData,
    order: &'a Order,
    label: Option<StateLabel>,
    derived: Option<SharedDerivedDataRef>,
    exclusions: Arc<RouteExclusions>,
    variations: Arc<[(Variation, Arc<RouteExclusions>)]>,
    deadline: Option<Instant>,
}

impl<'a, G> SolveRequest<'a, G> {
    /// The request's parts, moved out of it.
    #[must_use]
    pub fn into_parts(self) -> SolveParts<'a, G> {
        SolveParts {
            graph: self.graph,
            order: self.order,
            market: self.market,
            label: self.label,
            derived: self.derived,
            exclusions: self.exclusions,
            variations: self.variations,
            deadline: self.deadline,
        }
    }

    /// A solve against the live market state, with no derived data, nothing excluded, no
    /// variations and no deadline.
    pub fn new(graph: &'a G, market: MarketData, order: &'a Order) -> Self {
        Self {
            graph,
            market,
            order,
            label: None,
            derived: None,
            exclusions: Arc::new(RouteExclusions::default()),
            variations: Arc::from(Vec::new()),
            deadline: None,
        }
    }

    /// The solve reads market state through this overlay, so the request's component overrides
    /// apply.
    #[must_use]
    pub fn with_label(mut self, label: StateLabel) -> Self {
        self.label = Some(label);
        self
    }

    /// The derived data the algorithm may read: token prices, component depths.
    #[must_use]
    pub fn with_derived(mut self, derived: SharedDerivedDataRef) -> Self {
        self.derived = Some(derived);
        self
    }

    /// Liquidity this solve must not route through.
    #[must_use]
    pub fn with_exclusions(mut self, exclusions: RouteExclusions) -> Self {
        self.exclusions = Arc::new(exclusions);
        self
    }

    pub(crate) fn with_shared_exclusions(mut self, exclusions: Arc<RouteExclusions>) -> Self {
        self.exclusions = exclusions;
        self
    }

    /// Also solve the order under these variations, read by [`crate::Algorithm::find_routes`].
    ///
    /// Each variation comes with its exclusions: the request's, plus the pools of the protocols
    /// the variation excludes.
    #[must_use]
    pub fn with_variations(
        mut self,
        variations: impl Into<Arc<[(Variation, Arc<RouteExclusions>)]>>,
    ) -> Self {
        self.variations = variations.into();
        self
    }

    /// Asks the algorithm to stop solving variations at `deadline`, when the caller stops waiting
    /// for the answer. An algorithm can ignore it.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// The graph to search, in the algorithm's own `GraphType`.
    pub fn graph(&self) -> &'a G {
        self.graph
    }

    /// The market to read state from. Algorithms take their own locks.
    pub fn market(&self) -> &MarketData {
        &self.market
    }

    /// The order to solve.
    pub fn order(&self) -> &'a Order {
        self.order
    }

    /// The overlay to read market state through, if the request named one.
    pub fn label(&self) -> Option<&StateLabel> {
        self.label.as_ref()
    }

    /// The derived data, if the caller passed any.
    pub fn derived(&self) -> Option<&SharedDerivedDataRef> {
        self.derived.as_ref()
    }

    /// The pools and tokens this solve request must not use.
    pub fn exclusions(&self) -> &RouteExclusions {
        &self.exclusions
    }

    /// The variations to solve beside the main route, in request order, each with the pools and
    /// tokens it must not use.
    pub fn variations(&self) -> &[(Variation, Arc<RouteExclusions>)] {
        &self.variations
    }

    /// The caller's deadline for receiving the result, if set.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}
