use std::{future::Future, pin::Pin};

use alloy::{
    consensus::{TxEip1559, TypedTransaction},
    dyn_abi::TypedData,
    primitives::{Address, Signature, B256},
};
use num_bigint::BigUint;

use crate::{error::FyndError, Quote};

// ============================================================================
// PAYLOADS
// ============================================================================

/// A ready-to-sign EIP-1559 transaction produced by the Fynd execution path.
///
/// Obtain one via [`FyndClient::swap_payload`](crate::FyndClient::swap_payload) when
/// the quote's backend is [`BackendKind::Fynd`](crate::BackendKind::Fynd).
#[derive(Debug)]
pub struct FyndPayload {
    quote: Quote,
    tx: TypedTransaction,
}

impl FyndPayload {
    pub(crate) fn new(quote: Quote, tx: TypedTransaction) -> Self {
        Self { quote, tx }
    }

    /// The order quote this payload was built from.
    pub fn quote(&self) -> &Quote {
        &self.quote
    }

    /// The unsigned EIP-1559 transaction. Sign its
    /// [`signature_hash()`](alloy::consensus::SignableTransaction::signature_hash) and pass the
    /// result to [`SignedSwap::assemble`].
    pub fn tx(&self) -> &TypedTransaction {
        &self.tx
    }

    /// Consume the payload and return the inner parts for use in `execute()`.
    pub(crate) fn into_parts(self) -> (Quote, TypedTransaction) {
        (self.quote, self.tx)
    }
}

/// Turbine payload stub. Fields are `()` placeholders until the Turbine signing story lands.
#[derive(Debug)]
pub struct TurbinePayload {
    #[allow(dead_code)]
    // Placeholder: will be populated in the Turbine signing story
    _order_quote: (),
}

/// A payload that needs to be signed before a swap can be executed.
///
/// Use [`signing_hash`](Self::signing_hash) to obtain the bytes to sign, then pass the resulting
/// [`alloy::primitives::Signature`] to [`SignedSwap::assemble`].
///
/// Only the [`Fynd`](Self::Fynd) variant is currently executable; calling methods on the
/// [`Turbine`](Self::Turbine) variant will panic with `unimplemented!`.
#[derive(Debug)]
pub enum SwapPayload {
    /// Fynd execution path — an EIP-1559 transaction targeting the RouterV3 contract.
    Fynd(Box<FyndPayload>),
    /// Turbine execution path — not yet implemented.
    Turbine(TurbinePayload),
}

impl SwapPayload {
    /// Returns the 32-byte hash that must be signed.
    ///
    /// For the Fynd path this is the EIP-1559 transaction's `signature_hash()`.
    ///
    /// # Panics
    ///
    /// Panics if called on the `Turbine` variant.
    pub fn signing_hash(&self) -> B256 {
        match self {
            Self::Fynd(p) => {
                use alloy::consensus::SignableTransaction;
                p.tx.signature_hash()
            }
            Self::Turbine(_) => unimplemented!("Turbine signing not yet implemented"),
        }
    }

    /// Returns EIP-712 typed data for wallets that support `eth_signTypedData_v4`.
    ///
    /// Always returns `None` for EIP-1559 transactions (Fynd path); those use
    /// [`signing_hash`](Self::signing_hash) instead.
    pub fn typed_data(&self) -> Option<&TypedData> {
        // EIP-1559 transactions use a signing hash, not EIP-712 typed data.
        match self {
            Self::Fynd(_) | Self::Turbine(_) => None,
        }
    }

    /// The order quote embedded in this payload.
    ///
    /// # Panics
    ///
    /// Panics if called on the `Turbine` variant.
    pub fn quote(&self) -> &Quote {
        match self {
            Self::Fynd(p) => &p.quote,
            Self::Turbine(_) => unimplemented!("Turbine signing not yet implemented"),
        }
    }

