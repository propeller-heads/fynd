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

use crate::flash_loan::{
    encode_flash_loan_call, encode_flash_swap_v2_call, FlashTier,
};
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
    /// Address of deployed FlashArbExecutor contract (for flash modes).
    flash_arb_address: Option<Address>,
}

impl CycleExecutor {
    pub fn new(
        chain: Chain,
        slippage_bps: u32,
        bribe_pct: u32,
        rpc_url: Option<String>,
        private_key: Option<String>,
        flash_arb_address: Option<Address>,
    ) -> anyhow::Result<Self> {
        let signer = private_key
            .map(|pk| {
                let pk = if pk.starts_with("0x") { pk } else { format!("0x{}", pk) };
                PrivateKeySigner::from_str(&pk)
                    .map_err(|e| anyhow::anyhow!("invalid private key: {}", e))
            })
            .transpose()?;

        Ok(Self { chain, slippage_bps, bribe_pct, rpc_url, signer, flash_arb_address })
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
            ExecutionMode::SimulateFlash => {
                self.simulate_flash(cycle, market_ref, source_token_addr).await
            }
            ExecutionMode::ExecuteFlash => {
                self.execute_flash(cycle, market_ref, source_token_addr).await
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
            true, // transfer_from: pull tokens from sender
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

    // ==================== Flash Loan Methods ====================

    /// Build an `ExecutionSolution` for flash mode.
    ///
    /// `sender` and `receiver` are set to the flash arb contract address.
    /// For Tier 2 (Balancer), all hops are included.
    fn build_solution_flash(
        &self,
        cycle: &EvaluatedCycle,
        market: &SharedMarketData,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
        flash_arb_addr: Bytes,
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

        let bps = BigUint::from(10_000u32);
        let slippage = BigUint::from(self.slippage_bps);
        let multiplier = &bps - &slippage;
        let checked_amount = (&cycle.amount_out * &multiplier) / &bps;

        Ok(ExecutionSolution {
            sender: flash_arb_addr.clone(),
            receiver: flash_arb_addr,
            given_token: Bytes::from(source_token_addr.as_ref()),
            given_amount: cycle.optimal_amount_in.clone(),
            checked_token: Bytes::from(source_token_addr.as_ref()),
            exact_out: false,
            checked_amount,
            swaps,
            ..Default::default()
        })
    }

    /// Select flash tier based on first pool's protocol.
    ///
    /// Tier 1 (UniV2 flash swap) when first pool is UniV2/SushiV2.
    /// Tier 2 (Balancer flash loan) is the fallback.
    fn select_flash_tier(
        &self,
        cycle: &EvaluatedCycle,
        market: &SharedMarketData,
    ) -> FlashTier {
        if cycle.edges.len() < 2 {
            // Need at least 2 hops for Tier 1 (flash swap + rest)
            return FlashTier::BalancerLoan;
        }

        let (from_addr, _to_addr, component_id) = &cycle.edges[0];
        let component = match market.get_component(component_id) {
            Some(c) => c,
            None => return FlashTier::BalancerLoan,
        };

        let proto = component.protocol_system.as_str();
        debug!(
            first_pool_proto = proto,
            component_id = component_id.as_str(),
            "flash tier: checking first pool"
        );
        if proto != "uniswap_v2" && proto != "sushiswap_v2" {
            debug!("flash tier: falling back to Tier 2 (non-V2 first pool)");
            return FlashTier::BalancerLoan;
        }

        // Pair address: component ID is the pair address for V2
        let pair_address = component_id
            .parse::<Address>()
            .unwrap_or_else(|_| {
                component
                    .contract_addresses
                    .first()
                    .map(|b| Address::from_slice(b.as_ref()))
                    .unwrap_or(Address::ZERO)
            });

        // token0 is first in component's token list (sorted, matches
        // on-chain order). zeroForOne = input token IS token0.
        let token0 = component.tokens.first();
        let input_token = Bytes::from(from_addr.as_ref());
        let zero_for_one = token0
            .map(|t0| t0.as_ref() == input_token.as_ref())
            .unwrap_or(true);

        // Simulate first hop to get amount_out (intermediate token)
        let sim_state = match market.get_simulation_state(component_id) {
            Some(s) => s,
            None => return FlashTier::BalancerLoan,
        };
        let token_in = match market.get_token(from_addr) {
            Some(t) => t.clone(),
            None => return FlashTier::BalancerLoan,
        };
        let to_addr = &cycle.edges[0].1;
        let token_out = match market.get_token(to_addr) {
            Some(t) => t.clone(),
            None => return FlashTier::BalancerLoan,
        };
        let sim_result = match sim_state.get_amount_out(
            cycle.optimal_amount_in.clone(),
            &token_in,
            &token_out,
        ) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Tier 1 sim failed, using Tier 2");
                return FlashTier::BalancerLoan;
            }
        };
        let amount_out = biguint_to_u256(&sim_result.amount);

        // Repay amount = optimal_amount_in + 1 wei safety margin.
        // Flash swap fee = normal swap fee (0.3%). The K-invariant
        // check requires getAmountIn(amountOut) which equals the
        // input we would have spent on a normal swap. Add 1 wei
        // to cover integer rounding in the pair's K-check.
        let repay_amount = biguint_to_u256(
            &(&cycle.optimal_amount_in + BigUint::from(1u64)),
        );

        info!(
            tier = 1,
            pair = ?pair_address,
            zero_for_one,
            amount_out = %amount_out,
            repay = %repay_amount,
            "selected Tier 1: UniV2 flash swap"
        );

        FlashTier::V2FlashSwap {
            pair_address,
            zero_for_one,
            amount_out,
            repay_amount,
        }
    }

