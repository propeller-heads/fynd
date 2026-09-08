//! What an algorithm is given to solve one order.

use crate::{
    derived::SharedDerivedDataRef,
    feed::market_data::{MarketData, StateLabel},
    types::{quote::RouteExclusions, Order},
};

/// One order to solve, and everything the algorithm reads to solve it.
///
/// Build one with [`SolveRequest::new`] and the `with_` methods. Read it through the getters or
/// consume it with [`SolveRequest::into_parts`].
pub struct SolveRequest<'a, G> {
    graph: &'a G,
    market: MarketData,
    order: &'a Order,
    label: Option<StateLabel>,
    derived: Option<SharedDerivedDataRef>,
    exclusions: RouteExclusions,
}

impl<'a, G> SolveRequest<'a, G> {
    /// Consumes the request, returning its graph, order, market, label, derived data and
    /// exclusions.
    ///
    /// Moves the owned fields without cloning them. The graph and order retain their original
    /// borrow lifetime.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        &'a G,
        &'a Order,
        MarketData,
        Option<StateLabel>,
        Option<SharedDerivedDataRef>,
        RouteExclusions,
    ) {
        (self.graph, self.order, self.market, self.label, self.derived, self.exclusions)
    }

    /// A solve against the live market state, with no derived data and nothing excluded.
    pub fn new(graph: &'a G, market: MarketData, order: &'a Order) -> Self {
        Self {
            graph,
            market,
            order,
            label: None,
            derived: None,
            exclusions: RouteExclusions::default(),
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
        self.exclusions = exclusions;
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
}