    /// Consume the payload and return its inner parts for use in `execute_swap()`.
    pub(crate) fn into_fynd_parts(
        self,
    ) -> Result<(Quote, TypedTransaction), crate::error::FyndError> {
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

/// A [`SwapPayload`] paired with its cryptographic signature.
///
/// Construct via [`SignedSwap::assemble`] after signing the
/// [`signing_hash`](SwapPayload::signing_hash). Pass to
/// [`FyndClient::execute_swap`](crate::FyndClient::execute_swap) to broadcast and settle.
pub struct SignedSwap {
    payload: SwapPayload,
    signature: Signature,
}

impl SignedSwap {
    /// Pair a payload with the signature produced by signing its
    /// [`signing_hash`](SwapPayload::signing_hash).
    pub fn assemble(payload: SwapPayload, signature: Signature) -> Self {
        Self { payload, signature }
    }

    /// The underlying swap payload.
    pub fn payload(&self) -> &SwapPayload {
        &self.payload
    }

    /// The signature over the payload's signing hash.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    pub(crate) fn into_parts(self) -> (SwapPayload, Signature) {
        (self.payload, self.signature)
    }
}

// ============================================================================
// SETTLED ORDER
// ============================================================================

/// The result of a successfully mined or simulated swap transaction.
///
/// Returned by awaiting an [`ExecutionReceipt`]. For dry-run executions
/// ([`ExecutionOptions::dry_run`](crate::ExecutionOptions)), `tx_hash` and `tx_receipt` are `None`.
#[derive(Debug, Clone)]
pub struct SettledOrder {
    tx_hash: Option<B256>,
    settled_amount: Option<BigUint>,
    gas_cost: BigUint,
}

impl SettledOrder {
    pub(crate) fn new(
        tx_hash: Option<B256>,
        settled_amount: Option<BigUint>,
        gas_cost: BigUint,
    ) -> Self {
        Self { tx_hash, settled_amount, gas_cost }
    }

    /// The transaction hash of the mined swap. `None` for dry-run simulations.
    pub fn tx_hash(&self) -> Option<B256> {
        self.tx_hash
    }

    /// The total amount of `token_out` actually received by the receiver, summed across all
    /// matching ERC-20 and ERC-6909 Transfer logs. Returns `None` when no matching logs are found
    /// (e.g. the swap reverted or used an unsupported token standard).
    pub fn settled_amount(&self) -> Option<&BigUint> {
        self.settled_amount.as_ref()
    }

    /// The actual gas cost of the transaction in wei (`gas_used * effective_gas_price`).
    pub fn gas_cost(&self) -> &BigUint {
        &self.gas_cost
    }
}

// ============================================================================
// EXECUTION RECEIPT
// ============================================================================

/// A future that resolves once the swap transaction is mined and settled.
///
/// Returned by [`FyndClient::execute_swap`](crate::FyndClient::execute_swap). The inner future
/// polls the RPC node every 2 seconds and **has no built-in timeout** — it will poll indefinitely
/// if the transaction is never mined. Callers should wrap it with [`tokio::time::timeout`]:
///
/// ```rust,no_run
/// # use fynd_client::ExecutionReceipt;
/// # use std::time::Duration;
/// # async fn example(receipt: ExecutionReceipt) {
/// let result = tokio::time::timeout(Duration::from_secs(120), receipt).await;
/// # }
/// ```
pub enum ExecutionReceipt {
    /// A pending on-chain transaction.
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
// APPROVAL PAYLOAD
// ============================================================================

/// An unsigned EIP-1559 `approve(spender, amount)` transaction.
///
/// Obtain via [`FyndClient::approval`](crate::FyndClient::approval). Sign its
/// [`signing_hash`](Self::signing_hash) and pass the result to [`SignedApproval::assemble`].
pub struct ApprovalPayload {
    pub(crate) tx: TxEip1559,
    /// ERC-20 token contract address (20 raw bytes).
    pub(crate) token: bytes::Bytes,
    /// Spender address being approved (20 raw bytes).
    pub(crate) spender: bytes::Bytes,
    /// Amount being approved (token units).
    pub(crate) amount: BigUint,
}

impl ApprovalPayload {
    /// The 32-byte hash to sign.
    pub fn signing_hash(&self) -> B256 {
        use alloy::consensus::SignableTransaction;
        self.tx.signature_hash()
    }

    /// The unsigned EIP-1559 transaction.
    pub fn tx(&self) -> &TxEip1559 {
        &self.tx
    }

    /// ERC-20 token address (20 raw bytes).
    pub fn token(&self) -> &bytes::Bytes {
        &self.token
    }

    /// Spender address (20 raw bytes).
    pub fn spender(&self) -> &bytes::Bytes {
        &self.spender
    }