    /// Build an `ExecutionSolution` for V2 flash swap (Tier 1).
    ///
    /// Skips the first hop (the flash swap itself) and builds
    /// hops 2..N. `given_amount` is the intermediate token amount
    /// received from the pair.
    fn build_solution_flash_v2(
        &self,
        cycle: &EvaluatedCycle,
        market: &SharedMarketData,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
        flash_arb_addr: Bytes,
        intermediate_amount: &BigUint,
    ) -> Result<ExecutionSolution, String> {
        let mut swaps = Vec::new();
        for (from_addr, to_addr, component_id) in &cycle.edges[1..] {
            let component = market
                .get_component(component_id)
                .ok_or_else(|| {
                    format!("component not found: {}", component_id)
                })?;
            swaps.push(ExecutionSwap::new(
                component.clone(),
                Bytes::from(from_addr.as_ref()),
                Bytes::from(to_addr.as_ref()),
            ));
        }

        let intermediate_token = &cycle.edges[1].0;
        let bps = BigUint::from(10_000u32);
        let slippage = BigUint::from(self.slippage_bps);
        let multiplier = &bps - &slippage;
        let checked_amount = (&cycle.amount_out * &multiplier) / &bps;

        Ok(ExecutionSolution {
            sender: flash_arb_addr.clone(),
            receiver: flash_arb_addr,
            given_token: Bytes::from(intermediate_token.as_ref()),
            given_amount: intermediate_amount.clone(),
            checked_token: Bytes::from(source_token_addr.as_ref()),
            exact_out: false,
            checked_amount,
            swaps,
            ..Default::default()
        })
    }

    /// Encode an `ExecutionSolution` for flash mode (UserTransferType::TransferFrom).
    fn encode_solution_flash(
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

        encode_tycho_router_call_no_permit(
            encoded,
            solution,
            Bytes::from(source_token_addr.as_ref()),
            true, // transfer_from: router pulls tokens from flash_arb via approve
        )
    }

