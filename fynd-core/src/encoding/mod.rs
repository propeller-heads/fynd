use std::time::{SystemTime, UNIX_EPOCH};

use crate::SolveError;

/// Signer for disable-slippage-taking encoding: server-signed zero-fee `ClientFee` params.
pub mod disable_slippage_taking;
/// Route encoder: converts solver output into ABI-encoded on-chain calldata.
///
/// Wraps [tycho-execution](https://docs.propellerheads.xyz/tycho/for-solvers/execution) to
/// produce ABI-encoded calldata for single, sequential, and split swaps, with and without
/// Permit2. See
/// the [Fynd encoding guide](https://docs.fynd.xyz/guides/encoding-options) for supported
/// encoding options and how to configure them.
pub mod encoder;
pub mod exclusive_swap;
pub mod fee_fetcher;
pub mod router_fees;

/// Current Unix time in seconds, for the deadlines the signing modules stamp into their payloads.
///
/// # Errors
/// Errors when the system clock reads before the Unix epoch. A fallback timestamp would be signed
/// into a payload the router rejects on every submission, so the encode fails instead.
fn now_unix_secs() -> Result<u64, SolveError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| {
            SolveError::FailedEncoding(
                "system clock is before the Unix epoch, cannot compute a signing deadline"
                    .to_string(),
            )
        })
}
