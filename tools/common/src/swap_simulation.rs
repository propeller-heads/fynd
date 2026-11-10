//! On-chain re-execution of encoded swap calldata to measure real output and gas.
//!
//! [`EthCallRunner`] re-runs a quote's calldata at a pinned block using state overrides to inject
//! the sender's token balance and allowance. It prefers `eth_simulateV1` (a balance-delta
//! measurement that is independent of router return-data conventions) and falls back to a plain
//! `eth_call` with return-data heuristics when the RPC does not support `eth_simulateV1`.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use alloy::{
    hex,
    network::Ethereum,
    primitives::{map::B256HashMap, Address, TxKind, B256, U256},
    providers::{Provider, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        trace::geth::{
            CallConfig, CallFrame, GethDebugBuiltInTracerType, GethDebugTracerType,
            GethDebugTracingCallOptions, GethDebugTracingOptions,
        },
        BlockId, RpcBlockHash, TransactionRequest,
    },
    transports::{RpcError, TransportErrorKind},
};
use anyhow::Context;
use bytes::Bytes;
use fynd_core::simulation::{
    decode_error, reason_from_frame,
    token_layout::{discover_layout, TokenLayout},
};
use num_bigint::BigUint;
use tokio::sync::Mutex;
use tracing::warn;

/// Sentinel address for the inline ETH-balance reader injected into `eth_simulateV1` state.
/// Chosen to be obviously synthetic and not a real mainnet contract.
const ETH_BALANCE_READER_ADDR: Address = Address(alloy::primitives::FixedBytes([
    0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0,
    0xf0, 0xf0, 0xf0, 0xf0,
]));

/// Returns the native ETH balance of the calling address as a uint256.
const ETH_BALANCE_READER_CODE: [u8; 10] =
    [0x33, 0x31, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

/// What a swap did when [`EthCallRunner::execute`] ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapOutcome {
    /// The sender received `amount_out` of `token_out`. `gas` is `None` when neither
    /// `eth_simulateV1` nor `eth_estimateGas` reported it.
    Success { amount_out: BigUint, gas: Option<u64> },
    /// The swap reverted. `reason` names the error when the revert data or a trace of the call
    /// carries one, and holds the node's message otherwise.
    Reverted { reason: String },
}

/// A swap for [`EthCallRunner::execute`]: `calldata` sent to `router` with `value` wei, selling
/// `token_in` for `token_out`.
#[derive(Debug, Clone)]
pub struct SwapCall<'a> {
    pub token_in: Address,
    pub token_out: Address,
    pub router: Address,
    pub calldata: &'a [u8],
    pub value: &'a BigUint,
    /// The address the sender's `token_in` allowance is given to: the contract that pulls
    /// `token_in`, such as Permit2 for a router that pulls it through Permit2.
    pub spender: Address,
    /// State the swap reads besides the sender's `token_in` balance and allowance, such as a
    /// router's internal balance of the sender. It may not override the sender or the contract
    /// that holds the `token_in` balances.
    pub extra_state: StateOverride,
}

impl<'a> SwapCall<'a> {
    /// A swap that `router` pulls `token_in` for itself, with no extra state.
    pub fn new(
        token_in: Address,
        token_out: Address,
        router: Address,
        calldata: &'a [u8],
        value: &'a BigUint,
    ) -> Self {
        Self {
            token_in,
            token_out,
            router,
            calldata,
            value,
            spender: router,
            extra_state: StateOverride::default(),
        }
    }
}

/// Validates quotes by re-executing encoded calldata via simulation at the relevant block,
/// using state overrides to inject token balance and allowance.
///
/// Prefers `eth_simulateV1` (balance-delta: swap + `balanceOf` in one block) for an exact
/// output amount independent of router ABI conventions. Falls back to a plain `eth_call`
/// with return-data heuristics when the RPC does not support `eth_simulateV1`.
pub struct EthCallRunner {
    provider: Arc<RootProvider<Ethereum>>,
    /// Fixed sender used in all quotes — overridden in state to hold sufficient balance.
    sender: Address,
    /// Per-(token, spender) storage layout (discovered once, cached for the run).
    layouts: Arc<Mutex<HashMap<(Address, Address), TokenLayout>>>,
    /// Set to `false` after the first `eth_simulateV1` "method not found" error so subsequent
    /// calls skip straight to the `eth_call` fallback without retrying.
    simulate_supported: Arc<AtomicBool>,
    /// Tokens whose `balanceOf` reads the holder's native balance, such as USDC on Arc.
    native_balance_tokens: HashSet<Address>,
}