    /// Amount to approve (token units).
    pub fn amount(&self) -> &BigUint {
        &self.amount
    }
}

/// An [`ApprovalPayload`] paired with its cryptographic signature.
///
/// Construct via [`SignedApproval::assemble`] after signing the
/// [`signing_hash`](ApprovalPayload::signing_hash). Pass to
/// [`FyndClient::execute_approval`](crate::FyndClient::execute_approval).
pub struct SignedApproval {
    payload: ApprovalPayload,
    signature: Signature,
}

impl SignedApproval {
    /// Pair a payload with the signature produced by signing its
    /// [`signing_hash`](ApprovalPayload::signing_hash).
    pub fn assemble(payload: ApprovalPayload, signature: Signature) -> Self {
        Self { payload, signature }
    }

    /// The underlying approval payload.
    pub fn payload(&self) -> &ApprovalPayload {
        &self.payload
    }

    /// The signature over the payload's signing hash.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    pub(crate) fn into_parts(self) -> (ApprovalPayload, Signature) {
        (self.payload, self.signature)
    }
}

// ============================================================================
// MINED TX / TX RECEIPT
// ============================================================================

/// The result of a successfully mined transaction (non-swap).
#[derive(Debug, Clone)]
pub struct MinedTx {
    tx_hash: B256,
    gas_cost: BigUint,
}

impl MinedTx {
    pub(crate) fn new(tx_hash: B256, gas_cost: BigUint) -> Self {
        Self { tx_hash, gas_cost }
    }

    /// Transaction hash.
    pub fn tx_hash(&self) -> B256 {
        self.tx_hash
    }

    /// Actual gas cost in wei (`gas_used * effective_gas_price`).
    pub fn gas_cost(&self) -> &BigUint {
        &self.gas_cost
    }
}

/// A future that resolves once a submitted transaction is mined.
///
/// Returned by [`FyndClient::execute_approval`](crate::FyndClient::execute_approval). Polls the RPC
/// node every 2 seconds with no built-in timeout — wrap with [`tokio::time::timeout`] as needed.
pub enum TxReceipt {
    /// A pending on-chain transaction.
    Pending(Pin<Box<dyn Future<Output = Result<MinedTx, FyndError>> + Send + 'static>>),
}

impl Future for TxReceipt {
    type Output = Result<MinedTx, FyndError>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.get_mut() {
            Self::Pending(fut) => fut.as_mut().poll(cx),
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

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{keccak256, Address, Bytes as AlloyBytes, LogData, B256},
        rpc::types::{Log, TransactionReceipt},
    };

    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_receipt(logs: Vec<Log>) -> TransactionReceipt {
        use alloy::{
            consensus::{Receipt, ReceiptEnvelope, ReceiptWithBloom},
            primitives::{Bloom, TxHash},
        };

        TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(ReceiptWithBloom {
                receipt: Receipt {
                    status: alloy::consensus::Eip658Value::Eip658(true),
                    cumulative_gas_used: 21_000,
                    logs,
                },
                logs_bloom: Bloom::default(),
            }),
            transaction_hash: TxHash::default(),
            transaction_index: None,
            block_hash: None,
            block_number: None,
            gas_used: 21_000,
            effective_gas_price: 1,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        }
    }

    fn erc20_topic() -> B256 {
        keccak256(b"Transfer(address,address,uint256)")
    }

    fn erc6909_topic() -> B256 {
        keccak256(b"Transfer(address,address,address,uint256,uint256)")
    }

    /// Pad an address into a 32-byte B256 topic (right-align in 32 bytes).
    fn addr_topic(addr: Address) -> B256 {
        let mut topic = [0u8; 32];
        topic[12..].copy_from_slice(addr.as_slice());
        B256::from(topic)
    }

    /// Encode a u64 amount as 32 big-endian bytes.
    fn encode_u256(amount: u64) -> Vec<u8> {
        let mut buf = [0u8; 32];
        let bytes = amount.to_be_bytes();
        buf[24..].copy_from_slice(&bytes);
        buf.to_vec()
    }

    fn make_log(address: Address, topics: Vec<B256>, data: Vec<u8>) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address,
                data: LogData::new_unchecked(topics, AlloyBytes::from(data)),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    // -----------------------------------------------------------------------
    // ERC-20 tests
    // -----------------------------------------------------------------------

