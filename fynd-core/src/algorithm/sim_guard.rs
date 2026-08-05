//! Panic containment for component simulation calls.
//!
//! `ProtocolSim` implementations run third-party component math that can panic on degenerate
//! inputs (e.g. a U256 division by zero when quoting an absurdly large amount). Left
//! uncaught, such a panic unwinds through the solver worker thread and permanently kills
//! it. Simulation calls made while solving therefore go through this guard, which turns
//! a panic into a `SimulationError` so the algorithm skips the component and keeps solving.

use std::panic::{catch_unwind, AssertUnwindSafe};

use num_bigint::BigUint;
use tracing::warn;
use tycho_simulation::tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

/// Extension trait adding panic-guarded simulation calls to every [`ProtocolSim`].
///
/// Crate-private: solver code calls the guarded variants; the raw `ProtocolSim` methods
/// remain untouched for callers that manage panics themselves.
pub(crate) trait GuardedProtocolSim {
    /// Calls `get_amount_out`, converting a panic into a `SimulationError::FatalError`.
    ///
    /// On a contained panic, logs the input that triggered it (amount and token pair) so
    /// the offending component/quote can be tracked down from the logs.
    fn get_amount_out_guarded(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError>;

    /// Calls `get_limits`, converting a panic into a `SimulationError::FatalError`.
    ///
    /// On a contained panic, logs the token pair so the offending component/quote can be
    /// tracked down from the logs.
    fn get_limits_guarded(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError>;
}

/// Best-effort extraction of a human-readable message from a contained panic payload.
fn panic_message(panic_payload: &(dyn std::any::Any + Send)) -> &str {
    panic_payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| {
            panic_payload
                .downcast_ref::<String>()
                .map(String::as_str)
        })
        .unwrap_or("<non-string panic payload>")
}

impl<T: ProtocolSim + ?Sized> GuardedProtocolSim for T {
    fn get_amount_out_guarded(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // `amount_in` is cloned into the call so the original stays available for the panic log.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            self.get_amount_out(amount_in.clone(), token_in, token_out)
        }));

        outcome.unwrap_or_else(|panic_payload| {
            let message = panic_message(panic_payload.as_ref());
            warn!(
                %amount_in,
                token_in = %token_in.address,
                token_out = %token_out.address,
                token_in_symbol = %token_in.symbol,
                token_out_symbol = %token_out.symbol,
                panic = message,
                "component simulation panicked; skipping component"
            );
            Err(SimulationError::FatalError(format!("get_amount_out panicked: {message}")))
        })
    }

    fn get_limits_guarded(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        // Tokens are cloned into the call so the originals stay available for the panic log.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            self.get_limits(sell_token.clone(), buy_token.clone())
        }));

        outcome.unwrap_or_else(|panic_payload| {
            let message = panic_message(panic_payload.as_ref());
            warn!(
                %sell_token,
                %buy_token,
                panic = message,
                "component get_limits panicked; skipping component"
            );
            Err(SimulationError::FatalError(format!("get_limits panicked: {message}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::{token, DivByZeroSim, MockProtocolSim};

    #[test]
    fn test_get_amount_out_guarded_converts_panic_to_error() {
        let sim = DivByZeroSim::default();
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let result = sim.get_amount_out_guarded(BigUint::from(1000u64), &token_a, &token_b);

        match result {
            Err(SimulationError::FatalError(message)) => {
                assert!(
                    message.contains("panicked") && message.contains("divide by zero"),
                    "unexpected message: {message}"
                );
            }
            Err(other) => panic!("expected FatalError, got {other:?}"),
            Ok(_) => panic!("expected FatalError, got Ok"),
        }
    }

    #[test]
    fn test_get_amount_out_guarded_passes_through_success() {
        let sim = MockProtocolSim::new(2.0);
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let result = sim
            .get_amount_out_guarded(BigUint::from(1000u64), &token_a, &token_b)
            .expect("mock simulation should succeed");
        assert_eq!(result.amount, BigUint::from(2000u64));
    }
}
