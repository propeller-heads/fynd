//! The protocols Fynd signs exclusive swaps for.
//!
//! An [`ExclusiveProtocol`] recognises its own exclusive legs and signs them. Ekubo's exclusive
//! pools are exclusive only: Tycho flags their components, and public workers never route through
//! them. A [`DualPool`] protocol's pools are public too: public workers route through a dual pool
//! at its own price, and exclusive workers through its exclusive view, which prices the pool with
//! a grant Fynd signs. Curve is both.

mod curve;
mod ekubo;

use std::sync::Arc;

use tycho_simulation::tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes};

#[cfg(test)]
pub(crate) use self::curve::test_curve;
use crate::{encoding::exclusive_swap::ExclusiveSwapSigner, SolveError, Swap};

/// Fynd's controller address, which signs exclusive swaps, and the router that submits them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grantor {
    pub(crate) controller: Bytes,
    pub(crate) router: Bytes,
}

/// A protocol whose exclusive legs Fynd signs.
pub(crate) trait ExclusiveProtocol: Sync {
    /// Whether `swap` is an exclusive leg of this protocol.
    fn owns(&self, swap: &Swap) -> bool;

    /// Signs `swap`, an exclusive leg of this protocol, for `taker`. Returns the swap's
    /// `user_data`.
    fn sign(
        &self,
        signer: &ExclusiveSwapSigner,
        swap: &Swap,
        taker: &Bytes,
        slippage: f64,
    ) -> Result<Bytes, SolveError>;
}

/// A protocol whose public pools Fynd can also price with a grant.
pub(crate) trait DualPool: Sync {
    /// Builds the exclusive view of `state`: the pool priced with a grant. `None` when `state` is
    /// not of this protocol, or when the protocol cannot price the pool with a grant.
    fn build_exclusive_view(&self, state: &dyn ProtocolSim) -> Option<Arc<dyn ProtocolSim>>;

    /// Whether `grantor` can sign a swap through the pool of `state`.
    fn can_grant(&self, state: &dyn ProtocolSim, grantor: &Grantor) -> bool;
}

/// Every protocol whose exclusive legs Fynd signs.
const EXCLUSIVE_PROTOCOLS: &[&dyn ExclusiveProtocol] = &[&ekubo::Ekubo, &curve::Curve];

/// Every protocol whose public pools Fynd can price with a grant.
const DUAL_POOLS: &[&dyn DualPool] = &[&curve::Curve];

/// The protocol `swap` is an exclusive leg of. `None` when `swap` is not an exclusive leg.
pub(crate) fn protocol_of(swap: &Swap) -> Option<&'static dyn ExclusiveProtocol> {
    EXCLUSIVE_PROTOCOLS
        .iter()
        .copied()
        .find(|protocol| protocol.owns(swap))
}

/// Builds the exclusive view of `state` if `grantor` can sign for its pool. `None` for any other
/// state.
pub(crate) fn build_exclusive_view(
    state: &dyn ProtocolSim,
    grantor: &Grantor,
) -> Option<Arc<dyn ProtocolSim>> {
    DUAL_POOLS
        .iter()
        .filter(|protocol| protocol.can_grant(state, grantor))
        .find_map(|protocol| protocol.build_exclusive_view(state))
}
