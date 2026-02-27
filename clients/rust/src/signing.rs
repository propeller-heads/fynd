use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::primitives::Signature;

use crate::error::FyndClientError;

/// The payload the caller must sign to execute a trade.
///
/// Call `signing_hash()` to get the hash to sign, then pass the signature
/// to `assemble_signed_order()`.
#[derive(Debug, Clone)]
pub enum SignablePayload {
    /// Direct Fynd execution path.
    Fynd(FyndPayload),
    /// Turbine intent path (not yet implemented — stub for future use).
    Turbine(TurbinePayload),
}

#[derive(Debug, Clone)]
pub struct FyndPayload {
    /// The EIP-1559 transaction to sign, minus the signature.
    pub(crate) tx: TxEip1559,
}

/// Placeholder for the Turbine signing path.
#[derive(Debug, Clone)]
pub struct TurbinePayload {
    _private: (),
}

impl SignablePayload {
    /// Returns the hash the caller should sign.
    pub fn signing_hash(&self) -> alloy::primitives::B256 {
        match self {
            SignablePayload::Fynd(p) => p.tx.signature_hash(),
            SignablePayload::Turbine(_) => {
                unimplemented!("Turbine signing not yet implemented")
            }
        }
    }
}

/// A signed, ready-to-broadcast order.
#[derive(Debug, Clone)]
pub enum SignedOrder {
    Fynd { envelope: Box<TxEnvelope> },
    Turbine { _private: () },
}

/// Combines the signable payload with a signature to produce a signed order.
pub fn assemble_signed_order(
    payload: SignablePayload,
    signature: Signature,
) -> Result<SignedOrder, FyndClientError> {
    match payload {
        SignablePayload::Fynd(p) => {
            let signed_tx = p.tx.into_signed(signature);
            Ok(SignedOrder::Fynd { envelope: Box::new(TxEnvelope::Eip1559(signed_tx)) })
        }
        SignablePayload::Turbine(_) => {
            Err(FyndClientError::UnexpectedResponse("Turbine not yet implemented".to_string()))
        }
    }
}
