//! On-chain re-execution of encoded swap calldata to measure real output and gas.
//!
//! [`EthCallRunner`] re-runs a quote's calldata at a pinned block using state overrides to inject
//! the sender's token balance and allowance. It prefers `eth_simulateV1` (a balance-delta
//! measurement that is independent of router return-data conventions) and falls back to a plain
//! `eth_call` with return-data heuristics when the RPC does not support `eth_simulateV1`.

use std::{
    collections::HashMap,
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
        BlockId, RpcBlockHash, TransactionRequest,
    },
    transports::{RpcError, TransportErrorKind},
};
use anyhow::Context;
use bytes::Bytes;
use num_bigint::BigUint;
use tokio::sync::Mutex;
use tracing::warn;

/// Sentinel address for the inline ETH-balance reader injected into `eth_simulateV1` state.
/// Chosen to be obviously synthetic and not a real mainnet contract.
const ETH_BALANCE_READER_ADDR: Address = Address(alloy::primitives::FixedBytes([
    0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0,
    0xf0, 0xf0, 0xf0, 0xf0,
]));

/// EVM bytecode: CALLER BALANCE PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN.
/// Returns the ETH balance of the calling address as a uint256.
const ETH_BALANCE_READER_CODE: [u8; 10] =
    [0x33, 0x31, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

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
    /// Per-token balance storage slot (probed once, cached for the run).
    balance_slots: Arc<Mutex<HashMap<Address, B256>>>,
    /// Per-(token, spender) allowance storage slot (probed once, cached for the run).
    allowance_slots: Arc<Mutex<HashMap<(Address, Address), B256>>>,
    /// Set to `false` after the first `eth_simulateV1` "method not found" error so subsequent
    /// calls skip straight to the `eth_call` fallback without retrying.
    simulate_supported: Arc<AtomicBool>,
}

