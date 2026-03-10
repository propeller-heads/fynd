//! Cycle execution: encode, simulate, and submit cyclic arb transactions.
//!
//! Converts an `EvaluatedCycle` into an on-chain transaction via tycho-execution,
//! then either simulates (eth_simulate) or submits (public mempool / Flashbots).

use std::str::FromStr;

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, Keccak256, TxKind, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        TransactionInput, TransactionRequest,
    },
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use num_bigint::BigUint;
use tracing::{debug, error, info, warn};
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::{
        EncodedSolution, Solution as ExecutionSolution, Swap as ExecutionSwap,
        Transaction, UserTransferType,
    },
};
use tycho_simulation::{
    evm::protocol::u256_num::biguint_to_u256,
    tycho_common::{models::Chain, Bytes},
};

use fynd::feed::market_data::{SharedMarketData, SharedMarketDataRef};

use crate::types::{EvaluatedCycle, ExecutionMode, ExecutionResult};

/// Handles encoding and execution of profitable cycles.
///
/// Future improvements (from review):
/// - Return error for unsupported chains instead of silent mainnet fallback
/// - Add timeout on approval tx confirmation in execute_public
/// - Add state overrides for simulation with Address::ZERO
pub struct CycleExecutor {
    chain: Chain,
    slippage_bps: u32,
    /// Priority fee multiplier as a percentage of the network-suggested fee.
    /// 100 = use suggested fee, 200 = 2x (aggressive), 50 = 0.5x (passive).
    bribe_pct: u32,
    rpc_url: Option<String>,
    signer: Option<PrivateKeySigner>,
}

impl CycleExecutor {
    pub fn new(
        chain: Chain,
        slippage_bps: u32,
        bribe_pct: u32,
        rpc_url: Option<String>,
        private_key: Option<String>,
    ) -> anyhow::Result<Self> {
        let signer = private_key
            .map(|pk| {
                let pk = if pk.starts_with("0x") { pk } else { format!("0x{}", pk) };
                PrivateKeySigner::from_str(&pk)
                    .map_err(|e| anyhow::anyhow!("invalid private key: {}", e))
            })
            .transpose()?;

        Ok(Self { chain, slippage_bps, bribe_pct, rpc_url, signer })
    }

    /// Execute (or simulate) a profitable cycle.
    ///
    /// Takes `SharedMarketDataRef` (Arc<RwLock>) so the lock is only held
    /// during `build_solution` and released before any network I/O.
    pub async fn execute_cycle(
        &self,
        cycle: &EvaluatedCycle,
        market_ref: &SharedMarketDataRef,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
        mode: &ExecutionMode,
    ) -> ExecutionResult {
        match mode {
            ExecutionMode::LogOnly => ExecutionResult {
                tx_hash: None,
                success: true,
                gas_used: None,
                mode: mode.clone(),
                message: "log-only mode, no execution attempted".into(),
            },
            ExecutionMode::Simulate => {
                self.simulate(cycle, market_ref, source_token_addr).await
            }
            ExecutionMode::ExecutePublic => {
                self.execute_public(cycle, market_ref, source_token_addr).await
            }
            ExecutionMode::ExecuteProtected => {
                self.execute_protected(cycle, market_ref, source_token_addr).await
            }
        }
    }

    /// Build an `ExecutionSolution` from an `EvaluatedCycle`.
    fn build_solution(
        &self,
        cycle: &EvaluatedCycle,
        market: &SharedMarketData,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
        bot_address: Bytes,
    ) -> Result<ExecutionSolution, String> {
        let mut swaps = Vec::new();

        for (from_addr, to_addr, component_id) in &cycle.edges {
            let component = market
                .get_component(component_id)
                .ok_or_else(|| format!("component not found: {}", component_id))?;

            let swap = ExecutionSwap::new(
                component.clone(),
                Bytes::from(from_addr.as_ref()),
                Bytes::from(to_addr.as_ref()),
            );
            swaps.push(swap);
        }

        // For cyclic arb: given_token == checked_token == source token (WETH).
        // checked_amount = amount_out * (10000 - slippage) / 10000
        let bps = BigUint::from(10_000u32);
        let slippage = BigUint::from(self.slippage_bps);
        let multiplier = &bps - &slippage;
        let checked_amount = (&cycle.amount_out * &multiplier) / &bps;

        Ok(ExecutionSolution {
            sender: bot_address.clone(),
            receiver: bot_address,
            given_token: Bytes::from(source_token_addr.as_ref()),
            given_amount: cycle.optimal_amount_in.clone(),
            checked_token: Bytes::from(source_token_addr.as_ref()),
            exact_out: false,
            checked_amount,
            swaps,
            ..Default::default()
        })
    }

