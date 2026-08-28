//! Decides which worker pools serve an order, before any work is dispatched.
//!
//! Each order is classified into an [`OrderClass`] — what it is and what it may reach. Each
//! worker pool decides from its own configuration whether it serves a class
//! ([`SolverPoolHandle::serves`]). [`allocate`] intersects the two into an [`Allocation`], and
//! the router fans out to those worker pools only.
//!
//! Deciding before fan-out rather than filtering candidates afterwards means a worker pool that
//! does not serve a request costs it no CPU and no latency. It also leaves the router with a
//! single source of truth: early-return gating and the ranking split both read the allocation, so
//! a request without exclusive access cannot leak exclusive liquidity through a branch that was
//! missed.
//!
//! Both sides grow one field per dimension. Trade size derived from `amount_in` — routing small
//! orders to fast algorithms and large ones to algorithms that handle them better — is the next
//! one: a field on [`OrderClass`], a matching condition in [`SolverPoolHandle::serves`].

use rustc_hash::FxHashMap;
use tracing::warn;

use super::{LiquidityScope, SolverPoolHandle};
use crate::SolveError;

/// Whether a single request may route through exclusive liquidity.
///
/// Exclusive liquidity is reserved for selected clients, so access is decided at the request
/// boundary by the operator — the RPC layer reads it from a header set by the authenticating
/// proxy — and never from anything a caller can put in a `QuoteRequest`.
///
/// `Denied` is not an error: the request is quoted from public liquidity alone, which is the same
/// answer a deployment without exclusive worker pools would give. Missing access costs price
/// rather than breaking the request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExclusiveAccess {
    /// Route through public liquidity only. Default: access must be granted explicitly.
    #[default]
    Denied,
    /// Route through exclusive components too, capturing surplus above the public reference.
    Granted,
}

/// What an order is, and what it is allowed to reach.
///
/// Built once per order from the trust-boundary access decision plus facts about the order
/// itself. Each worker pool checks it in [`SolverPoolHandle::serves`] to decide the fan-out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OrderClass {
    /// Whether this order may be served by exclusive-access worker pools.
    exclusive_access: ExclusiveAccess,
}

impl OrderClass {
    /// Classifies an order whose only distinguishing property is the caller's access.
    pub(crate) fn new(exclusive_access: ExclusiveAccess) -> Self {
        Self { exclusive_access }
    }
}

impl SolverPoolHandle {
    /// Returns whether this worker pool serves orders of `class`.
    ///
    /// Every configured condition must hold; today the only one is the liquidity scope.
    pub(crate) fn serves(&self, class: OrderClass) -> bool {
        match self.liquidity_scope() {
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
    scopes: FxHashMap<String, LiquidityScope>,
    /// Whether an exclusive-scope worker pool is selected — the single fact that early-return
    /// gating and the ranking split branch on. See [`Allocation::exclusive_routing_active`].
    exclusive_routing_active: bool,
}

impl<'a> Allocation<'a> {
    /// Returns the selected worker pools, in configuration order.
    pub(crate) fn worker_pools(&self) -> &[&'a SolverPoolHandle] {
        &self.worker_pools
    }

    /// Returns the liquidity scope of each selected worker pool, keyed by worker pool name.
    pub(crate) fn scopes(&self) -> &FxHashMap<String, LiquidityScope> {
        &self.scopes
    }

    /// Returns `true` when a selected worker pool routes through exclusive liquidity
    /// ([`LiquidityScope::IncludeExclusive`]). A [`LiquidityScope::PublicOnly`] worker pool is
    /// then also selected — public worker pools serve every order, and startup rejects a
    /// configuration without one — so the ranking always has a public reference to overlay the
    /// exclusive candidate onto.
    pub(crate) fn exclusive_routing_active(&self) -> bool {
        self.exclusive_routing_active
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
///
/// `pool_allowlist` further restricts the selection to the named worker pools. It is validated
/// against the full configuration, not the class-filtered set, so a typo fails loudly instead of
/// silently routing through nothing. An empty allowlist is rejected rather than silently
/// resolving to no worker pools — omit it to reach every worker pool that serves the request.
pub(crate) fn allocate<'a>(
    worker_pools: &'a [SolverPoolHandle],
    class: OrderClass,
    pool_allowlist: Option<&[String]>,
) -> Result<Allocation<'a>, SolveError> {
    if let Some(allowlist) = pool_allowlist {
        if allowlist.is_empty() {
            return Err(SolveError::Internal(
                "worker pool allowlist is empty; omit it to use every pool that serves the \
                 request"
                    .to_string(),
            ));
        }
        let unknown: Vec<&str> = allowlist
            .iter()
            .map(String::as_str)
            .filter(|name| {
                !worker_pools
                    .iter()
                    .any(|pool| pool.name() == *name)
            })
            .collect();
        if !unknown.is_empty() {
            let configured: Vec<&str> = worker_pools
                .iter()
                .map(SolverPoolHandle::name)
                .collect();
            warn!(?configured, ?unknown, "worker pool allowlist names unknown pool(s)");
            return Err(SolveError::Internal(format!("unknown worker pool(s) {unknown:?}")));
        }
    }

    let worker_pools: Vec<&SolverPoolHandle> = worker_pools
        .iter()
        .filter(|worker_pool| worker_pool.serves(class))
        .filter(|worker_pool| {
            pool_allowlist.is_none_or(|allowlist| {
                allowlist
                    .iter()
                    .any(|n| n == worker_pool.name())
            })
        })
        .collect();

    let scopes: FxHashMap<String, LiquidityScope> = worker_pools
        .iter()
        .map(|worker_pool| (worker_pool.name().to_string(), worker_pool.liquidity_scope()))
        .collect();
    let exclusive_routing_active = scopes
        .values()
        .any(|scope| *scope == LiquidityScope::IncludeExclusive);

    Ok(Allocation { worker_pools, scopes, exclusive_routing_active })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{worker_pool::TaskQueueHandle, SolveError};

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
    fn test_serves(
        #[case] scope: LiquidityScope,
        #[case] access: ExclusiveAccess,
        #[case] expected: bool,
    ) {
        let (tx, _rx) = async_channel::bounded(1);
        let worker_pool = SolverPoolHandle::new("worker_pool", TaskQueueHandle::from_sender(tx))
            .with_liquidity_scope(scope);

        assert_eq!(worker_pool.serves(OrderClass::new(access)), expected);
    }

    fn handle(name: &str) -> SolverPoolHandle {
        let (tx, _rx) = async_channel::bounded(1);
        SolverPoolHandle::new(name, TaskQueueHandle::from_sender(tx))
    }

    fn names<'a>(allocation: &Allocation<'a>) -> Vec<&'a str> {
        allocation
            .worker_pools()
            .iter()
            .map(|pool| pool.name())
            .collect()
    }