    /// Build solution + flash calldata for any tier.
    ///
    /// Returns (solution, flash_calldata, router_calldata).
    #[allow(clippy::type_complexity)]
    fn build_flash_calldata(
        &self,
        cycle: &EvaluatedCycle,
        market: &SharedMarketData,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
        flash_arb_addr: Bytes,
        tier: &FlashTier,
    ) -> Result<(ExecutionSolution, AlloyBytes, AlloyBytes), String> {
        let (solution, _given_token_addr) = match tier {
            FlashTier::V2FlashSwap { amount_out, .. } => {
                // Tier 1: hops 2..N only
                let intermediate_amount =
                    num_bigint::BigUint::from_bytes_be(
                        &amount_out.to_be_bytes::<32>(),
                    );
                let sol = self.build_solution_flash_v2(
                    cycle,
                    market,
                    source_token_addr,
                    flash_arb_addr,
                    &intermediate_amount,
                )?;
                // For Tier 1, the given token is the intermediate
                // token (output of first hop, input of second hop).
                let given_tok = cycle.edges[1].0.clone();
                (sol, given_tok)
            }
            FlashTier::BalancerLoan => {
                // Tier 2: all hops
                let sol = self.build_solution_flash(
                    cycle,
                    market,
                    source_token_addr,
                    flash_arb_addr,
                )?;
                let given_tok = source_token_addr.clone();
                (sol, given_tok)
            }
        };

        // Encode router calldata
        let router_tx = self.encode_solution_flash(
            &solution, source_token_addr,
        )?;
        let router_address = Address::from_slice(&router_tx.to);
        let router_calldata = AlloyBytes::from(router_tx.data.clone());

        if router_calldata.len() >= 4 {
            let sel: String = router_calldata[..4]
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();
            debug!(
                selector = %format!("0x{}", sel),
                router = ?router_address,
                calldata_len = router_calldata.len(),
                "router calldata selector"
            );
        }

        // Wrap in appropriate FlashArbExecutor call
        let flash_calldata = match tier {
            FlashTier::V2FlashSwap {
                pair_address,
                zero_for_one,
                amount_out,
                repay_amount,
            } => {
                debug!(
                    tier = 1,
                    pair = ?pair_address,
                    "encoding V2 flash swap"
                );
                encode_flash_swap_v2_call(
                    *pair_address,
                    *amount_out,
                    *zero_for_one,
                    *repay_amount,
                    router_address,
                    router_calldata.clone(),
                )
            }
            FlashTier::BalancerLoan => {
                let source_token =
                    Address::from_slice(source_token_addr.as_ref());
                let amount_in =
                    biguint_to_u256(&cycle.optimal_amount_in);
                debug!(tier = 2, "encoding Balancer flash loan");
                encode_flash_loan_call(
                    source_token,
                    amount_in,
                    router_address,
                    router_calldata.clone(),
                )
            }
        };

        debug!(
            calldata_len = flash_calldata.len(),
            "encoded flash call for simulation"
        );

        Ok((solution, flash_calldata, router_calldata))
    }