impl EthCallRunner {
    pub fn new(provider: RootProvider<Ethereum>) -> Self {
        // Generate a fresh random address so it has no pre-existing token balances.
        let mut bytes = [0u8; 20];
        fastrand::fill(&mut bytes);
        let sender = Address::from(bytes);
        Self {
            provider: Arc::new(provider),
            sender,
            balance_slots: Arc::new(Mutex::new(HashMap::new())),
            allowance_slots: Arc::new(Mutex::new(HashMap::new())),
            simulate_supported: Arc::new(AtomicBool::new(true)),
        }
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

    /// Execute `calldata` against `router` at `block`, returning the actual `token_out`
    /// amount received by the runner's sender and, when `eth_simulateV1` is used, the gas
    /// consumed.
    ///
    /// Tries `eth_simulateV1` first (balance-delta approach). Falls back to `eth_call` +
    /// return-data parsing when the RPC does not support `eth_simulateV1`. Gas is `None`
    /// on the fallback path.
    pub async fn execute(
        &self,
        token_in: Address,
        token_out: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<(BigUint, Option<u64>)>> {
        if self
            .simulate_supported
            .load(Ordering::Relaxed)
        {
            match self
                .try_simulate(token_in, token_out, router, calldata, value, block)
                .await
            {
                Ok(result) => return Ok(result.map(|(amount, gas)| (amount, Some(gas)))),
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
        let (amount_res, gas_res) = tokio::join!(
            self.try_eth_call(token_in, router, calldata, value, block),
            self.try_estimate_gas(token_in, router, calldata, value, block),
        );
        let gas = gas_res.ok().flatten();
        amount_res.map(|opt| opt.map(|amount| (amount, gas)))
    }

    /// Run the swap and a balance measurement in the same simulated block via `eth_simulateV1`,
    /// returning the exact amount of `token_out` received and the gas consumed by the swap call.
    async fn try_simulate(
        &self,
        token_in: Address,
        token_out: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<(BigUint, u64)>> {
        let mut overrides = self
            .build_overrides(token_in, router)
            .await?;
        let (swap_call, balance_call) =
            self.build_simulate_calls(token_out, router, calldata, value, &mut overrides);

        let state_overrides =
            serde_json::to_value(&overrides).context("serialise state overrides")?;
        let params = serde_json::json!([
            {
                "blockStateCalls": [{
                    "stateOverrides": state_overrides,
                    "calls": [swap_call, balance_call]
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

        parse_simulate_result(&result)
    }

    /// Build the two `eth_simulateV1` calls: the swap, then a balance read of `token_out`.
    ///
    /// For ERC-20 output the second call is `balanceOf(sender)` on the token. For native ETH
    /// output (`token_out == ZERO`) the sender's ETH balance is zeroed and a 10-byte
    /// balance-reader contract is injected at a reserved address; `gasPrice=0` on the swap
    /// keeps the post-swap balance equal to the ETH received.
    fn build_simulate_calls(
        &self,
        token_out: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        overrides: &mut StateOverride,
    ) -> (serde_json::Value, serde_json::Value) {
        if token_out == Address::ZERO {
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
            let mut bal_data = vec![0x70u8, 0xa0, 0x82, 0x31];
            bal_data.extend_from_slice(&[0u8; 12]);
            bal_data.extend_from_slice(self.sender.as_slice());
            (
                serde_json::json!({
                    "from":  format!("0x{}", hex::encode(self.sender)),
                    "to":    format!("0x{}", hex::encode(router)),
                    "data":  format!("0x{}", hex::encode(calldata)),
                    "value": format!("0x{:x}", value),
                }),
                serde_json::json!({
                    "from": "0x0000000000000000000000000000000000000000",
                    "to":   format!("0x{}", hex::encode(token_out)),
                    "data": format!("0x{}", hex::encode(&bal_data)),
                }),
            )
        }
    }

    /// Fallback when `eth_simulateV1` is unavailable: plain `eth_call` with heuristic
    /// return-data decoding.
    async fn try_eth_call(
        &self,
        token_in: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<BigUint>> {
        let overrides = self
            .build_overrides(token_in, router)
            .await?;

        let value_u128: u128 = value.to_string().parse().unwrap_or(0);
        let tx = TransactionRequest {
            from: Some(self.sender),
            to: Some(TxKind::Call(router)),
            input: alloy::primitives::Bytes::copy_from_slice(calldata).into(),
            value: Some(U256::from(value_u128)),
            max_fee_per_gas: Some(0),
            max_priority_fee_per_gas: Some(0),
            ..Default::default()
        };

        let result = self
            .provider
            .call(tx)
            .overrides(overrides)
            .block(BlockId::Hash(RpcBlockHash {
                block_hash: block,
                require_canonical: Some(false),
            }))
            .await;

        let return_data = match result {
            Ok(d) => d,
            Err(e) => {
                warn!("eth_call at block {block} reverted: {e}");
                return Ok(None);
            }
        };

        Ok(parse_return_amount(&return_data))
    }

    /// Gas estimate via `eth_estimateGas` with state overrides.
    ///
    /// Used as a fallback when `eth_simulateV1` is unavailable. Returns the estimated gas
    /// units including the 21 000-unit base transaction cost.
    async fn try_estimate_gas(
        &self,
        token_in: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<u64>> {
        let overrides = self
            .build_overrides(token_in, router)
            .await?;
        let state_overrides =
            serde_json::to_value(&overrides).context("serialise state overrides")?;

        let params = serde_json::json!([
            {
                "from": format!("0x{}", hex::encode(self.sender)),
                "to":   format!("0x{}", hex::encode(router)),
                "data": format!("0x{}", hex::encode(calldata)),
                "value": format!("0x{:x}", value),
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
    /// to spend `token_in` via `router`. Probes storage slots on first use and caches.
    ///
    /// Always injects a large ETH balance for the sender so that `eth_simulateV1` can charge
    /// gas without rejecting the transaction. For ERC-20 tokens the ERC-20 balance and
    /// allowance storage slots are also overridden. For native ETH (zero address) only the
    /// ETH balance override is needed.
    async fn build_overrides(
        &self,
        token_in: Address,
        router: Address,
    ) -> anyhow::Result<StateOverride> {
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

        if token_in != Address::ZERO {
            let balance_slot = self.balance_slot(token_in).await?;
            let allowance_slot = self
                .allowance_slot(token_in, router)
                .await?;

            let mut state_diff = B256HashMap::default();
            state_diff.insert(balance_slot, max_val);
            state_diff.insert(allowance_slot, max_val);

            overrides.insert(
                token_in,
                AccountOverride { state_diff: Some(state_diff), ..Default::default() },
            );
        }

        Ok(overrides)
    }

    async fn balance_slot(&self, token: Address) -> anyhow::Result<B256> {
        {
            let cache = self.balance_slots.lock().await;
            if let Some(&slot) = cache.get(&token) {
                return Ok(slot);
            }
        }
        let slot = erc20_overrides::find_balance_slot(&self.provider, token, self.sender).await?;
        self.balance_slots
            .lock()
            .await
            .insert(token, slot);
        Ok(slot)
    }

    async fn allowance_slot(&self, token: Address, spender: Address) -> anyhow::Result<B256> {
        {
            let cache = self.allowance_slots.lock().await;
            if let Some(&slot) = cache.get(&(token, spender)) {
                return Ok(slot);
            }
        }
        let slot =
            erc20_overrides::find_allowance_slot(&self.provider, token, self.sender, spender)
                .await?;
        self.allowance_slots
            .lock()
            .await
            .insert((token, spender), slot);
        Ok(slot)
    }
}

/// Parse an `eth_simulateV1` result: confirm the swap succeeded, then read the balance call's
/// `returnData` (a uint256). Returns `(amount_out, gas_used)` or `None` when the swap reverted
/// or the balance word is missing.
fn parse_simulate_result(result: &serde_json::Value) -> anyhow::Result<Option<(BigUint, u64)>> {
    let calls = result
        .get(0)
        .and_then(|b| b.get("calls"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: missing calls array in result"))?;

    let swap_call = calls
        .first()
        .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: calls array is empty"))?;
    let swap_ok = swap_call
        .get("status")
        .map(|s| s == "0x1" || s.as_u64() == Some(1))
        .unwrap_or(false);
    if !swap_ok {
        warn!("eth_simulateV1: swap call reverted");
        return Ok(None);
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

    let amount_hex = calls
        .get(1)
        .and_then(|c| c.get("returnData"))
        .and_then(|d| d.as_str())
        .unwrap_or("0x");
    let amount_bytes =
        hex::decode(amount_hex.trim_start_matches("0x")).context("decode amount return")?;

    if amount_bytes.len() >= 32 {
        Ok(Some((BigUint::from_bytes_be(&amount_bytes[..32]), gas_used)))
    } else {
        Ok(None)
    }
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
    fn parse_simulate_result_reverted_swap_is_none() {
        let result =
            serde_json::json!([{ "calls": [{ "status": "0x0" }, { "returnData": "0x" }] }]);
        assert_eq!(parse_simulate_result(&result).unwrap(), None);
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
        let (amount, gas) = parse_simulate_result(&result)
            .unwrap()
            .unwrap();
        assert_eq!(amount, BigUint::from(100u32));
        assert_eq!(gas, 0x5208);
    }

    #[test]
    fn parse_simulate_result_missing_calls_errors() {
        let result = serde_json::json!([{}]);
        assert!(parse_simulate_result(&result).is_err());
    }
}
