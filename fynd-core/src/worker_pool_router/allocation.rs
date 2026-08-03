//! Decides which worker pools serve an order, before any work is dispatched.
//!
//! Each worker pool declares a [`WorkerPoolAdmission`] — which requests it serves. Each order is
//! classified into a [`RequestClass`] — what it is and what it may reach. [`allocate`] intersects
//! the two into an [`Allocation`], and the router fans out to those worker pools only.
//!
//! Deciding before fan-out rather than filtering candidates afterwards means a worker pool that
//! does not serve a request costs it no CPU and no latency. It also leaves the router with a
//! single source of truth: early-return gating, the ranking split and the surplus overlay all read
//! the allocation, so an unentitled request cannot leak exclusive liquidity through a branch that
//! was missed.
//!
//! Both sides grow one field per dimension. Trade size derived from `amount_in` — routing small
//! orders to fast algorithms and large ones to algorithms that handle them better — is the next
//! one: a field on `RequestClass`, a matching condition in `WorkerPoolAdmission::admits`.

use std::collections::HashMap;

use super::{LiquidityScope, SolverPoolHandle};

/// Whether a single request may route through exclusive liquidity.
///
/// Exclusive liquidity is reserved for selected clients, so the entitlement is decided at the
/// request boundary by the operator — the RPC layer reads it from a header set by the
/// authenticating proxy — and never from anything a caller can put in a
/// `QuoteRequest`.
///
/// `Denied` is not an error: the request is quoted from public liquidity alone, which is the same
/// answer a deployment without exclusive worker pools would give. A missing entitlement costs
/// price rather than breaking the request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExclusiveAccess {
    /// Route through public liquidity only. Default: entitlement must be granted explicitly.
    #[default]
    Denied,
    /// Route through exclusive components too, capturing surplus above the public reference.
    Granted,
}

/// What an order is, and what it is allowed to reach.
///
/// Built once per order from the trust-boundary entitlement plus facts about the order itself.
/// Matched against every worker pool's [`WorkerPoolAdmission`] to decide the fan-out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestClass {
    /// Whether this order may be served by exclusive-access worker pools.
    exclusive_access: ExclusiveAccess,
}

impl RequestClass {
    /// Classifies an order whose only distinguishing property is the caller's entitlement.
    pub(crate) fn new(exclusive_access: ExclusiveAccess) -> Self {
        Self { exclusive_access }
    }
}

/// Which requests a worker pool serves.
///
/// Every field is a condition; a worker pool is selected only when all of them hold for the
/// request. Built from the worker pool's `worker_pools.toml` entry and held by its
/// [`SolverPoolHandle`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WorkerPoolAdmission {
    /// The liquidity this worker pool's threads can see. Decides which entitlements it serves.
    liquidity_scope: LiquidityScope,
}

impl WorkerPoolAdmission {
    /// Returns the liquidity scope this rule admits on.
    pub(crate) fn liquidity_scope(self) -> LiquidityScope {
        self.liquidity_scope
    }

    /// Sets the liquidity scope this rule admits on.
    pub(crate) fn with_liquidity_scope(mut self, liquidity_scope: LiquidityScope) -> Self {
        self.liquidity_scope = liquidity_scope;
        self
    }

    /// Returns whether a worker pool with this rule serves `class`.
    fn admits(self, class: RequestClass) -> bool {
        match self.liquidity_scope {
            // Public liquidity is available to every caller.
            LiquidityScope::PublicOnly => true,
            LiquidityScope::IncludeExclusive => class.exclusive_access == ExclusiveAccess::Granted,
        }
    }
}

/// The worker pools selected to serve one order.
///
/// The router's only source of scope facts once fan-out begins — nothing downstream re-derives
/// them from the full worker pool list, so a worker pool that was not selected cannot reappear in
/// the ranking.
pub(crate) struct Allocation<'a> {
    /// Selected worker pools, in configuration order.
    worker_pools: Vec<&'a SolverPoolHandle>,
    /// Liquidity scope of each selected worker pool, keyed by worker pool name.
    scopes: HashMap<String, LiquidityScope>,
    /// Whether both scopes are selected — the single fact that early-return gating and the
    /// ranking split branch on. See [`Allocation::surplus_routing_active`].
    surplus_routing_active: bool,
}

impl<'a> Allocation<'a> {
    /// Returns the selected worker pools, in configuration order.
    pub(crate) fn worker_pools(&self) -> &[&'a SolverPoolHandle] {
        &self.worker_pools
    }

    /// Returns the liquidity scope of each selected worker pool, keyed by worker pool name.
    pub(crate) fn scopes(&self) -> &HashMap<String, LiquidityScope> {
        &self.scopes
    }

    /// Returns `true` when surplus routing is active for this allocation: it needs both scopes —
    /// a [`LiquidityScope::PublicOnly`] worker pool for the committed reference and a
    /// [`LiquidityScope::IncludeExclusive`] one that may beat it.
    pub(crate) fn surplus_routing_active(&self) -> bool {
        self.surplus_routing_active
    }

    /// Returns whether the named worker pool was selected and routes through exclusive liquidity.
    pub(crate) fn is_exclusive(&self, worker_pool_name: &str) -> bool {
        self.scopes.get(worker_pool_name) == Some(&LiquidityScope::IncludeExclusive)
    }

    /// Returns whether no worker pool serves the request.
    pub(crate) fn is_empty(&self) -> bool {
        self.worker_pools.is_empty()
    }
}

/// Selects the worker pools that serve `class`, preserving configuration order.
pub(crate) fn allocate(worker_pools: &[SolverPoolHandle], class: RequestClass) -> Allocation<'_> {
    let worker_pools: Vec<&SolverPoolHandle> = worker_pools
        .iter()
        .filter(|worker_pool| worker_pool.admission().admits(class))
        .collect();

    let scopes: HashMap<String, LiquidityScope> = worker_pools
        .iter()
        .map(|worker_pool| (worker_pool.name().to_string(), worker_pool.liquidity_scope()))
        .collect();
    let surplus_routing_active = scopes
        .values()
        .any(|scope| *scope == LiquidityScope::IncludeExclusive) &&
        scopes
            .values()
            .any(|scope| *scope == LiquidityScope::PublicOnly);

    Allocation { worker_pools, scopes, surplus_routing_active }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::public_scope_denied(LiquidityScope::PublicOnly, ExclusiveAccess::Denied, true)]
    #[case::public_scope_granted(LiquidityScope::PublicOnly, ExclusiveAccess::Granted, true)]
    #[case::exclusive_scope_denied(
        LiquidityScope::IncludeExclusive,
        ExclusiveAccess::Denied,
        false
    )]
    #[case::exclusive_scope_granted(
        LiquidityScope::IncludeExclusive,
        ExclusiveAccess::Granted,
        true
    )]
    fn test_admits(
        #[case] scope: LiquidityScope,
        #[case] access: ExclusiveAccess,
        #[case] expected: bool,
    ) {
        let admission = WorkerPoolAdmission::default().with_liquidity_scope(scope);

        assert_eq!(admission.admits(RequestClass::new(access)), expected);
    }
}
