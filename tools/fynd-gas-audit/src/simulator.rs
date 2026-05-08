//! On-chain simulator: routes gas estimation through `FyndClient::execute_swap`
//! in dry-run mode (with `max_fee_per_gas=1` so `gas_cost == gas_used` and we
//! recover raw gas units from the returned `SettledOrder`).
//!
//! Storage-slot detection (balance + allowance) stays local to this crate — we
//! still need an alloy provider of our own to brute-force the slots via
//! `eth_call` with overrides.
//!
//! Slot-detection helpers mirror `tools/fynd-swap-cli/src/erc20.rs` (the swap CLI
//! is a bin crate with no lib target, so we can't depend on it).

use alloy::{
    network::Ethereum,
    primitives::{keccak256, Address, Bytes as AlloyBytes, Signature, TxKind, B256, U256},
    providers::{Provider, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionRequest,
    },
    sol,
    sol_types::SolCall,
};
use anyhow::bail;
use bytes::Bytes;
use fynd_client::{
    ExecutionOptions, FyndClient, Quote, SignedSwap, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

const MAX_PROBE_SLOT: u64 = 20;
const PROBE_SENTINEL: U256 = U256::from_limbs([0xdead_beef, 0, 0, 0]);

/// 1000 gwei — well above any realistic mainnet base fee, so nodes accept the
/// simulated tx. Used to recover `gas_used` from `SettledOrder::gas_cost()` via
/// exact division.
const MAX_FEE_PER_GAS_WEI: u128 = 1_000_000_000_000;

// U256::MAX >> 1 avoids clobbering tokens that pack metadata into bit 255
// (e.g. USDC's blacklist flag).
fn huge_balance() -> B256 {
    B256::from(U256::MAX >> 1)
}

pub fn balance_slot_at(holder: Address, position: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[56..64].copy_from_slice(&position.to_be_bytes());
    keccak256(buf)
}

pub fn allowance_slot_at(owner: Address, spender: Address, position: u64) -> B256 {
    let inner = balance_slot_at(owner, position);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(spender.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

fn state_override_single(contract: Address, slot: B256, value: B256) -> StateOverride {
    let mut overrides = StateOverride::default();
    let mut state_diff = alloy::primitives::map::B256HashMap::default();
    state_diff.insert(slot, value);
    overrides
        .insert(contract, AccountOverride { state_diff: Some(state_diff), ..Default::default() });
    overrides
}

async fn find_balance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> anyhow::Result<u64> {
    let calldata = IERC20::balanceOfCall { account: holder }.abi_encode();
    let sentinel = B256::from(PROBE_SENTINEL);
    for position in 0..=MAX_PROBE_SLOT {
        let slot = balance_slot_at(holder, position);
        let result = provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(token)),
                input: AlloyBytes::from(calldata.clone()).into(),
                ..Default::default()
            })
            .overrides(state_override_single(token, slot, sentinel))
            .await?;
        if result.len() >= 32 && result[..32] == *sentinel.as_slice() {
            return Ok(position);
        }
    }
    bail!("could not detect balance slot for {token:#x}");
}

async fn find_allowance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> anyhow::Result<u64> {
    let calldata = IERC20::allowanceCall { owner, spender }.abi_encode();
    let sentinel = B256::from(PROBE_SENTINEL);
    for position in 0..=MAX_PROBE_SLOT {
        let slot = allowance_slot_at(owner, spender, position);
        let result = provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(token)),
                input: AlloyBytes::from(calldata.clone()).into(),
                ..Default::default()
            })
            .overrides(state_override_single(token, slot, sentinel))
            .await?;
        if result.len() >= 32 && result[..32] == *sentinel.as_slice() {
            return Ok(position);
        }
    }
    bail!("could not detect allowance slot for {token:#x}");
}

/// Probe slots for `token_in`, then build `StorageOverrides` that give `sender`
/// a huge ERC-20 balance + allowance to `router`, plus a huge native-ETH balance
/// so the node accepts the `gas_limit * max_fee_per_gas` affordability check.
async fn build_fynd_overrides(
    provider: &RootProvider<Ethereum>,
    sender: Address,
    token_in: Address,
    router: Address,
) -> anyhow::Result<StorageOverrides> {
    let (balance_pos, allowance_pos) = tokio::join!(
        find_balance_slot(provider, token_in, sender),
        find_allowance_slot(provider, token_in, sender, router),
    );
    let balance_pos = balance_pos?;
    let allowance_pos = allowance_pos?;

    let huge = huge_balance();
    let mut overrides = StorageOverrides::default();
    let token_addr = Bytes::copy_from_slice(token_in.as_slice());
    overrides.insert(
        token_addr.clone(),
        Bytes::copy_from_slice(balance_slot_at(sender, balance_pos).as_slice()),
        Bytes::copy_from_slice(huge.as_slice()),
    );
    overrides.insert(
        token_addr,
        Bytes::copy_from_slice(allowance_slot_at(sender, router, allowance_pos).as_slice()),
        Bytes::copy_from_slice(huge.as_slice()),
    );
    overrides.set_native_balance(
        Bytes::copy_from_slice(sender.as_slice()),
        BigUint::from_bytes_be(huge.as_slice()),
    );
    Ok(overrides)
}

/// Outcome of one dry-run simulation.
#[derive(Debug)]
pub enum SimOutcome {
    Ok { actual_gas: u64 },
    Reverted { reason: String },
}

