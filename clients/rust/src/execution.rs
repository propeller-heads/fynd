use std::sync::Arc;

use alloy::network::Ethereum;
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::Provider;

use crate::error::FyndClientError;

/// A handle to a submitted transaction, resolves to a `SettledOrder`.
#[derive(Clone)]
pub struct TransactionHandle {
    pub(crate) tx_hash: TxHash,
    pub(crate) provider: Arc<dyn Provider<Ethereum>>,
    pub(crate) token_out: Address,
    pub(crate) receiver: Address,
}

/// An order that has been confirmed on-chain.
#[derive(Debug, Clone)]
pub struct SettledOrder {
    pub tx_hash: TxHash,
    /// Amount of output token actually received, derived from on-chain Transfer logs.
    pub amount_received: U256,
    pub block_number: u64,
}

/// Receipt returned after broadcasting a signed order.
pub enum ExecutionReceipt {
    /// Transaction was submitted; poll `settle()` to wait for confirmation.
    Transaction(TransactionHandle),
    /// Turbine intent — not yet implemented.
    Intent,
}

// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC: alloy::primitives::B256 =
    alloy::primitives::b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

impl TransactionHandle {
    /// Waits for the transaction to be confirmed and returns the settled order.
    ///
    /// Derives `amount_received` from on-chain ERC-20 Transfer event logs —
    /// no debug RPC required.
    pub async fn settle(self) -> Result<SettledOrder, FyndClientError> {
        let receipt = self
            .provider
            .get_transaction_receipt(self.tx_hash)
            .await
            .map_err(|e| FyndClientError::Rpc(e.to_string()))?
            .ok_or_else(|| FyndClientError::Rpc("transaction not found".to_string()))?;

        let block_number = receipt
            .block_number
            .ok_or_else(|| FyndClientError::Rpc("receipt missing block number".to_string()))?;

        let amount_received = receipt
            .inner
            .logs()
            .iter()
            .filter(|log| {
                log.address() == self.token_out
                    && log.topics().first() == Some(&TRANSFER_TOPIC)
                    && log
                        .topics()
                        .get(2)
                        .map(|t| {
                            // indexed `to` address is padded to 32 bytes
                            let addr_bytes = &t.0[12..];
                            addr_bytes == self.receiver.as_slice()
                        })
                        .unwrap_or(false)
            })
            .filter_map(|log| {
                // Transfer value is in data (non-indexed)
                let data = log.data().data.as_ref();
                if data.len() >= 32 {
                    Some(U256::from_be_slice(&data[..32]))
                } else {
                    None
                }
            })
            .fold(U256::ZERO, |acc, v| acc + v);

        Ok(SettledOrder { tx_hash: self.tx_hash, amount_received, block_number })
    }
}