impl EthCallRunner {
    pub fn new(provider: RootProvider<Ethereum>) -> Self {
        // Generate a fresh random address so it has no pre-existing token balances.
        let mut bytes = [0u8; 20];
        fastrand::fill(&mut bytes);
        Self::with_sender(provider, Address::from(bytes))
    }

    /// A runner that sends every swap from `sender`, for a caller that signs as the sender, such
    /// as a Permit2 permit. The sender's ETH and `token_in` balances are overridden.
    pub fn with_sender(provider: RootProvider<Ethereum>, sender: Address) -> Self {
        Self {
            provider: Arc::new(provider),
            sender,
            layouts: Arc::new(Mutex::new(HashMap::new())),
            simulate_supported: Arc::new(AtomicBool::new(true)),
            native_balance_tokens: HashSet::new(),
        }
    }

    /// Marks `token` as a token whose `balanceOf` reads the holder's native balance.
    ///
    /// Such a token is funded by the sender's native balance override, and its allowance is set by
    /// an `approve` call before the swap, because a node can refuse overrides of its storage. Only
    /// `eth_simulateV1` runs that call, so a swap that sells `token` fails on the `eth_call`
    /// fallback.
    pub fn with_native_balance_token(mut self, token: Address) -> Self {
        self.native_balance_tokens.insert(token);
        self
    }

    fn is_native_balance_token(&self, token: Address) -> bool {
        self.native_balance_tokens
            .contains(&token)
    }

    pub fn sender_hex(&self) -> String {
        format!("0x{}", hex::encode(self.sender.as_slice()))
    }

    /// Hash of the latest canonical block, used to pin every quote and `eth_call` in a batch to
    /// the same state. Returns `None` (with a warning) when the block cannot be fetched.
    pub async fn latest_block_hash(&self) -> Option<B256> {
        match self
            .provider
            .get_block_by_number(Default::default())
            .await
        {
            Ok(Some(block)) => Some(block.header.hash),
            Ok(None) => {
                warn!("could not fetch latest block for eth_call validation");
                None
            }
            Err(e) => {
                warn!("could not fetch block for eth_call validation: {e}");
                None
            }
        }
    }