    /// Encode an `ExecutionSolution` into calldata via TychoRouter.
    fn encode_solution(
        &self,
        solution: &ExecutionSolution,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
    ) -> Result<Transaction, String> {
        let swap_encoder_registry = SwapEncoderRegistry::new(self.chain)
            .add_default_encoders(None)
            .map_err(|e| format!("swap encoder registry: {}", e))?;

        let encoder = TychoRouterEncoderBuilder::new()
            .chain(self.chain)
            .user_transfer_type(UserTransferType::TransferFrom)
            .swap_encoder_registry(swap_encoder_registry)
            .build()
            .map_err(|e| format!("encoder build: {}", e))?;

        let encoded_solutions = encoder
            .encode_solutions(vec![solution.clone()])
            .map_err(|e| format!("encode: {}", e))?;

        let encoded = encoded_solutions
            .into_iter()
            .next()
            .ok_or("no encoded solution produced")?;

        // Encode TransferFrom-mode router call (no permit)
        encode_tycho_router_call_no_permit(
            encoded,
            solution,
            Bytes::from(source_token_addr.as_ref()),
        )
    }

    /// Build approval + swap `TransactionRequest`s for eth_simulate.
    ///
    /// Gas limits are estimated via `eth_estimateGas` with a 20% buffer.
    /// Priority fee is scaled by `bribe_pct` relative to the network
    /// suggested fee (from `eth_maxPriorityFeePerGas`).
    async fn build_tx_requests(
        &self,
        provider: &RootProvider<Ethereum>,
        amount_in: U256,
        user_address: Address,
        sell_token_address: Address,
        router_address: Address,
        tx: &Transaction,
        chain_id: u64,
    ) -> Result<(TransactionRequest, TransactionRequest), String> {
        let (base_fee, priority_fee, nonce) =
            self.fetch_tx_params(provider, user_address).await?;
        let max_fee_per_gas = base_fee + priority_fee;

        let approve_data = build_approval_calldata(&amount_in, router_address);

        let mut approval_request = TransactionRequest {
            to: Some(TxKind::Call(sell_token_address)),
            from: Some(user_address),
            value: None,
            input: TransactionInput {
                input: Some(AlloyBytes::from(approve_data)),
                data: None,
            },
            chain_id: Some(chain_id),
            max_fee_per_gas: Some(max_fee_per_gas.into()),
            max_priority_fee_per_gas: Some(priority_fee.into()),
            nonce: Some(nonce),
            ..Default::default()
        };

        let approval_gas = estimate_gas_with_buffer(provider, &approval_request, 100_000)
            .await;
        approval_request.gas = Some(approval_gas);

        let mut swap_request = TransactionRequest {
            to: Some(TxKind::Call(Address::from_slice(&tx.to))),
            from: Some(user_address),
            value: Some(biguint_to_u256(&tx.value)),
            input: TransactionInput {
                input: Some(AlloyBytes::from(tx.data.clone())),
                data: None,
            },
            chain_id: Some(chain_id),
            max_fee_per_gas: Some(max_fee_per_gas.into()),
            max_priority_fee_per_gas: Some(priority_fee.into()),
            nonce: Some(nonce + 1),
            ..Default::default()
        };

        let swap_gas = estimate_gas_with_buffer(provider, &swap_request, 800_000)
            .await;
        swap_request.gas = Some(swap_gas);

        Ok((approval_request, swap_request))
    }

