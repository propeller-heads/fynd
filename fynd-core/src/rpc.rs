//! `eth_call` plumbing shared by the background tasks that read contract state.
//!
//! `encoding::fee_fetcher` reads the router's `FeeCalculator` rates as view functions over HTTP
//! and decodes the result in three steps. The helpers here return the failure as a message so
//! each caller can attach the method name it was calling and keep its own error type.

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind},
    providers::{Provider, RootProvider},
    rpc::types::TransactionRequest,
    sol_types::SolCall,
};
use tycho_simulation::tycho_common::Bytes;

/// Converts a Tycho address into an alloy [`Address`].
///
/// # Errors
///
/// Returns a message naming `what` when `raw` is not 20 bytes.
pub(crate) fn to_address(raw: &Bytes, what: &str) -> Result<Address, String> {
    if raw.len() != 20 {
        return Err(format!("{what} {raw:?} is not 20 bytes"));
    }
    Ok(Address::from_slice(raw.as_ref()))
}

/// Performs an `eth_call` of `calldata` against `contract` and decodes the return value.
///
/// # Errors
///
/// Returns the transport failure, or the ABI decoding failure when the node answers with data the
/// call's return type does not fit.
pub(crate) async fn eth_call<C: SolCall>(
    provider: &RootProvider<Ethereum>,
    contract: Address,
    calldata: Vec<u8>,
) -> Result<C::Return, String> {
    let response = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(contract)),
            input: AlloyBytes::from(calldata).into(),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
    C::abi_decode_returns(&response).map_err(|e| format!("failed to decode response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_address() {
        let raw = Bytes::from(vec![0x11; 20]);
        assert_eq!(to_address(&raw, "router address"), Ok(Address::repeat_byte(0x11)));

        let short = to_address(&Bytes::from(vec![0x11; 19]), "router address")
            .expect_err("19 bytes is not an address");
        assert!(short.contains("router address"), "{short}");
    }
}
