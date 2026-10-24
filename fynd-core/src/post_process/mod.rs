//! Route post-processors: steps a pool runs on the route its algorithm returned.
//!
//! A pool config lists post-processors by name. Each worker builds one instance of each, keeps it
//! in step with the market events the worker sees, and runs the instances in config order on the
//! [`RouteResult`](crate::types::RouteResult) its algorithm returned. The worker checks the
//! algorithm's route first, then runs the post-processors, then checks the processed route. The
//! router ranks the quote after that.
//!
//! `rfq_swap_replacer` is built in: a pool config lists it with no registration. A registry adds
//! only custom names.
//!
//! ```ignore
//! // In the deployment's binary:
//! let post_processors = PostProcessorRegistry::new().with_post_processor("rfq", Rfq::new)?;
//! let solver = FyndBuilder::new(..).with_post_processors(post_processors).build()?;
//! ```
//! ```toml
//! # In worker_pools.toml:
//! [pools.mine]
//! algorithm = "most_liquid"
//! post_processors = ["rfq"]
//! ```

mod exclusive;
mod pipeline;
pub mod registry;
mod replace;
mod rfq;
#[cfg(test)]
pub(crate) mod test_utils;

use std::time::Instant;

use async_trait::async_trait;
use tycho_simulation::tycho_common::models::protocol::ProtocolComponent;

pub(crate) use self::pipeline::{PostProcessors, ProcessedRoute};
pub use self::registry::{
    PostProcessorAlreadyRegistered, PostProcessorRegistry, UnknownPostProcessorError,
};
use crate::{
    derived::SharedDerivedDataRef,
    feed::{
        events::MarketEventHandler,
        market_data::{MarketData, StateLabel},
    },
    types::{Order, RouteExclusions, RouteResult},
    worker_pool_router::ExclusiveAccess,
};

/// A step that can replace the route a pool's algorithm returned.
///
/// A worker owns its instances and calls them from one task, so an instance may keep state. It
/// receives the market events the worker's graph manager receives through [`MarketEventHandler`].
/// Events for components of an excluded protocol never arrive, but `initialize` sees the whole
/// market. Events for exclusive components arrive on a `public_only` pool too, because whether a
/// request may use them depends on the caller: see [`PostProcessRequest::exclusive_access`].
///
/// The worker checks the route a processor returns. When the route is invalid, uses a pool the
/// request excluded, or uses a component the pool drops, the worker logs a warning and quotes the
/// route the algorithm returned. A processor owns `net_amount_out`: the worker does not recompute
/// it, and the router ranks quotes on it. For a sell order the worker also keeps the algorithm's
/// route when `net_amount_out` is above the route's amount out.
#[async_trait]
pub trait RoutePostProcessor: MarketEventHandler + Send + Sync {
    /// The worker calls this when it builds its graph: at start, and after it falls behind on
    /// market events. Does nothing by default.
    async fn initialize(&mut self, _market: &MarketData) {}

    /// Returns the route to quote in place of `route`, or `None` to keep `route`. A processor
    /// that cannot improve the route returns `None`, so a failed optimization never fails a
    /// quote.
    ///
    /// The worker stops waiting at [`PostProcessRequest::deadline`] and keeps the route it had
    /// before this call.
    async fn process(
        &self,
        request: &PostProcessRequest<'_>,
        route: &RouteResult,
    ) -> Option<RouteResult>;
}

/// What a post-processor reads about the solve that produced the route.
pub struct PostProcessRequest<'a> {
    order: &'a Order,
    market: &'a MarketData,
    label: Option<&'a StateLabel>,
    derived: &'a SharedDerivedDataRef,
    exclusions: &'a RouteExclusions,
    pool_drops: Option<&'a (dyn Fn(&ProtocolComponent) -> bool + Sync)>,
    exclusive_access: ExclusiveAccess,
    deadline: Instant,
}

impl<'a> PostProcessRequest<'a> {
    /// A request on the base market state, which the worker stops waiting for at `deadline`.
    pub fn new(
        order: &'a Order,
        market: &'a MarketData,
        derived: &'a SharedDerivedDataRef,
        exclusions: &'a RouteExclusions,
        deadline: Instant,
    ) -> Self {
        Self {
            order,
            market,
            label: None,
            derived,
            exclusions,
            pool_drops: None,
            exclusive_access: ExclusiveAccess::Denied,
            deadline,
        }
    }

    /// Sets the rule for dropping components from the pool. See
    /// [`drops_component`](Self::drops_component).
    #[must_use]
    pub fn with_pool_drops(
        mut self,
        pool_drops: &'a (dyn Fn(&ProtocolComponent) -> bool + Sync),
    ) -> Self {
        self.pool_drops = Some(pool_drops);
        self
    }

    /// Sets whether the post-processor may add exclusive components to the route. See
    /// [`exclusive_access`](Self::exclusive_access).
    #[must_use]
    pub fn with_exclusive_access(mut self, access: ExclusiveAccess) -> Self {
        self.exclusive_access = access;
        self
    }

    /// Sets the overlay the solve ran against. `None` is the base market state.
    #[must_use]
    pub fn with_label(mut self, label: Option<&'a StateLabel>) -> Self {
        self.label = label;
        self
    }

    /// The order the route solves.
    pub fn order(&self) -> &'a Order {
        self.order
    }

    /// The shared market. The solve may have read an overlay named by [`label`](Self::label);
    /// [`read_market`](crate::algorithm::paths::read_market) opens the right view.
    pub fn market(&self) -> &'a MarketData {
        self.market
    }

    /// The overlay the solve ran against, if the request named one. Pass it to
    /// [`read_market`](crate::algorithm::paths::read_market) to read the same state.
    pub fn label(&self) -> Option<&'a StateLabel> {
        self.label
    }

    /// The derived data: token prices, component depths.
    pub fn derived(&self) -> &'a SharedDerivedDataRef {
        self.derived
    }

    /// The pools and tokens this route must not use. The worker keeps the algorithm's route when
    /// the returned route uses one.
    pub fn exclusions(&self) -> &'a RouteExclusions {
        self.exclusions
    }

    /// Returns whether the pool drops `component`. The worker rejects processed routes that use
    /// dropped components. Returns `false` when the request has no pool drop rule.
    pub fn drops_component(&self, component: &ProtocolComponent) -> bool {
        self.pool_drops
            .is_some_and(|drops| drops(component))
    }

    /// Whether the post-processor may add exclusive components to the route.
    /// [`ExclusiveAccess::Granted`] only when the caller has exclusive access and the pool leaves
    /// exclusive liquidity out of its graph; [`drops_component`](Self::drops_component) then
    /// keeps exclusive components. [`ExclusiveAccess::Denied`] when the request has no access, and
    /// on a pool that already routes through exclusive liquidity.
    pub fn exclusive_access(&self) -> ExclusiveAccess {
        self.exclusive_access
    }

    /// The time after which the worker stops waiting for the post-processor. The worker sets it
    /// a little before the deadline of the task, so it can still send the route it had in time.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}