    /// Fetch base fee, bribe-scaled priority fee, and current nonce.
    async fn fetch_tx_params(
        &self,
        provider: &RootProvider<Ethereum>,
        user_address: Address,
    ) -> Result<(u64, u64, u64), String> {
        let block = provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
            .await
            .map_err(|e| format!("get block: {}", e))?
            .ok_or("block not found")?;

        let base_fee = block.header.base_fee_per_gas.ok_or("no base fee")?;

        // Fetch network-suggested priority fee, scale by bribe_pct
        let suggested_priority = provider
            .get_max_priority_fee_per_gas()
            .await
            .unwrap_or(1_000_000_000); // 1 gwei fallback
        let priority_fee = suggested_priority * self.bribe_pct as u128 / 100;
        let priority_fee = priority_fee.min(u64::MAX as u128) as u64;

        let nonce = provider
            .get_transaction_count(user_address)
            .await
            .map_err(|e| format!("get nonce: {}", e))?;

        // Use 2 * base_fee to handle EIP-1559 base fee volatility.
        // Base fee can increase up to 12.5% per block; 2x covers ~6 blocks of
        // consecutive increases, which is plenty for search latency.
        Ok((base_fee * 2, priority_fee, nonce))
    }

    /// Simulate the cycle via eth_simulate (no real tx sent).
    async fn simulate(
        &self,
        cycle: &EvaluatedCycle,
        market_ref: &SharedMarketDataRef,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
    ) -> ExecutionResult {
        let rpc_url = match &self.rpc_url {
            Some(u) => u.clone(),
            None => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::Simulate,
                    message: "RPC_URL required for simulation".into(),
                }
            }
        };

        let bot_address = self
            .signer
            .as_ref()
            .map(|s| Bytes::from(s.address().as_slice()))
            .unwrap_or_else(|| Bytes::from(Address::ZERO.as_slice()));

        let user_address = self
            .signer
            .as_ref()
            .map(|s| s.address())
            .unwrap_or(Address::ZERO);

        // Hold the market lock only for build_solution, then drop before network I/O
        let market = market_ref.read().await;
        let solution = match self.build_solution(cycle, &market, source_token_addr, bot_address) {
            Ok(s) => s,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::Simulate,
                    message: format!("build solution: {}", e),
                }
            }
        };
        drop(market);

        let tx = match self.encode_solution(&solution, source_token_addr) {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::Simulate,
                    message: format!("encode: {}", e),
                }
            }
        };

        debug!(
            router = %hex::encode(&tx.to),
            calldata_len = tx.data.len(),
            "encoded cycle for simulation"
        );

        let provider = match ProviderBuilder::default().connect(&rpc_url).await {
            Ok(p) => p,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::Simulate,
                    message: format!("RPC connect: {}", e),
                }
            }
        };

        let chain_id = chain_to_id(self.chain);
        let router_address = Address::from_slice(&tx.to);
        let sell_token_address = Address::from_slice(source_token_addr.as_ref());

        let (approval_request, swap_request) = match self
            .build_tx_requests(
                &provider,
                biguint_to_u256(&cycle.optimal_amount_in),
                user_address,
                sell_token_address,
                router_address,
                &tx,
                chain_id,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::Simulate,
                    message: format!("build tx requests: {}", e),
                }
            }
        };

        // State overrides: give the sender 1000 ETH so the simulation
        // doesn't fail with "insufficient funds". The swap calldata is
        // what we're testing, not the sender's balance.
        let overrides = alloy::rpc::types::state::StateOverride::from_iter([(
            user_address,
            alloy::rpc::types::state::AccountOverride {
                balance: Some(U256::from(1_000u64) * U256::from(10u64).pow(U256::from(18u64))),
                ..Default::default()
            },
        )]);

        let payload = SimulatePayload {
            block_state_calls: vec![SimBlock {
                block_overrides: None,
                state_overrides: Some(overrides),
                calls: vec![approval_request, swap_request],
            }],
            trace_transfers: true,
            // Disable validation: sender may not have signed txs (simulation only)
            validation: false,
            return_full_transactions: true,
        };

        match provider.simulate(&payload).await {
            Ok(output) => {
                let mut all_success = true;
                let mut total_gas = 0u64;
                for block in output.iter() {
                    for (j, call_result) in block.calls.iter().enumerate() {
                        let tx_name = if j == 0 { "approval" } else { "swap" };
                        if !call_result.status {
                            all_success = false;
                            warn!(tx = tx_name, "simulation tx failed");
                        }
                        total_gas += call_result.gas_used;
                        info!(
                            tx = tx_name,
                            status = call_result.status,
                            gas = call_result.gas_used,
                            "simulation result"
                        );
                    }
                }
                ExecutionResult {
                    tx_hash: None,
                    success: all_success,
                    gas_used: Some(total_gas),
                    mode: ExecutionMode::Simulate,
                    message: if all_success {
                        "simulation passed".into()
                    } else {
                        "simulation: one or more txs failed".into()
                    },
                }
            }
            Err(e) => ExecutionResult {
                tx_hash: None,
                success: false,
                gas_used: None,
                mode: ExecutionMode::Simulate,
                message: format!("eth_simulate error: {}", e),
            },
        }
    }

    /// Sign and send via public mempool.
    async fn execute_public(
        &self,
        cycle: &EvaluatedCycle,
        market_ref: &SharedMarketDataRef,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
    ) -> ExecutionResult {
        let (rpc_url, signer) = match (&self.rpc_url, &self.signer) {
            (Some(u), Some(s)) => (u.clone(), s.clone()),
            _ => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: "RPC_URL and PRIVATE_KEY required for execution".into(),
                }
            }
        };

        let bot_address = Bytes::from(signer.address().as_slice());
        let user_address = signer.address();

        // Hold market lock only for build_solution, then drop before network I/O
        let market = market_ref.read().await;
        let solution =
            match self.build_solution(cycle, &market, source_token_addr, bot_address) {
                Ok(s) => s,
                Err(e) => {
                    return ExecutionResult {
                        tx_hash: None,
                        success: false,
                        gas_used: None,
                        mode: ExecutionMode::ExecutePublic,
                        message: format!("build solution: {}", e),
                    }
                }
            };
        drop(market);

        let tx = match self.encode_solution(&solution, source_token_addr) {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("encode: {}", e),
                }
            }
        };

        let wallet = alloy::network::EthereumWallet::from(signer);
        let chain_id = chain_to_id(self.chain);
        let named_chain = alloy_chains::NamedChain::try_from(chain_id)
            .unwrap_or(alloy_chains::NamedChain::Mainnet);
        let provider = match ProviderBuilder::default()
            .with_chain(named_chain)
            .wallet(wallet)
            .connect(&rpc_url)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("RPC connect: {}", e),
                }
            }
        };

        let router_address = Address::from_slice(&tx.to);
        let sell_token = Address::from_slice(source_token_addr.as_ref());

        // Build approval + swap requests using the base provider (via deref)
        let base_provider: &RootProvider<Ethereum> = provider.root();
        let (approval_request, swap_request) = match self
            .build_tx_requests(
                base_provider,
                biguint_to_u256(&cycle.optimal_amount_in),
                user_address,
                sell_token,
                router_address,
                &tx,
                chain_id,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("build tx requests: {}", e),
                }
            }
        };

        // Send approval
        info!("sending approval tx...");
        let approval_receipt = match provider.send_transaction(approval_request).await {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("approval send: {}", e),
                }
            }
        };
        let approval_result = match approval_receipt.get_receipt().await {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("approval receipt: {}", e),
                }
            }
        };
        if !approval_result.status() {
            return ExecutionResult {
                tx_hash: Some(format!("{:?}", approval_result.transaction_hash)),
                success: false,
                gas_used: Some(approval_result.gas_used),
                mode: ExecutionMode::ExecutePublic,
                message: "approval tx reverted".into(),
            };
        }

        // Send swap
        info!("sending swap tx...");
        let swap_receipt = match provider.send_transaction(swap_request).await {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("swap send: {}", e),
                }
            }
        };
        let swap_result = match swap_receipt.get_receipt().await {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecutePublic,
                    message: format!("swap receipt: {}", e),
                }
            }
        };

        let total_gas = approval_result.gas_used + swap_result.gas_used;
        ExecutionResult {
            tx_hash: Some(format!("{:?}", swap_result.transaction_hash)),
            success: swap_result.status(),
            gas_used: Some(total_gas),
            mode: ExecutionMode::ExecutePublic,
            message: if swap_result.status() {
                "swap executed successfully".into()
            } else {
                "swap tx reverted".into()
            },
        }
    }

    /// Sign locally and POST raw signed tx to Flashbots Protect.
    ///
    /// Does NOT broadcast to the public mempool. Requires the bot to have
    /// a persistent approval to the TychoRouter (set up once beforehand).
    async fn execute_protected(
        &self,
        cycle: &EvaluatedCycle,
        market_ref: &SharedMarketDataRef,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
    ) -> ExecutionResult {
        let (rpc_url, signer) = match (&self.rpc_url, &self.signer) {
            (Some(u), Some(s)) => (u.clone(), s.clone()),
            _ => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: "RPC_URL and PRIVATE_KEY required for protected execution".into(),
                }
            }
        };

        let bot_address = Bytes::from(signer.address().as_slice());

        // Hold market lock only for build_solution, then drop before network I/O
        let market = market_ref.read().await;
        let solution =
            match self.build_solution(cycle, &market, source_token_addr, bot_address) {
                Ok(s) => s,
                Err(e) => {
                    return ExecutionResult {
                        tx_hash: None,
                        success: false,
                        gas_used: None,
                        mode: ExecutionMode::ExecuteProtected,
                        message: format!("build solution: {}", e),
                    }
                }
            };
        drop(market);

        let tx = match self.encode_solution(&solution, source_token_addr) {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: format!("encode: {}", e),
                }
            }
        };

        // Use a read-only provider for nonce/gas queries (no wallet, no broadcasting)
        let provider = match ProviderBuilder::default().connect(&rpc_url).await {
            Ok(p) => p,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: format!("RPC connect: {}", e),
                }
            }
        };

        let chain_id = chain_to_id(self.chain);
        let user_address = signer.address();

        // Build swap-only request at CURRENT nonce (no approval tx in this flow;
        // bot must have a persistent approval to the TychoRouter).
        let swap_request = match self
            .build_swap_only_request(
                &provider,
                user_address,
                &tx,
                chain_id,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: format!("build tx: {}", e),
                }
            }
        };

        // Sign locally via NetworkWallet without broadcasting
        let wallet = alloy::network::EthereumWallet::from(signer);
        use alloy::network::TransactionBuilder;
        let typed_tx = match swap_request.clone().build_unsigned() {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: format!("build unsigned tx: {}", e),
                }
            }
        };
        let tx_envelope = match <alloy::network::EthereumWallet as alloy::network::NetworkWallet<
            Ethereum,
        >>::sign_transaction(&wallet, typed_tx)
        .await
        {
            Ok(t) => t,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: format!("sign tx: {}", e),
                }
            }
        };

        // Encode to raw RLP bytes for Flashbots submission
        use alloy::eips::Encodable2718;
        let raw_bytes = tx_envelope.encoded_2718();
        let raw_tx_hex = format!("0x{}", hex::encode(&raw_bytes));

        let tx_hash = format!("0x{}", hex::encode(tx_envelope.tx_hash().as_slice()));
        info!(tx_hash = %tx_hash, "tx signed locally, sending to Flashbots Protect");

        // POST raw signed tx to Flashbots Protect (NOT the user's RPC)
        let client = reqwest::Client::new();
        let fb_resp = client
            .post("https://rpc.flashbots.net")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendRawTransaction",
                "params": [raw_tx_hex]
            }))
            .send()
            .await;

        match fb_resp {
            Ok(resp) if resp.status().is_success() => {
                info!("tx submitted to Flashbots Protect");
                ExecutionResult {
                    tx_hash: Some(tx_hash),
                    success: true,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: "submitted to Flashbots Protect (pending inclusion)".into(),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                error!(status = %status, body = %body, "Flashbots rejected tx");
                ExecutionResult {
                    tx_hash: Some(tx_hash),
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteProtected,
                    message: format!("Flashbots rejected: {} {}", status, body),
                }
            }
            Err(e) => ExecutionResult {
                tx_hash: Some(tx_hash),
                success: false,
                gas_used: None,
                mode: ExecutionMode::ExecuteProtected,
                message: format!("Flashbots POST failed: {}", e),
            },
        }
    }

    /// Build a single swap `TransactionRequest` at current nonce (no approval).
    ///
    /// Used by `execute_protected` where the bot already has a persistent
    /// approval to the TychoRouter.
    async fn build_swap_only_request(
        &self,
        provider: &RootProvider<Ethereum>,
        user_address: Address,
        tx: &Transaction,
        chain_id: u64,
    ) -> Result<TransactionRequest, String> {
        let (base_fee, priority_fee, nonce) =
            self.fetch_tx_params(provider, user_address).await?;
        let max_fee_per_gas = base_fee + priority_fee;

        let mut swap_request = TransactionRequest {
            to: Some(TxKind::Call(Address::from_slice(&tx.to))),
            from: Some(user_address),
            value: Some(biguint_to_u256(&tx.value)),
            input: TransactionInput {
                input: Some(AlloyBytes::from(tx.data.clone())),
                data: None,
            },
            chain_id: Some(chain_id),
            max_fee_per_gas: Some(max_fee_per_gas.into()),
            max_priority_fee_per_gas: Some(priority_fee.into()),
            nonce: Some(nonce), // current nonce, no approval tx before this
            ..Default::default()
        };

        let gas = estimate_gas_with_buffer(provider, &swap_request, 800_000).await;
        swap_request.gas = Some(gas);

        Ok(swap_request)
    }
}