    /// Simulate a flash cycle via eth_call (auto-selects tier).
    async fn simulate_flash(
        &self,
        cycle: &EvaluatedCycle,
        market_ref: &SharedMarketDataRef,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
    ) -> ExecutionResult {
        let flash_arb = match self.flash_arb_address {
            Some(addr) => addr,
            None => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::SimulateFlash,
                    message: "flash-arb-address required for flash mode".into(),
                }
            }
        };
        let rpc_url = match &self.rpc_url {
            Some(u) => u.clone(),
            None => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::SimulateFlash,
                    message: "RPC_URL required for simulation".into(),
                }
            }
        };

        let flash_arb_bytes = Bytes::from(flash_arb.as_slice());

        // Select tier and build solution + calldata
        let market = market_ref.read().await;
        let tier = self.select_flash_tier(cycle, &market);

        let (_solution, flash_calldata, _router_calldata) =
            match self.build_flash_calldata(
                cycle,
                &market,
                source_token_addr,
                flash_arb_bytes,
                &tier,
            ) {
                Ok(v) => v,
                Err(e) => {
                    return ExecutionResult {
                        tx_hash: None,
                        success: false,
                        gas_used: None,
                        mode: ExecutionMode::SimulateFlash,
                        message: e,
                    }
                }
            };
        drop(market);

        let provider: RootProvider<Ethereum> =
            match ProviderBuilder::default().connect(&rpc_url).await {
                Ok(p) => p,
                Err(e) => {
                    return ExecutionResult {
                        tx_hash: None,
                        success: false,
                        gas_used: None,
                        mode: ExecutionMode::SimulateFlash,
                        message: format!("RPC connect: {}", e),
                    }
                }
            };

        // Use bot address as sender (the contract owner)
        let user_address = self
            .signer
            .as_ref()
            .map(|s| s.address())
            .unwrap_or(Address::ZERO);

        // eth_call doesn't need gas pricing or nonce
        let flash_request = TransactionRequest {
            to: Some(TxKind::Call(flash_arb)),
            from: Some(user_address),
            input: TransactionInput {
                input: Some(flash_calldata),
                data: None,
            },
            gas: Some(1_500_000),
            ..Default::default()
        };

        // Use eth_call (universally supported) instead of eth_simulate
        match provider.call(flash_request.clone()).await {
            Ok(_output) => {
                // eth_call succeeded (no revert). Estimate gas.
                let gas_used = provider
                    .estimate_gas(flash_request)
                    .await
                    .ok();
                info!(
                    gas = ?gas_used,
                    "flash simulation passed (eth_call)"
                );
                ExecutionResult {
                    tx_hash: None,
                    success: true,
                    gas_used: gas_used.map(|g| g as u64),
                    mode: ExecutionMode::SimulateFlash,
                    message: "flash simulation passed".into(),
                }
            }
            Err(e) => {
                warn!(error = %e, "flash simulation reverted");

                // Diagnostic logging only
                ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::SimulateFlash,
                    message: format!(
                        "flash simulation reverted: {}",
                        e
                    ),
                }
            }
        }
    }

    /// Execute a flash loan cycle on-chain (auto-selects tier).
    async fn execute_flash(
        &self,
        cycle: &EvaluatedCycle,
        market_ref: &SharedMarketDataRef,
        source_token_addr: &tycho_simulation::tycho_common::models::Address,
    ) -> ExecutionResult {
        let flash_arb = match self.flash_arb_address {
            Some(addr) => addr,
            None => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteFlash,
                    message: "flash-arb-address required for flash mode".into(),
                }
            }
        };
        let (rpc_url, signer) = match (&self.rpc_url, &self.signer) {
            (Some(u), Some(s)) => (u.clone(), s.clone()),
            _ => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteFlash,
                    message: "RPC_URL and PRIVATE_KEY required for flash execution"
                        .into(),
                }
            }
        };

        let flash_arb_bytes = Bytes::from(flash_arb.as_slice());

        let market = market_ref.read().await;
        let tier = self.select_flash_tier(cycle, &market);

        let (_solution, flash_calldata, _router_calldata) =
            match self.build_flash_calldata(
                cycle,
                &market,
                source_token_addr,
                flash_arb_bytes,
                &tier,
            ) {
                Ok(v) => v,
                Err(e) => {
                    return ExecutionResult {
                        tx_hash: None,
                        success: false,
                        gas_used: None,
                        mode: ExecutionMode::ExecuteFlash,
                        message: e,
                    }
                }
            };
        drop(market);

        let wallet = alloy::network::EthereumWallet::from(signer.clone());
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
                    mode: ExecutionMode::ExecuteFlash,
                    message: format!("RPC connect: {}", e),
                }
            }
        };

        let user_address = signer.address();
        let base_provider: &RootProvider<Ethereum> = provider.root();
        let (base_fee, priority_fee, nonce) =
            match self.fetch_tx_params(base_provider, user_address).await {
                Ok(p) => p,
                Err(e) => {
                    return ExecutionResult {
                        tx_hash: None,
                        success: false,
                        gas_used: None,
                        mode: ExecutionMode::ExecuteFlash,
                        message: format!("fetch tx params: {}", e),
                    }
                }
            };
        let max_fee_per_gas = base_fee + priority_fee;

        let mut flash_request = TransactionRequest {
            to: Some(TxKind::Call(flash_arb)),
            from: Some(user_address),
            value: None,
            input: TransactionInput {
                input: Some(flash_calldata),
                data: None,
            },
            chain_id: Some(chain_id),
            max_fee_per_gas: Some(max_fee_per_gas.into()),
            max_priority_fee_per_gas: Some(priority_fee.into()),
            nonce: Some(nonce),
            ..Default::default()
        };

        let gas = estimate_gas_with_buffer(
            base_provider, &flash_request, 1_000_000,
        ).await;
        flash_request.gas = Some(gas);

        info!("sending flash loan tx...");
        let receipt = match provider.send_transaction(flash_request).await {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteFlash,
                    message: format!("flash tx send: {}", e),
                }
            }
        };
        let result = match receipt.get_receipt().await {
            Ok(r) => r,
            Err(e) => {
                return ExecutionResult {
                    tx_hash: None,
                    success: false,
                    gas_used: None,
                    mode: ExecutionMode::ExecuteFlash,
                    message: format!("flash tx receipt: {}", e),
                }
            }
        };

        ExecutionResult {
            tx_hash: Some(format!("{:?}", result.transaction_hash)),
            success: result.status(),
            gas_used: Some(result.gas_used),
            mode: ExecutionMode::ExecuteFlash,
            message: if result.status() {
                "flash arb executed successfully".into()
            } else {
                "flash arb tx reverted".into()
            },
        }
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

/// Encode router call (no permit signature needed).
///
/// `transfer_from`: when `true`, router pulls tokens from sender via
/// `transferFrom`. When `false` (flash mode), tokens are already in the
/// router so no pull is needed.
fn encode_tycho_router_call_no_permit(
    encoded_solution: EncodedSolution,
    solution: &ExecutionSolution,
    native_address: Bytes,
    transfer_from: bool,
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
        transfer_from = transfer_from,
        "encoding router call"
    );

    let method_calldata = (
        given_amount,
        given_token,
        checked_token,
        min_amount_out,
        false, // wrap
        false, // unwrap
        receiver,
        transfer_from,
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
