/// Route encoder: converts solver output into ABI-encoded on-chain calldata.
///
/// Wraps [tycho-execution](https://docs.propellerheads.xyz/tycho/for-solvers/execution) to
/// produce ABI-encoded calldata for single and sequential swaps, with and without Permit2. See
/// the [Fynd encoding guide](https://docs.fynd.xyz/guides/encoding-options) for supported
/// encoding options and how to configure them.
pub mod encoder;
