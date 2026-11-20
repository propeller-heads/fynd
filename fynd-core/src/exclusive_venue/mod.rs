//! The protocol specifics of exclusive pricing.
//!
//! A venue is a protocol whose pools price a swap that Fynd signs below their public price. Each
//! venue builds the exclusive view of a pool's state, tells whether Fynd can sign for the pool, and
//! signs the swap. The rest of Fynd calls the venues through [`ExclusiveVenue`] and does not know
//! which protocol it routes through.

mod curve;

use std::sync::Arc;

use tycho_simulation::tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes};

#[cfg(test)]
pub(crate) use self::curve::test_curve;
use crate::{encoding::exclusive_swap::ExclusiveSwapSigner, SolveError, Swap};

/// The key that signs Fynd's exclusive swaps and the router that submits them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grantor {
    controller: Bytes,
    router: Bytes,
}

impl Grantor {
    pub(crate) fn new(controller: Bytes, router: Bytes) -> Self {
        Self { controller, router }
    }

    pub(crate) fn controller(&self) -> &Bytes {
        &self.controller
    }

    pub(crate) fn router(&self) -> &Bytes {
        &self.router
    }
}

/// A protocol whose pools price a swap that Fynd signs below their public price.
pub(crate) trait ExclusiveVenue: Sync {
    /// Builds the exclusive view of `state`. `None` when `state` is not of this venue, or when the
    /// venue cannot price its pool with a grant.
    fn build_exclusive_view(&self, state: &dyn ProtocolSim) -> Option<Arc<dyn ProtocolSim>>;

    /// Whether `state` is an exclusive view of this venue.
    fn is_exclusive_view(&self, state: &dyn ProtocolSim) -> bool;

    /// Whether `grantor` can sign a swap through the pool of `state`.
    fn can_grant(&self, state: &dyn ProtocolSim, grantor: &Grantor) -> bool;

    /// Signs `swap`, priced by an exclusive view of this venue, for `taker`. Returns the swap's
    /// `user_data`.
    fn sign(
        &self,
        signer: &ExclusiveSwapSigner,
        swap: &Swap,
        taker: &Bytes,
        slippage: f64,
    ) -> Result<Bytes, SolveError>;
}

/// Every venue Fynd prices with a grant.
const VENUES: &[&dyn ExclusiveVenue] = &[&curve::CurveVenue];

/// Builds the exclusive view of `state`, from the first venue that has one.
pub(crate) fn build_exclusive_view(state: &dyn ProtocolSim) -> Option<Arc<dyn ProtocolSim>> {
    for venue in VENUES {
        if let Some(view) = venue.build_exclusive_view(state) {
            return Some(view);
        }
    }
    None
}

/// The venue of the exclusive view `state`. `None` when `state` is not an exclusive view.
pub(crate) fn find_venue(state: &dyn ProtocolSim) -> Option<&'static dyn ExclusiveVenue> {
    for venue in VENUES {
        if venue.is_exclusive_view(state) {
            return Some(*venue);
        }
    }
    None
}

/// Whether `grantor` can sign a swap through the exclusive view `view`.
pub(crate) fn can_grant(view: &dyn ProtocolSim, grantor: &Grantor) -> bool {
    find_venue(view).is_some_and(|venue| venue.can_grant(view, grantor))
}
