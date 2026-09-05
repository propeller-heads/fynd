//! What an algorithm is given to solve one order.

use crate::{
    derived::SharedDerivedDataRef,
    feed::market_data::{MarketData, StateLabel},
    types::{quote::RouteFilter, Order},
};

/// One order to solve, and everything the algorithm reads to solve it.
///
/// `#[non_exhaustive]`, so what a solve carries can grow without breaking an algorithm outside this
/// crate. Build one with [`SolveRequest::new`] and the `with_` methods.
#[non_exhaustive]
pub struct SolveRequest<'a, G> {
    graph: &'a G,
    market: MarketData,
    order: &'a Order,
    label: Option<StateLabel>,
    derived: Option<SharedDerivedDataRef>,
    filter: RouteFilter,
}

impl<'a, G> SolveRequest<'a, G> {
    /// A solve against the live market state, with no derived data and nothing excluded.
    pub fn new(graph: &'a G, market: MarketData, order: &'a Order) -> Self {
        Self { graph, market, order, label: None, derived: None, filter: RouteFilter::default() }
    }

    /// Reads market state through this overlay, so a request's component overrides apply.
    #[must_use]
    pub fn with_label(mut self, label: Option<StateLabel>) -> Self {
        self.label = label;
        self
    }

    /// The derived data the algorithm may read: token prices, component depths.
    #[must_use]
    pub fn with_derived(mut self, derived: Option<SharedDerivedDataRef>) -> Self {
        self.derived = derived;
        self
    }

    /// Liquidity this solve must not route through.
    #[must_use]
    pub fn with_filter(mut self, filter: RouteFilter) -> Self {
        self.filter = filter;
        self
    }

    /// The graph to search, in the type the algorithm asked for.
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

    /// The derived data, if this deployment computes any.
    pub fn derived(&self) -> Option<&SharedDerivedDataRef> {
        self.derived.as_ref()
    }

    /// What the caller will accept in a route.
    ///
    /// Empty for most quotes. Hop bounds are not here: those are the algorithm's own configuration,
    /// not the caller's to set.
    pub fn filter(&self) -> &RouteFilter {
        &self.filter
    }
}