    #[test]
    fn erc20_transfer_log_matched() {
        let token = Address::with_last_byte(0x01);
        let from = Address::with_last_byte(0x02);
        let receiver = Address::with_last_byte(0x03);

        let log = make_log(
            token,
            vec![erc20_topic(), addr_topic(from), addr_topic(receiver)],
            encode_u256(500),
        );
        let receipt = make_receipt(vec![log]);

        let result = compute_settled_amount(&receipt, &token, &receiver);
        assert_eq!(result, Some(BigUint::from(500u64)));
    }

    #[test]
    fn erc20_transfer_log_wrong_token() {
        let token = Address::with_last_byte(0x01);
        let other_token = Address::with_last_byte(0x99);
        let receiver = Address::with_last_byte(0x03);

        let log = make_log(
            other_token, // different token address
            vec![erc20_topic(), addr_topic(Address::ZERO), addr_topic(receiver)],
            encode_u256(500),
        );
        let receipt = make_receipt(vec![log]);

        let result = compute_settled_amount(&receipt, &token, &receiver);
        assert!(result.is_none());
    }

    #[test]
    fn erc20_transfer_log_wrong_receiver() {
        let token = Address::with_last_byte(0x01);
        let receiver = Address::with_last_byte(0x03);
        let other_receiver = Address::with_last_byte(0x04);

        let log = make_log(
            token,
            vec![erc20_topic(), addr_topic(Address::ZERO), addr_topic(other_receiver)],
            encode_u256(500),
        );
        let receipt = make_receipt(vec![log]);

        let result = compute_settled_amount(&receipt, &token, &receiver);
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // ERC-6909 tests
    // -----------------------------------------------------------------------

    #[test]
    fn erc6909_transfer_log_matched() {
        let token = Address::with_last_byte(0x10);
        let from = Address::with_last_byte(0x11);
        let receiver = Address::with_last_byte(0x12);

        // ERC-6909: data = (address caller [32 bytes], uint256 amount [32 bytes])
        let mut data = [0u8; 64];
        // caller address in first 32 bytes (right-aligned)
        data[12..32].copy_from_slice(Address::with_last_byte(0xca).as_slice());
        // amount in last 32 bytes
        let amount_bytes = encode_u256(750);
        data[32..].copy_from_slice(&amount_bytes);

        let log = make_log(
            token,
            vec![erc6909_topic(), addr_topic(from), addr_topic(receiver)],
            data.to_vec(),
        );
        let receipt = make_receipt(vec![log]);

        let result = compute_settled_amount(&receipt, &token, &receiver);
        assert_eq!(result, Some(BigUint::from(750u64)));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn no_matching_logs_returns_none() {
        let token = Address::with_last_byte(0x01);
        let receiver = Address::with_last_byte(0x03);

        // Log with unrelated topic
        let unrelated_topic = keccak256(b"Approval(address,address,uint256)");
        let log = make_log(
            token,
            vec![unrelated_topic, addr_topic(Address::ZERO), addr_topic(receiver)],
            encode_u256(100),
        );
        let receipt = make_receipt(vec![log]);

        let result = compute_settled_amount(&receipt, &token, &receiver);
        assert!(result.is_none());
    }

    #[test]
    fn empty_logs_returns_none() {
        let token = Address::with_last_byte(0x01);
        let receiver = Address::with_last_byte(0x03);
        let receipt = make_receipt(vec![]);
        assert!(compute_settled_amount(&receipt, &token, &receiver).is_none());
    }

    #[test]
    fn multiple_matching_logs_amounts_summed() {
        let token = Address::with_last_byte(0x01);
        let from = Address::with_last_byte(0x02);
        let receiver = Address::with_last_byte(0x03);

        let log1 = make_log(
            token,
            vec![erc20_topic(), addr_topic(from), addr_topic(receiver)],
            encode_u256(100),
        );
        let log2 = make_log(
            token,
            vec![erc20_topic(), addr_topic(from), addr_topic(receiver)],
            encode_u256(200),
        );
        // A log to a different receiver that should NOT be counted.
        let log3 = make_log(
            token,
            vec![erc20_topic(), addr_topic(from), addr_topic(Address::with_last_byte(0xff))],
            encode_u256(999),
        );
        let receipt = make_receipt(vec![log1, log2, log3]);

        let result = compute_settled_amount(&receipt, &token, &receiver);
        assert_eq!(result, Some(BigUint::from(300u64)));
    }
}
