//! Ekubo pools with the `SignedExclusiveSwap` extension.
//!
//! Tycho flags these pools exclusive, so only exclusive workers route through them, and every swap
//! through one needs an authorization Fynd signs.

use tycho_simulation::tycho_common::Bytes;

use crate::{
    encoding::exclusive_swap::ExclusiveSwapSigner, exclusive_protocol::ExclusiveProtocol,
    feed::exclusivity::is_exclusive, SolveError, Swap,
};

/// An Ekubo leg is exclusive when Tycho flags its pool exclusive. Ekubo's `SignedExclusiveSwap`
/// pools are the only components Tycho flags.
pub(crate) struct Ekubo;

impl ExclusiveProtocol for Ekubo {
    fn owns(&self, swap: &Swap) -> bool {
        is_exclusive(swap.protocol_component())
    }

    /// The extension authorizes the router, not the taker, so `taker` is unused.
    fn sign(
        &self,
        signer: &ExclusiveSwapSigner,
        swap: &Swap,
        _taker: &Bytes,
        slippage: f64,
    ) -> Result<Bytes, SolveError> {
        signer.build_user_data(swap, slippage)
    }
}