    #[test]
    fn test_allocate_without_allowlist_keeps_every_serving_pool() {
        let pools = [handle("a"), handle("b")];
        let allocation =
            allocate(&pools, OrderClass::new(ExclusiveAccess::Denied), None).expect("allocates");
        assert_eq!(names(&allocation), vec!["a", "b"]);
    }

    #[test]
    fn test_allocate_allowlist_selects_subset_in_configuration_order() {
        let pools = [handle("a"), handle("b"), handle("c")];
        let allowlist = ["c".to_string(), "a".to_string()];
        let allocation =
            allocate(&pools, OrderClass::new(ExclusiveAccess::Denied), Some(&allowlist))
                .expect("allocates");
        assert_eq!(names(&allocation), vec!["a", "c"]);
    }

    #[test]
    fn test_allocate_allowlist_unknown_pool_is_an_error() {
        let pools = [handle("a")];
        let allowlist = ["a".to_string(), "nope".to_string()];
        let Err(err) = allocate(&pools, OrderClass::new(ExclusiveAccess::Denied), Some(&allowlist))
        else {
            panic!("expected an error for an unknown pool name");
        };
        let SolveError::Internal(message) = err else { panic!("expected Internal, got {err:?}") };
        assert!(message.contains("nope"), "{message}");
        // The configured pool list is logged, not returned to the caller — see M2.
        assert!(!message.contains("\"a\""), "{message}");
    }

    #[test]
    fn test_allocate_empty_allowlist_is_an_error() {
        let pools = [handle("a")];
        let allowlist: [String; 0] = [];
        let Err(err) = allocate(&pools, OrderClass::new(ExclusiveAccess::Denied), Some(&allowlist))
        else {
            panic!("expected an error for an empty allowlist");
        };
        let SolveError::Internal(message) = err else { panic!("expected Internal, got {err:?}") };
        assert!(message.contains("empty"), "{message}");
    }

    #[test]
    fn test_allocate_allowlist_cannot_reach_exclusive_pool_without_access() {
        let pools = [
            handle("public"),
            handle("exclusive").with_liquidity_scope(LiquidityScope::IncludeExclusive),
        ];
        let allowlist = ["exclusive".to_string()];
        let allocation =
            allocate(&pools, OrderClass::new(ExclusiveAccess::Denied), Some(&allowlist))
                .expect("allocates");
        assert!(allocation.is_empty());
    }
}
