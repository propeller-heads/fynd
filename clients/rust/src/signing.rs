use std::{future::Future, pin::Pin};

use alloy::consensus::TypedTransaction;
use alloy::dyn_abi::TypedData;
use alloy::primitives::Signature;
use alloy::primitives::{Address, B256};
use num_bigint::BigUint;

use crate::error::FyndError;
use crate::types::OrderSolution;

// ============================================================================
// PAYLOADS
// ============================================================================

pub struct FyndPayload {
    order_solution: OrderSolution,
    tx: TypedTransaction,
    token_out: bytes::Bytes,
    receiver: bytes::Bytes,
}

impl FyndPayload {
    pub(crate) fn new(
        order_solution: OrderSolution,
        tx: TypedTransaction,
        token_out: bytes::Bytes,
        receiver: bytes::Bytes,
    ) -> Self {
        Self { order_solution, tx, token_out, receiver }
    }

    pub fn order_solution(&self) -> &OrderSolution {
        &self.order_solution
    }

    pub fn tx(&self) -> &TypedTransaction {
        &self.tx
    }

    /// Consume the payload and return the inner parts for use in `execute()`.
    pub(crate) fn into_parts(
        self,
    ) -> (OrderSolution, TypedTransaction, bytes::Bytes, bytes::Bytes) {
        (self.order_solution, self.tx, self.token_out, self.receiver)
    }
}

/// Turbine payload stub. Fields are `()` placeholders until the Turbine signing story lands.
pub struct TurbinePayload {
    #[allow(dead_code)]
    // Placeholder: will be populated in the Turbine signing story
    _order_solution: (),
}

pub enum SignablePayload {
    Fynd(Box<FyndPayload>),
    Turbine(TurbinePayload),
}

impl SignablePayload {
    pub fn signing_hash(&self) -> B256 {
        match self {
            Self::Fynd(p) => {
                use alloy::consensus::SignableTransaction;
                p.tx.signature_hash()
            }
            Self::Turbine(_) => unimplemented!("Turbine signing not yet implemented"),
        }
    }

    pub fn typed_data(&self) -> Option<&TypedData> {
        // EIP-1559 transactions use a signing hash, not EIP-712 typed data.
        match self {
            Self::Fynd(_) | Self::Turbine(_) => None,
        }
    }

    pub fn order_solution(&self) -> &OrderSolution {
        match self {
            Self::Fynd(p) => &p.order_solution,
            Self::Turbine(_) => unimplemented!("Turbine signing not yet implemented"),
        }
    }

    /// Consume the payload and return its inner parts for use in `execute()`.
    pub(crate) fn into_fynd_parts(
        self,
    ) -> Result<
        (OrderSolution, TypedTransaction, bytes::Bytes, bytes::Bytes),
        crate::error::FyndError,
    > {
        match self {
            Self::Fynd(p) => Ok(p.into_parts()),
            Self::Turbine(_) => Err(crate::error::FyndError::Protocol(
                "Turbine execution not yet implemented".into(),
            )),
        }
    }
}

// ============================================================================
// SIGNED ORDER
// ============================================================================

pub struct SignedOrder {
    payload: SignablePayload,
    signature: Signature,
}

impl SignedOrder {
    pub fn assemble(payload: SignablePayload, signature: Signature) -> Self {
        Self { payload, signature }
    }

    pub fn payload(&self) -> &SignablePayload {
        &self.payload
    }

    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    pub(crate) fn into_parts(self) -> (SignablePayload, Signature) {
        (self.payload, self.signature)
    }
}

// ============================================================================
// SETTLED ORDER
// ============================================================================

pub struct SettledOrder {
    tx_receipt: alloy::rpc::types::TransactionReceipt,
    settled_amount: Option<BigUint>,
    gas_cost: BigUint,
}

impl SettledOrder {
    pub(crate) fn new(
        tx_receipt: alloy::rpc::types::TransactionReceipt,
        settled_amount: Option<BigUint>,
        gas_cost: BigUint,
    ) -> Self {
        Self { tx_receipt, settled_amount, gas_cost }
    }

    pub fn tx_receipt(&self) -> &alloy::rpc::types::TransactionReceipt {
        &self.tx_receipt
    }

    pub fn settled_amount(&self) -> Option<&BigUint> {
        self.settled_amount.as_ref()
    }

    pub fn gas_cost(&self) -> &BigUint {
        &self.gas_cost
    }
}

// ============================================================================
// EXECUTION RECEIPT
// ============================================================================

pub enum ExecutionReceipt {
    Transaction(Pin<Box<dyn Future<Output = Result<SettledOrder, FyndError>> + Send + 'static>>),
}

impl Future for ExecutionReceipt {
    type Output = Result<SettledOrder, FyndError>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.get_mut() {
            Self::Transaction(fut) => fut.as_mut().poll(cx),
        }
    }
}

// ============================================================================
// TRANSFER LOG DECODING
// ============================================================================

/// Compute the total amount of `token_out` received by `receiver` from a transaction receipt.
///
/// Scans ERC-20 and ERC-6909 Transfer logs matching the given token address and receiver.
pub(crate) fn compute_settled_amount(
    receipt: &alloy::rpc::types::TransactionReceipt,
    token_out_addr: &Address,
    receiver_addr: &Address,
) -> Option<BigUint> {
    use alloy::primitives::keccak256;

    // ERC-20: Transfer(address indexed from, address indexed to, uint256 value)
    let erc20_topic = keccak256(b"Transfer(address,address,uint256)");
    // ERC-6909: Transfer(address caller, address indexed from, address indexed to,
    //                    uint256 indexed id, uint256 amount)
    let erc6909_topic = keccak256(b"Transfer(address,address,address,uint256,uint256)");

    let mut total = BigUint::ZERO;
    let mut found = false;

    for log in receipt.logs() {
        if log.address() != *token_out_addr {
            continue;
        }
        let topics = log.topics();
        if topics.is_empty() {
            continue;
        }

        if topics[0] == erc20_topic && topics.len() >= 3 {
            // topics[2] is the `to` address (padded to 32 bytes); address is in last 20 bytes.
            let to = Address::from_slice(&topics[2].as_slice()[12..]);
            if to == *receiver_addr {
                let data = &log.data().data;
                if data.len() >= 32 {
                    let amount = BigUint::from_bytes_be(&data[0..32]);
                    total += amount;
                    found = true;
                }
            }
        } else if topics[0] == erc6909_topic && topics.len() >= 3 {
            // topics[2] is the `to` address.
            let to = Address::from_slice(&topics[2].as_slice()[12..]);
            if to == *receiver_addr {
                // data encodes (address caller, uint256 amount) = 64 bytes; amount at bytes 32..64.
                let data = &log.data().data;
                if data.len() >= 64 {
                    let amount = BigUint::from_bytes_be(&data[32..64]);
                    total += amount;
                    found = true;
                }
            }
        }
    }

    if found {
        Some(total)
    } else {
        None
    }
}