// ==================== Helpers ====================

/// Estimate gas for a transaction with a 20% buffer.
///
/// Falls back to `fallback_gas` if the estimate fails (e.g. the sender
/// has no balance, which is common in simulation mode).
async fn estimate_gas_with_buffer(
    provider: &RootProvider<Ethereum>,
    tx: &TransactionRequest,
    fallback_gas: u64,
) -> u64 {
    match provider.estimate_gas(tx.clone()).await {
        Ok(estimated) => {
            let buffered = estimated * 120 / 100; // 20% buffer
            debug!(estimated, buffered, "gas estimate");
            buffered
        }
        Err(e) => {
            debug!(fallback = fallback_gas, error = %e, "gas estimate failed, using fallback");
            fallback_gas
        }
    }
}

/// Convert tycho Chain to numeric chain ID.
fn chain_to_id(chain: Chain) -> u64 {
    match chain {
        Chain::Ethereum => 1,
        Chain::Base => 8453,
        Chain::Arbitrum => 42161,
        Chain::ZkSync => 324,
        Chain::Unichain => 130,
        _ => 1, // fallback to mainnet for unsupported chains
    }
}

/// Encode `approve(address,uint256)` calldata.
fn build_approval_calldata(amount: &U256, spender: Address) -> Vec<u8> {
    let args = (spender, *amount).abi_encode();
    encode_input("approve(address,uint256)", args)
}

