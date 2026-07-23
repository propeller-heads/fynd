//! Panic containment for pool simulation calls.
//!
//! `ProtocolSim` implementations run third-party pool math that can panic on degenerate
//! inputs (e.g. a U256 division by zero when quoting an absurdly large amount). Left
//! uncaught, such a panic unwinds through the solver worker thread and permanently kills
//! it. Simulation calls made while solving therefore go through this guard, which turns
//! a panic into a `SimulationError` so the algorithm skips the pool and keeps solving.

use std::panic::{catch_unwind, AssertUnwindSafe};

use num_bigint::BigUint;
use tycho_simulation::tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
};

/// Calls `sim.get_amount_out`, converting a panic into a `SimulationError::FatalError`.
pub(crate) fn get_amount_out_guarded(
    sim: &dyn ProtocolSim,
    amount_in: BigUint,
    token_in: &Token,
    token_out: &Token,
) -> Result<GetAmountOutResult, SimulationError> {
    catch_unwind(AssertUnwindSafe(|| sim.get_amount_out(amount_in, token_in, token_out)))
        .unwrap_or_else(|panic_payload| {
            let message = panic_payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| {
                    panic_payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                })
                .unwrap_or("<non-string panic payload>");
            Err(SimulationError::FatalError(format!("get_amount_out panicked: {message}")))
        })
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

        let result = get_amount_out_guarded(&sim, BigUint::from(1000u64), &token_a, &token_b);

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

        let result = get_amount_out_guarded(&sim, BigUint::from(1000u64), &token_a, &token_b)
            .expect("mock simulation should succeed");
        assert_eq!(result.amount, BigUint::from(2000u64));
    }
}
