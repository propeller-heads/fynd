use std::collections::HashMap;
use alloy::{primitives::B256, signers::local::PrivateKeySigner};
use num_bigint::BigUint;
use tycho_execution::encoding::{
    evm::encoder_builders::TychoRouterEncoderBuilder,
    models::{Solution, Transaction, UserTransferType},
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

/// Custom error types for execution failures
#[derive(Debug)]
pub enum ExecutorError {
    EncodingError(String),
    RpcError(String),
    ValidationError(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EncodingError(msg) => write!(f, "Failed to encode solution: {}", msg),
            Self::RpcError(msg) => write!(f, "RPC submission failed: {}", msg),
            Self::ValidationError(msg) => write!(f, "Invalid order or route: {}", msg),
        }
    }
}

impl std::error::Error for ExecutorError {}

/// Transaction status information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransactionStatus {
    pub hash: String,
    pub status: TxStatus,
    pub block_number: Option<u64>,
    pub confirmations: Option<u64>,
    pub gas_used: Option<u64>,
}

/// Transaction status enum  
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TxStatus {
    Pending,
    Confirmed,
    Failed,
    Unknown,
}

/// Result of simulating a transaction
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Whether the transaction would succeed
    pub success: bool,
    /// Estimated gas usage
    pub gas_used: BigUint,
    /// Gas price used for estimation
    pub gas_price: BigUint,
    /// Revert reason if the transaction fails
    pub revert_reason: Option<String>,
    /// Expected output amounts
    pub output_amounts: Vec<BigUint>,
}

/// Executor handles route encoding and transaction submission
#[derive(Clone)]
pub struct Executor {
    rpc_url: String,
    chain: Chain,
    signer: Option<PrivateKeySigner>,
    allowed_slippage: f64,
    router_address: Bytes,
    user_transfer_type: UserTransferType,
}

impl Executor {
    pub fn new(
        rpc_url: String,
        chain: Chain,
        allowed_slippage: f64,
        router_address: Bytes,
        user_transfer_type: UserTransferType,
    ) -> Self {
        Self { rpc_url, chain, signer: None, allowed_slippage, router_address, user_transfer_type }
    }

    pub fn with_signer(mut self, signer: PrivateKeySigner) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Encode solutions without execution
    pub fn encode(&self, _solutions: &[Solution]) -> Result<Vec<Transaction>, ExecutorError> {
        // Initialize the encoder
        let _encoder = TychoRouterEncoderBuilder::new()
            .chain(self.chain)
            .user_transfer_type(self.user_transfer_type.clone())
            .router_address(self.router_address.clone())
            .build()
            .expect("Failed to build encoder");

        // encode solutions with tycho-encoding
        // encode tycho router call. CAREFUL WITH THE min amount out <- this is the sensitive bit
        todo!("Implement Solution encoding using TychoRouterEncoderBuilder")
    }

    /// Execute solutions as transactions
    pub async fn execute(&self, _transactions: &[Transaction]) -> Result<Vec<B256>, ExecutorError> {
        // 1. Option A: Submit each solution as separate transaction (parallel execution)
        // 2. Option B: Combine multiple solutions into single multicall transaction
        // 4. Build TransactionRequest with calldata from encoded_solutions
        // 5. Set gas price, gas limit, nonce, etc.
        // 6. Sign transactions with self.signer
        // 7. Submit to self.rpc_url using alloy provider
        // 8. Return vector of transaction hashes
        // 9. For single solution, just pass vec with one element
        todo!("Implement transaction building and submission")
    }

    /// Simulate solutions to estimate gas, check for reverts, etc.
    pub async fn simulate(
        &self,
        _transactions: &[Transaction],
    ) -> Result<Vec<SimulationResult>, ExecutorError> {
        // 1. Encode solutions to get calldata
        // 2. Use eth_call or tenderly/anvil simulation APIs (see tycho-test for inspiration as
        //    well)
        // 3. Estimate gas usage for each transaction
        // 4. Check for potential reverts
        // 5. Return simulation results with gas estimates, success/failure, etc.
        // 6. Consider using self.allowed_slippage for realistic simulations
        todo!("Implement transaction simulation")
    }

    /// Track the status of multiple transactions
    /// 
    /// Takes transaction hashes and returns their current status including
    /// block number, confirmation count, and execution status.
    pub async fn track_transactions(
        &self,
        tx_hashes: &[String],
    ) -> Result<HashMap<String, TransactionStatus>, ExecutorError> {
        // 1. Use alloy provider to query transaction status for each hash
        // 2. Get transaction receipt if available
        // 3. Calculate confirmations based on current block - tx block
        // 4. Determine status (pending/confirmed/failed) from receipt
        // 5. Extract gas usage from receipt
        // 6. Return HashMap mapping each hash to its status
        
        let mut statuses = HashMap::new();
        
        for hash in tx_hashes {
            // Placeholder implementation - in real implementation would query RPC
            let status = TransactionStatus {
                hash: hash.clone(),
                status: TxStatus::Unknown, // Would query actual status
                block_number: None,        // Would get from receipt
                confirmations: None,       // Would calculate from current block
                gas_used: None,           // Would get from receipt
            };
            statuses.insert(hash.clone(), status);
        }
        
        Ok(statuses)
        // TODO: Implement actual transaction tracking using:
        // - self.rpc_url for provider connection
        // - provider.get_transaction_by_hash() for transaction details  
        // - provider.get_transaction_receipt() for receipt/confirmation
        // - provider.get_block_number() for current block to calc confirmations
    }
}
