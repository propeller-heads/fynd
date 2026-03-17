//! Permit2 helpers: on-chain nonce reading.

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind},
    providers::{Provider, RootProvider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};

sol! {
    interface IPermit2 {
        /// Returns the current AllowanceTransfer state for a `(owner, token, spender)` triple.
        function allowance(address owner, address token, address spender)
            external view returns (uint160 amount, uint48 expiration, uint48 nonce);
    }
}

/// Read the current nonce for a `(owner, token, spender)` triple from the Permit2 contract.
///
/// For a freshly generated address (e.g. dry-run ephemeral key) the nonce is always
/// 0 without a chain call, but this confirms it is safe to use.
pub async fn read_nonce(
    provider: &RootProvider<Ethereum>,
    permit2: Address,
    owner: Address,
    token: Address,
    spender: Address,
) -> anyhow::Result<u64> {
    let calldata = IPermit2::allowanceCall { owner, token, spender }.abi_encode();
    let result = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(permit2)),
            input: AlloyBytes::from(calldata).into(),
            ..Default::default()
        })
        .await?;
    let decoded = IPermit2::allowanceCall::abi_decode_returns(&result)?;
    Ok(decoded.nonce.to::<u64>())
}
