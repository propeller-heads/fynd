//! What an algorithm is given to solve one order.

use crate::{
    derived::SharedDerivedDataRef,
    feed::market_data::{MarketData, StateLabel},
    types::{quote::RouteExclusions, Order},
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
    exclusions: RouteExclusions,
}

impl<'a, G> SolveRequest<'a, G> {
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

    /// The pools and tokens this solve must not use.
    ///
    /// The caller's [`crate::RouteFilter`] is already resolved: a protocol system it named is here
    /// as that system's pools. Hop bounds stay in the algorithm's own configuration; a caller does
    /// not set them.
    pub fn exclusions(&self) -> &RouteExclusions {
        &self.exclusions
    }
}