pub struct Simulator {
    provider: RootProvider<Ethereum>,
    sender: Address,
}

impl Simulator {
    pub fn new(provider: RootProvider<Ethereum>, sender: Address) -> Self {
        Self { provider, sender }
    }

    /// Current gas price from the node (wei). Call once per run.
    pub async fn gas_price(&self) -> anyhow::Result<BigUint> {
        let price = self.provider.get_gas_price().await?;
        Ok(BigUint::from(price))
    }

    /// Simulate `quote` by routing through `FyndClient::execute_swap` in dry-run
    /// mode. The signing step uses `Signature::test_signature()` because dry-run
    /// never inspects the signature (Tycho router authorizes inside calldata,
    /// and `dry_run_execute` discards the signature entirely).
    ///
    /// `max_fee_per_gas` and `max_priority_fee_per_gas` are pinned to `1` wei so
    /// that the returned `SettledOrder::gas_cost()` equals the raw `gas_used` in
    /// numeric value — the audit applies its own gas-price snapshot downstream.
    pub async fn simulate(&self, client: &FyndClient, quote: &Quote) -> SimOutcome {
        let Some(tx) = quote.transaction() else {
            return SimOutcome::Reverted { reason: "quote has no transaction".to_string() };
        };
        let router = match Address::try_from(tx.to().as_ref()) {
            Ok(a) => a,
            Err(e) => return SimOutcome::Reverted { reason: format!("bad to addr: {e}") },
        };
        let token_in_bytes = match quote
            .route()
            .and_then(|r| r.swaps().first())
            .map(|s| s.token_in().clone())
        {
            Some(b) => b,
            None => return SimOutcome::Reverted { reason: "quote has no route".into() },
        };
        let token_in = match Address::try_from(token_in_bytes.as_ref()) {
            Ok(a) => a,
            Err(e) => return SimOutcome::Reverted { reason: format!("bad token_in: {e}") },
        };

        let overrides =
            match build_fynd_overrides(&self.provider, self.sender, token_in, router).await {
                Ok(o) => o,
                Err(e) => return SimOutcome::Reverted { reason: format!("overrides: {e}") },
            };

        // Pin gas_limit to the block limit so `swap_payload` doesn't fall back to
        // calling `estimate_gas` *without* overrides (which would revert — our
        // sender has no real balance). The actual gas measurement happens in
        // `dry_run_execute` below, using our overrides.
        //
        // `max_fee_per_gas` must stay above the current block's base fee or the
        // node rejects the request (error -32000). We pin it to a fixed high
        // value (1000 gwei ≫ any realistic base fee) so we can exactly recover
        // `gas_used` from `SettledOrder::gas_cost()` by dividing.
        let hints = SigningHints::default()
            .with_sender(self.sender)
            .with_max_fee_per_gas(MAX_FEE_PER_GAS_WEI)
            .with_max_priority_fee_per_gas(MAX_FEE_PER_GAS_WEI)
            .with_gas_limit(30_000_000);

        let payload = match client
            .swap_payload(quote.clone(), &hints)
            .await
        {
            Ok(p) => p,
            Err(e) => return SimOutcome::Reverted { reason: format!("swap_payload: {e}") },
        };
        let signed = SignedSwap::assemble(payload, Signature::test_signature());

        let options = ExecutionOptions {
            dry_run: true,
            storage_overrides: Some(overrides),
            fetch_revert_reason: false,
        };
        let receipt = match client
            .execute_swap(signed, &options)
            .await
        {
            Ok(r) => r,
            Err(e) => return SimOutcome::Reverted { reason: format!("execute_swap: {e}") },
        };
        let settled = match receipt.await {
            Ok(s) => s,
            Err(e) => return SimOutcome::Reverted { reason: format!("receipt: {e}") },
        };

        // gas_cost = gas_used * MAX_FEE_PER_GAS_WEI (exact). Divide to recover gas_used.
        let divisor = BigUint::from(MAX_FEE_PER_GAS_WEI);
        let gas_used_big = settled.gas_cost() / &divisor;
        match gas_used_big.to_u64() {
            Some(gas_used) => SimOutcome::Ok { actual_gas: gas_used },
            None => SimOutcome::Reverted {
                reason: format!("gas_used overflows u64: {gas_used_big}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::hex;

    use super::*;

    // Slot-formula parity with the swap-cli test suite, so we detect
    // silent regressions from copy-drift.

    #[test]
    fn balance_slot_zero_address_position_zero_matches_spec() {
        let slot = balance_slot_at(Address::ZERO, 0);
        let expected = hex!("ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5");
        assert_eq!(slot.0, expected);
    }

    #[test]
    fn balance_slot_usdc_position_zero_matches_spec() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let slot = balance_slot_at(usdc, 0);
        let expected = hex!("c6521c8ea4247e8beb499344e591b9401fb2807ff9997dd598fd9e56c73a264d");
        assert_eq!(slot.0, expected);
    }

    #[test]
    fn allowance_slot_usdc_weth_position_zero_matches_spec() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let slot = allowance_slot_at(usdc, weth, 0);
        let expected = hex!("7b7d28f4178b11583278450af3b85d49a04fd0597c53f7ed3fbfac3750fde37d");
        assert_eq!(slot.0, expected);
    }
}