    /// Execute `swap` at `block`, returning the actual `token_out` amount received by the
    /// runner's sender and, when `eth_simulateV1` is used, the gas consumed.
    ///
    /// Tries `eth_simulateV1` first (balance-delta approach). Falls back to `eth_call` +
    /// return-data parsing when the RPC does not support `eth_simulateV1`. Gas is `None`
    /// on the fallback path.
    ///
    /// Returns [`SwapOutcome::Reverted`] when the swap reverts, and `Err` when the node fails to
    /// answer, for example on a rate limit, a timeout or missing state, or answers with data the
    /// runner cannot read, or when `swap.extra_state` overrides the sender or the `token_in`
    /// balances.
    pub async fn execute(&self, swap: &SwapCall<'_>, block: B256) -> anyhow::Result<SwapOutcome> {
        if self
            .simulate_supported
            .load(Ordering::Relaxed)
        {
            match self.try_simulate(swap, block).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) => {
                    if is_method_not_found(&e) {
                        self.simulate_supported
                            .store(false, Ordering::Relaxed);
                        warn!(
                            "eth_simulateV1 not supported on this RPC — falling back to eth_call"
                        );
                    } else {
                        warn!("eth_simulateV1 failed: {e}");
                    }
                }
            }
        }
        let (amount_res, gas_res) =
            tokio::join!(self.try_eth_call(swap, block), self.try_estimate_gas(swap, block));
        Ok(match amount_res? {
            SwapOutcome::Success { amount_out, .. } => {
                SwapOutcome::Success { amount_out, gas: gas_res.ok().flatten() }
            }
            reverted @ SwapOutcome::Reverted { .. } => reverted,
        })
    }

    /// Run the swap and a balance measurement in the same simulated block via `eth_simulateV1`,
    /// returning the exact amount of `token_out` received and the gas consumed by the swap call.
    ///
    /// `eth_simulateV1` can drop the revert data of a reverted swap. The swap is then replayed
    /// under the call tracer to read the error off the frame that reverted.
    async fn try_simulate(&self, swap: &SwapCall<'_>, block: B256) -> anyhow::Result<SwapOutcome> {
        let mut overrides = self.build_overrides(swap).await?;
        let (swap_call, balance_call) = self.build_simulate_calls(swap, &mut overrides);
        let approve_call = self
            .is_native_balance_token(swap.token_in)
            .then(|| self.build_approve_call(swap));
        let approves = usize::from(approve_call.is_some());
        let calls: Vec<serde_json::Value> = approve_call
            .into_iter()
            .chain([swap_call, balance_call])
            .collect();

        let state_overrides =
            serde_json::to_value(&overrides).context("serialise state overrides")?;
        let params = serde_json::json!([
            {
                "blockStateCalls": [{
                    "stateOverrides": state_overrides,
                    "calls": calls
                }]
            },
            {
                "blockHash": format!("0x{}", hex::encode(block.as_slice())),
                "requireCanonical": false,
            }
        ]);

        let result: serde_json::Value = self
            .provider
            .raw_request("eth_simulateV1".into(), params)
            .await
            .context("eth_simulateV1")?;

        let (return_data, message) = match parse_simulate_result(&result, approves)? {
            SimulatedSwap::Success { amount_out, gas } => {
                return Ok(SwapOutcome::Success { amount_out, gas: Some(gas) })
            }
            SimulatedSwap::Reverted { return_data, message } => (return_data, message),
        };
        if let Some(reason) = decode_error(&return_data) {
            return Ok(SwapOutcome::Reverted { reason });
        }
        // A trace runs the swap alone, without the approve call, so it would blame the allowance.
        if approves > 0 {
            return Ok(SwapOutcome::Reverted { reason: message });
        }
        let traced = self
            .traced_revert_reason(self.swap_request(swap), overrides, block)
            .await;
        Ok(SwapOutcome::Reverted { reason: traced.unwrap_or(message) })
    }

    /// Replays a reverted swap under the call tracer, with the same overrides at the same block,
    /// and reads the reason off the frame that reverted.
    ///
    /// Returns `None` when the node serves no `debug_traceCall` or the trace names no reason.
    async fn traced_revert_reason(
        &self,
        swap: TransactionRequest,
        overrides: StateOverride,
        block: B256,
    ) -> Option<String> {
        let options = GethDebugTracingCallOptions {
            tracing_options: GethDebugTracingOptions::default()
                .with_tracer(GethDebugTracerType::BuiltInTracer(
                    GethDebugBuiltInTracerType::CallTracer,
                ))
                .with_call_config(CallConfig { only_top_call: Some(false), with_log: Some(false) }),
            state_overrides: Some(overrides),
            ..Default::default()
        };
        match self
            .provider
            .raw_request::<_, CallFrame>("debug_traceCall".into(), (swap, pinned(block), options))
            .await
        {
            Ok(frame) => reason_from_frame(&frame),
            Err(e) => {
                warn!("debug_traceCall at block {block} failed: {e}");
                None
            }
        }
    }

    /// Build the two `eth_simulateV1` calls: the swap, then a balance read of `token_out`.
    ///
    /// For ERC-20 output the second call is `balanceOf(sender)` on the token. For native ETH
    /// output (`token_out == ZERO`) the sender's ETH balance is zeroed and a 10-byte
    /// balance-reader contract is injected at a reserved address; `gasPrice=0` on the swap
    /// keeps the post-swap balance equal to the ETH received. A native balance token's output
    /// also zeroes the sender's native balance and sets `gasPrice=0`, and reads `balanceOf`.
    fn build_simulate_calls(
        &self,
        swap: &SwapCall<'_>,
        overrides: &mut StateOverride,
    ) -> (serde_json::Value, serde_json::Value) {
        let SwapCall { token_out, router, calldata, value, .. } = swap;
        if self.is_native_balance_token(*token_out) {
            overrides.insert(
                self.sender,
                AccountOverride { balance: Some(U256::ZERO), ..Default::default() },
            );
            return (
                serde_json::json!({
                    "from":     format!("0x{}", hex::encode(self.sender)),
                    "to":       format!("0x{}", hex::encode(router)),
                    "data":     format!("0x{}", hex::encode(calldata)),
                    "value":    format!("0x{:x}", value),
                    "gasPrice": "0x0",
                }),
                self.build_balance_of_call(*token_out),
            );
        }
        if *token_out == Address::ZERO {
            overrides.insert(
                self.sender,
                AccountOverride { balance: Some(U256::ZERO), ..Default::default() },
            );
            overrides.insert(
                ETH_BALANCE_READER_ADDR,
                AccountOverride {
                    code: Some(alloy::primitives::Bytes(Bytes::from_static(
                        &ETH_BALANCE_READER_CODE,
                    ))),
                    ..Default::default()
                },
            );
            (
                serde_json::json!({
                    "from":     format!("0x{}", hex::encode(self.sender)),
                    "to":       format!("0x{}", hex::encode(router)),
                    "data":     format!("0x{}", hex::encode(calldata)),
                    "value":    format!("0x{:x}", value),
                    "gasPrice": "0x0",
                }),
                serde_json::json!({
                    "from":     format!("0x{}", hex::encode(self.sender)),
                    "to":       format!("0x{}", hex::encode(ETH_BALANCE_READER_ADDR)),
                    "gasPrice": "0x0",
                }),
            )
        } else {
            (
                serde_json::json!({
                    "from":  format!("0x{}", hex::encode(self.sender)),
                    "to":    format!("0x{}", hex::encode(router)),
                    "data":  format!("0x{}", hex::encode(calldata)),
                    "value": format!("0x{:x}", value),
                }),
                self.build_balance_of_call(*token_out),
            )
        }
    }

    /// `token.balanceOf(sender)`, from the zero address.
    fn build_balance_of_call(&self, token: Address) -> serde_json::Value {
        let mut bal_data = vec![0x70u8, 0xa0, 0x82, 0x31];
        bal_data.extend_from_slice(&[0u8; 12]);
        bal_data.extend_from_slice(self.sender.as_slice());
        serde_json::json!({
            "from": "0x0000000000000000000000000000000000000000",
            "to":   format!("0x{}", hex::encode(token)),
            "data": format!("0x{}", hex::encode(&bal_data)),
        })
    }

    /// The sender's `token_in.approve(spender, MAX >> 1)`, run before the swap.
    fn build_approve_call(&self, swap: &SwapCall<'_>) -> serde_json::Value {
        let mut data = vec![0x09u8, 0x5e, 0xa7, 0xb3];
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(swap.spender.as_slice());
        data.extend_from_slice(B256::from(U256::MAX >> 1).as_slice());
        serde_json::json!({
            "from":     format!("0x{}", hex::encode(self.sender)),
            "to":       format!("0x{}", hex::encode(swap.token_in)),
            "data":     format!("0x{}", hex::encode(&data)),
            "gasPrice": "0x0",
        })
    }

    /// Fallback when `eth_simulateV1` is unavailable: plain `eth_call` with heuristic
    /// return-data decoding.
    async fn try_eth_call(&self, swap: &SwapCall<'_>, block: B256) -> anyhow::Result<SwapOutcome> {
        anyhow::ensure!(
            !self.is_native_balance_token(swap.token_in),
            "selling {:#x} needs eth_simulateV1 to approve it, and the node did not run it",
            swap.token_in
        );
        let overrides = self.build_overrides(swap).await?;

        let result = self
            .provider
            .call(self.swap_request(swap))
            .overrides(overrides)
            .block(pinned(block))
            .await;

        let return_data = match result {
            Ok(d) => d,
            Err(e) if is_revert(&e) => {
                warn!("eth_call at block {block} reverted: {e}");
                return Ok(SwapOutcome::Reverted { reason: revert_reason(&e) });
            }
            Err(e) => return Err(e).context(format!("eth_call at block {block}")),
        };

        let amount_out = parse_return_amount(&return_data).with_context(|| {
            format!("eth_call at block {block} returned {} bytes, not an amount", return_data.len())
        })?;
        Ok(SwapOutcome::Success { amount_out, gas: None })
    }

    /// The swap transaction from the runner's sender, with a zero gas price so that the sender's
    /// native balance is not charged for gas.
    fn swap_request(&self, swap: &SwapCall<'_>) -> TransactionRequest {
        let value_u128: u128 = swap
            .value
            .to_string()
            .parse()
            .unwrap_or(0);
        TransactionRequest {
            from: Some(self.sender),
            to: Some(TxKind::Call(swap.router)),
            input: alloy::primitives::Bytes::copy_from_slice(swap.calldata).into(),
            value: Some(U256::from(value_u128)),
            max_fee_per_gas: Some(0),
            max_priority_fee_per_gas: Some(0),
            ..Default::default()
        }
    }

    /// Gas estimate via `eth_estimateGas` with state overrides.
    ///
    /// Used as a fallback when `eth_simulateV1` is unavailable. Returns the estimated gas
    /// units including the 21 000-unit base transaction cost.
    async fn try_estimate_gas(
        &self,
        swap: &SwapCall<'_>,
        block: B256,
    ) -> anyhow::Result<Option<u64>> {
        let overrides = self.build_overrides(swap).await?;
        let state_overrides =
            serde_json::to_value(&overrides).context("serialise state overrides")?;

        let params = serde_json::json!([
            {
                "from": format!("0x{}", hex::encode(self.sender)),
                "to":   format!("0x{}", hex::encode(swap.router)),
                "data": format!("0x{}", hex::encode(swap.calldata)),
                "value": format!("0x{:x}", swap.value),
            },
            {
                "blockHash": format!("0x{}", hex::encode(block.as_slice())),
                "requireCanonical": false,
            },
            state_overrides,
        ]);

        let result: serde_json::Value = match self
            .provider
            .raw_request("eth_estimateGas".into(), params)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("eth_estimateGas at block {block} failed: {e}");
                return Ok(None);
            }
        };

        Ok(result
            .as_str()
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .or_else(|| result.as_u64()))
    }

    /// Build a `StateOverride` that injects sufficient balance and allowance for the sender
    /// to spend `token_in` via `swap.spender`, plus `swap.extra_state`. Probes storage slots on
    /// first use and caches.
    ///
    /// Always injects a large ETH balance for the sender so that `eth_simulateV1` can charge
    /// gas without rejecting the transaction. For ERC-20 tokens the ERC-20 balance and
    /// allowance storage slots are also overridden. For native ETH (zero address) and a native
    /// balance token only the ETH balance override is needed.
    async fn build_overrides(&self, swap: &SwapCall<'_>) -> anyhow::Result<StateOverride> {
        let SwapCall { token_in, spender, .. } = *swap;
        // Use MAX >> 1: avoids triggering tokens that pack metadata into bit 255 (e.g. USDC).
        let max_val = B256::from(U256::MAX >> 1);
        let eth_balance = U256::MAX >> 1;

        let mut overrides = StateOverride::default();

        // Give sender unlimited ETH — required for eth_simulateV1 to charge gas without
        // failing. Harmless for eth_call which does not deduct gas from the sender balance.
        overrides.insert(
            self.sender,
            AccountOverride { balance: Some(eth_balance), ..Default::default() },
        );

        if token_in != Address::ZERO && !self.is_native_balance_token(token_in) {
            let layout = self.layout(token_in, spender).await?;

            let mut state_diff = B256HashMap::default();
            state_diff.insert(layout.balance_slot(self.sender), max_val);
            state_diff.insert(layout.allowance_slot(self.sender, spender), max_val);

            // A proxy keeps its balances somewhere other than the address the swap calls, so the
            // write goes to the contract discovery named rather than to the token.
            overrides.insert(
                layout.storage_contract(),
                AccountOverride { state_diff: Some(state_diff), ..Default::default() },
            );
        }

        for (address, account) in &swap.extra_state {
            anyhow::ensure!(
                !overrides.contains_key(address),
                "the extra state overrides {address}, which holds the sender's ETH or token_in \
                 balance; drop {address} from the extra state"
            );
            overrides.insert(*address, account.clone());
        }

        Ok(overrides)
    }

    async fn layout(&self, token: Address, spender: Address) -> anyhow::Result<TokenLayout> {
        {
            let cache = self.layouts.lock().await;
            if let Some(&layout) = cache.get(&(token, spender)) {
                return Ok(layout);
            }
        }
        let layout = discover_layout(&self.provider, token, self.sender, spender).await?;
        self.layouts
            .lock()
            .await
            .insert((token, spender), layout);
        Ok(layout)
    }
}