/// Compute 4-byte selector + ABI-encoded args.
fn encode_input(selector: &str, mut encoded_args: Vec<u8>) -> Vec<u8> {
    let mut hasher = Keccak256::new();
    hasher.update(selector.as_bytes());
    let selector_bytes = &hasher.finalize()[..4];
    let mut call_data = selector_bytes.to_vec();

    // Remove extra ABI prefix if present (dynamic encoding artifact)
    if encoded_args.len() > 32
        && encoded_args[..32]
            == [0u8; 31]
                .into_iter()
                .chain([32].to_vec())
                .collect::<Vec<u8>>()
    {
        encoded_args = encoded_args[32..].to_vec();
    }

    call_data.extend(encoded_args);
    call_data
}

/// Encode router call for TransferFrom mode (no permit signature needed).
fn encode_tycho_router_call_no_permit(
    encoded_solution: EncodedSolution,
    solution: &ExecutionSolution,
    native_address: Bytes,
) -> Result<Transaction, String> {
    let given_amount = biguint_to_u256(&solution.given_amount);
    let min_amount_out = biguint_to_u256(&solution.checked_amount);
    let given_token = Address::from_slice(&solution.given_token);
    let checked_token = Address::from_slice(&solution.checked_token);
    let receiver = Address::from_slice(&solution.receiver);

    debug!(
        given_amount = %given_amount,
        min_amount_out = %min_amount_out,
        given_token = ?given_token,
        checked_token = ?checked_token,
        receiver = ?receiver,
        "encoding TransferFrom router call"
    );

    let method_calldata = (
        given_amount,
        given_token,
        checked_token,
        min_amount_out,
        false, // wrap
        false, // unwrap
        receiver,
        true, // transfer_from = true
        encoded_solution.swaps.clone(),
    )
        .abi_encode();

    let contract_interaction =
        encode_input(&encoded_solution.function_signature, method_calldata);

    let value = if solution.given_token == native_address {
        solution.given_amount.clone()
    } else {
        BigUint::ZERO
    };

    Ok(Transaction {
        to: encoded_solution.interacting_with,
        value,
        data: contract_interaction,
    })
}