/// The swap call of an `eth_simulateV1` result.
#[derive(Debug, PartialEq, Eq)]
enum SimulatedSwap {
    Success {
        amount_out: BigUint,
        gas: u64,
    },
    /// `return_data` is the revert data, which some nodes leave empty.
    Reverted {
        return_data: Vec<u8>,
        message: String,
    },
}

/// The block id that pins a call to `block`, whether or not the block is still canonical.
fn pinned(block: B256) -> BlockId {
    BlockId::Hash(RpcBlockHash { block_hash: block, require_canonical: Some(false) })
}

/// Parse an `eth_simulateV1` result whose first `approves` calls are approvals: check that they
/// succeeded and whether the swap succeeded, then read the balance call's `returnData` (a
/// uint256). Fails when an approval failed, or the result holds no calls or no balance word.
fn parse_simulate_result(
    result: &serde_json::Value,
    approves: usize,
) -> anyhow::Result<SimulatedSwap> {
    let all_calls = result
        .get(0)
        .and_then(|b| b.get("calls"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: missing calls array in result"))?;
    let (approve_calls, calls) = all_calls.split_at(approves.min(all_calls.len()));
    for approve_call in approve_calls {
        anyhow::ensure!(
            is_success(approve_call),
            "eth_simulateV1: the approve call before the swap failed: {approve_call}"
        );
    }

    let swap_call = calls
        .first()
        .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: calls array is empty"))?;
    if !is_success(swap_call) {
        let return_data = hex_field(swap_call, "returnData")?;
        let message = swap_call
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("execution reverted")
            .to_string();
        warn!("eth_simulateV1: swap call reverted: {message}");
        return Ok(SimulatedSwap::Reverted { return_data, message });
    }

    // gasUsed may be a hex string ("0x...") or a JSON integer depending on the RPC provider.
    let gas_used = swap_call
        .get("gasUsed")
        .and_then(|g| {
            g.as_str()
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .or_else(|| g.as_u64())
        })
        .unwrap_or(0);

    let balance_call = calls
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: missing balance call in result"))?;
    let amount_bytes = hex_field(balance_call, "returnData")?;
    let Some(amount_word) = amount_bytes.get(..32) else {
        anyhow::bail!("eth_simulateV1: balance call returned {} bytes", amount_bytes.len());
    };
    Ok(SimulatedSwap::Success { amount_out: BigUint::from_bytes_be(amount_word), gas: gas_used })
}

/// Whether an `eth_simulateV1` call result has status 1.
fn is_success(call: &serde_json::Value) -> bool {
    call.get("status")
        .map(|s| s == "0x1" || s.as_u64() == Some(1))
        .unwrap_or(false)
}

/// Decodes the hex string `call` holds under `field`. A missing field reads as empty.
fn hex_field(call: &serde_json::Value, field: &str) -> anyhow::Result<Vec<u8>> {
    let hex_str = call
        .get(field)
        .and_then(|d| d.as_str())
        .unwrap_or("0x");
    hex::decode(hex_str.trim_start_matches("0x")).with_context(|| format!("decode {field}"))
}

/// Returns `true` when `e` indicates that the called JSON-RPC method does not exist on this RPC.
///
/// Prefers the standard error code `-32601` (method not found). Falls back to narrow
/// phrase matching for providers that return non-standard error shapes.
pub fn is_method_not_found(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(rpc) = cause.downcast_ref::<RpcError<TransportErrorKind>>() {
            return matches!(rpc, RpcError::ErrorResp(p) if p.code == -32601);
        }
    }
    // Fallback for providers that surface the error as plain text.
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("method not found") || msg.contains("does not exist")
}

/// Returns `true` when the node ran the call and the call reverted, as opposed to a node that
/// failed to run it.
///
/// Geth-style nodes answer a revert with error code `3`. Nodes that use another code still say
/// "revert" in the message.
fn is_revert(e: &RpcError<TransportErrorKind>) -> bool {
    let RpcError::ErrorResp(payload) = e else {
        return false;
    };
    payload.code == 3 ||
        payload
            .message
            .to_ascii_lowercase()
            .contains("revert")
}

/// Names the error of an `eth_call` that reverted, from its revert data when the node returned
/// any, and from the node's message otherwise.
fn revert_reason(e: &RpcError<TransportErrorKind>) -> String {
    let Some(payload) = e.as_error_resp() else {
        return e.to_string();
    };
    payload
        .as_revert_data()
        .and_then(|data| decode_error(&data))
        .unwrap_or_else(|| payload.message.to_string())
}

/// Extract `uint256 amountOut` from raw `eth_call` return bytes.
///
/// Most routers return a bare `uint256` as the first word. The 0x AllowanceHolder wraps
/// the inner swap result in ABI-encoded `bytes memory`, so the layout is:
///   word 0: offset = 0x20 (32)
///   word 1: inner length
///   word 2: inner uint256 amountOut
///
/// When the first word equals 32 and the data is long enough, we skip the ABI wrapper.
pub fn parse_return_amount(data: &[u8]) -> Option<BigUint> {
    if data.len() < 32 {
        return None;
    }
    // Detect ABI `bytes memory` wrapper: first word is exactly 0x0000…0020.
    if data.len() >= 96 && data[..31] == [0u8; 31] && data[31] == 0x20 {
        let inner = BigUint::from_bytes_be(&data[64..96]);
        if inner > BigUint::ZERO {
            return Some(inner);
        }
    }
    Some(BigUint::from_bytes_be(&data[..32]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: Address = Address::repeat_byte(0x11);
    const ROUTER: Address = Address::repeat_byte(0x22);

    /// A runner on a port nothing listens on: a native `token_in` needs no layout probe.
    fn offline_runner() -> EthCallRunner {
        let provider = RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        EthCallRunner::with_sender(provider, SENDER)
    }

    fn native_swap_with_extra_state(address: Address) -> (StateOverride, BigUint) {
        let account = AccountOverride { balance: Some(U256::from(7)), ..Default::default() };
        (StateOverride::from_iter([(address, account)]), BigUint::ZERO)
    }

    #[tokio::test]
    async fn test_build_overrides_extra_state() {
        let (extra_state, value) = native_swap_with_extra_state(ROUTER);
        let swap = SwapCall {
            extra_state,
            ..SwapCall::new(Address::ZERO, Address::ZERO, ROUTER, &[], &value)
        };

        let overrides = offline_runner()
            .build_overrides(&swap)
            .await
            .unwrap();

        assert_eq!(overrides[&ROUTER].balance, Some(U256::from(7)));
        assert!(overrides.contains_key(&SENDER));
    }

    #[tokio::test]
    async fn test_build_overrides_extra_state_on_sender() {
        let (extra_state, value) = native_swap_with_extra_state(SENDER);
        let swap = SwapCall {
            extra_state,
            ..SwapCall::new(Address::ZERO, Address::ZERO, ROUTER, &[], &value)
        };

        let error = offline_runner()
            .build_overrides(&swap)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&SENDER.to_string()),
            "{error}"
        );
    }

    const NATIVE_BALANCE_TOKEN: Address = Address::repeat_byte(0x36);
    const OTHER_TOKEN: Address = Address::repeat_byte(0x44);

    fn native_balance_runner() -> EthCallRunner {
        offline_runner().with_native_balance_token(NATIVE_BALANCE_TOKEN)
    }

    /// The runner is offline, so a layout probe of the token would fail the call.
    #[tokio::test]
    async fn test_build_overrides_native_balance_token_in() {
        let value = BigUint::ZERO;
        let swap = SwapCall::new(NATIVE_BALANCE_TOKEN, OTHER_TOKEN, ROUTER, &[], &value);

        let overrides = native_balance_runner()
            .build_overrides(&swap)
            .await
            .unwrap();

        assert_eq!(overrides.keys().collect::<Vec<_>>(), [&SENDER]);
        assert_eq!(overrides[&SENDER].balance, Some(U256::MAX >> 1));
    }

    #[test]
    fn test_build_simulate_calls_native_balance_token_out() {
        let value = BigUint::ZERO;
        let swap = SwapCall::new(OTHER_TOKEN, NATIVE_BALANCE_TOKEN, ROUTER, &[], &value);
        let mut overrides = StateOverride::default();

        let (swap_call, balance_call) =
            native_balance_runner().build_simulate_calls(&swap, &mut overrides);

        assert_eq!(overrides[&SENDER].balance, Some(U256::ZERO));
        assert_eq!(swap_call["gasPrice"], "0x0");
        assert_eq!(balance_call["to"], format!("0x{}", hex::encode(NATIVE_BALANCE_TOKEN)));
    }

    #[test]
    fn test_build_approve_call() {
        let value = BigUint::ZERO;
        let swap = SwapCall {
            spender: Address::repeat_byte(0x55),
            ..SwapCall::new(NATIVE_BALANCE_TOKEN, OTHER_TOKEN, ROUTER, &[], &value)
        };

        let approve_call = native_balance_runner().build_approve_call(&swap);

        let expected_data = format!(
            "0x095ea7b3{}{}{}",
            "00".repeat(12),
            "55".repeat(20),
            hex::encode(B256::from(U256::MAX >> 1))
        );
        assert_eq!(approve_call["to"], format!("0x{}", hex::encode(NATIVE_BALANCE_TOKEN)));
        assert_eq!(approve_call["from"], format!("0x{}", hex::encode(SENDER)));
        assert_eq!(approve_call["data"], expected_data);
    }

    #[tokio::test]
    async fn test_eth_call_selling_native_balance_token() {
        let value = BigUint::ZERO;
        let swap = SwapCall::new(NATIVE_BALANCE_TOKEN, OTHER_TOKEN, ROUTER, &[], &value);

        let error = native_balance_runner()
            .try_eth_call(&swap, B256::ZERO)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("needs eth_simulateV1"),
            "{error}"
        );
    }

    #[test]
    fn test_parse_simulate_result_after_approve() {
        let amount = format!("0x{}", hex::encode(word(100)));
        let result = serde_json::json!([{ "calls": [
            { "status": "0x1" },
            { "status": "0x1", "gasUsed": "0x5208" },
            { "returnData": amount }
        ] }]);

        assert_eq!(
            parse_simulate_result(&result, 1).unwrap(),
            SimulatedSwap::Success { amount_out: BigUint::from(100u32), gas: 0x5208 }
        );
    }

    #[test]
    fn test_parse_simulate_result_failed_approve() {
        let amount = format!("0x{}", hex::encode(word(100)));
        let result = serde_json::json!([{ "calls": [
            { "status": "0x0" },
            { "status": "0x1", "gasUsed": "0x5208" },
            { "returnData": amount }
        ] }]);

        assert!(parse_simulate_result(&result, 1).is_err());
    }

    fn word(value: u8) -> Vec<u8> {
        let mut w = vec![0u8; 32];
        w[31] = value;
        w
    }

    #[test]
    fn parse_return_amount_short_data_is_none() {
        assert_eq!(parse_return_amount(&[0u8; 31]), None);
    }

    #[test]
    fn parse_return_amount_bare_uint256() {
        assert_eq!(parse_return_amount(&word(42)), Some(BigUint::from(42u32)));
    }

    #[test]
    fn parse_return_amount_unwraps_abi_bytes() {
        // offset (0x20) + length + inner amount → returns the inner word.
        let mut data = word(0x20);
        data.extend(word(32)); // inner length
        data.extend(word(99)); // inner amount
        assert_eq!(parse_return_amount(&data), Some(BigUint::from(99u32)));
    }

    #[test]
    fn parse_return_amount_offset_word_with_zero_inner_falls_through() {
        // First word is 0x20 but the inner amount is zero → fall through to the bare-word read,
        // which returns 0x20 (32) as the literal first word value.
        let mut data = word(0x20);
        data.extend(word(0));
        data.extend(word(0));
        assert_eq!(parse_return_amount(&data), Some(BigUint::from(32u32)));
    }

    #[test]
    fn test_parse_simulate_result_reverted_swap() {
        let result = serde_json::json!([{ "calls": [
            {
                "status": "0x0",
                "returnData": "0xdeadbeef",
                "error": { "code": 3, "message": "execution reverted" }
            },
            { "returnData": "0x" }
        ] }]);
        assert_eq!(
            parse_simulate_result(&result, 0).unwrap(),
            SimulatedSwap::Reverted {
                return_data: vec![0xde, 0xad, 0xbe, 0xef],
                message: "execution reverted".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_simulate_result_short_balance_word() {
        let result =
            serde_json::json!([{ "calls": [{ "status": "0x1" }, { "returnData": "0x" }] }]);
        assert!(parse_simulate_result(&result, 0).is_err());
    }

    #[test]
    fn parse_simulate_result_reads_amount_and_gas() {
        let amount = format!("0x{}", hex::encode(word(100)));
        let result = serde_json::json!([{
            "calls": [
                { "status": "0x1", "gasUsed": "0x5208" },
                { "returnData": amount }
            ]
        }]);
        assert_eq!(
            parse_simulate_result(&result, 0).unwrap(),
            SimulatedSwap::Success { amount_out: BigUint::from(100u32), gas: 0x5208 }
        );
    }

    fn error_response(code: i64, message: &'static str) -> RpcError<TransportErrorKind> {
        RpcError::ErrorResp(alloy::rpc::json_rpc::ErrorPayload {
            code,
            message: message.into(),
            data: None,
        })
    }

    #[test]
    fn test_revert_reason() {
        // TychoRouter__AmountOutZero(), selector 0x6fa417d5.
        let payload = alloy::rpc::json_rpc::ErrorPayload {
            code: 3,
            message: "execution reverted".into(),
            data: Some(serde_json::value::to_raw_value("0x6fa417d5").unwrap()),
        };
        let reason = revert_reason(&RpcError::ErrorResp(payload));
        assert!(reason.contains("TychoRouter__AmountOutZero"), "{reason}");

        assert_eq!(
            revert_reason(&error_response(-32000, "execution reverted: STF")),
            "execution reverted: STF"
        );
    }

    #[test]
    fn test_is_revert() {
        assert!(is_revert(&error_response(3, "execution reverted")));
        assert!(is_revert(&error_response(-32000, "execution reverted")));
        assert!(!is_revert(&error_response(-32005, "rate limit exceeded")));
        assert!(!is_revert(&error_response(-32000, "missing trie node")));
        assert!(!is_revert(&RpcError::Transport(TransportErrorKind::BackendGone)));
    }

    #[test]
    fn parse_simulate_result_missing_calls_errors() {
        let result = serde_json::json!([{}]);
        assert!(parse_simulate_result(&result, 0).is_err());
    }
}
