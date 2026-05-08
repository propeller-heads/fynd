//! On-chain simulator: routes gas estimation through `FyndClient::execute_swap`
//! in dry-run mode. With `max_fee_per_gas` pinned to a known constant, the
//! returned `SettledOrder::gas_cost()` divides exactly to the raw `gas_used`.
//!
//! Storage-slot detection (balance + allowance) is delegated to `erc20-overrides`;
//! we still keep a local alloy provider for the probe `eth_call`s.

use alloy::{
    network::Ethereum,
    primitives::{Address, Signature, B256, U256},
    providers::{Provider, RootProvider},
};
use bytes::Bytes;
use erc20_overrides::{allowance_slot_at, balance_slot_at, find_allowance_slot, find_balance_slot};
use fynd_client::{
    ExecutionOptions, FyndClient, Quote, SignedSwap, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;

/// 1000 gwei — well above any realistic mainnet base fee, so nodes accept the
/// simulated tx. Used to recover `gas_used` from `SettledOrder::gas_cost()` via
/// exact division.
const MAX_FEE_PER_GAS_WEI: u128 = 1_000_000_000_000;

// U256::MAX >> 1 avoids clobbering tokens that pack metadata into bit 255
// (e.g. USDC's blacklist flag).
fn huge_balance() -> B256 {
    B256::from(U256::MAX >> 1)
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
    /// `max_fee_per_gas` and `max_priority_fee_per_gas` are pinned to
    /// `MAX_FEE_PER_GAS_WEI` (1000 gwei) — well above any realistic base fee so
    /// the node accepts the request, and a known constant so we can recover
    /// `gas_used` exactly from `SettledOrder::gas_cost()` by dividing. The audit
    /// applies its own gas-price snapshot to `gas_used` downstream.
    pub async fn simulate(&self, client: &FyndClient, quote: &Quote) -> SimOutcome {
        match self.try_simulate(client, quote).await {
            Ok(actual_gas) => SimOutcome::Ok { actual_gas },
            Err(reason) => SimOutcome::Reverted { reason },
        }
    }

    async fn try_simulate(&self, client: &FyndClient, quote: &Quote) -> Result<u64, String> {
        let tx = quote
            .transaction()
            .ok_or_else(|| "quote has no transaction".to_string())?;
        let router =
            Address::try_from(tx.to().as_ref()).map_err(|e| format!("bad to addr: {e}"))?;
        let token_in_bytes = quote
            .route()
            .and_then(|r| r.swaps().first())
            .map(|s| s.token_in().clone())
            .ok_or_else(|| "quote has no route".to_string())?;
        let token_in =
            Address::try_from(token_in_bytes.as_ref()).map_err(|e| format!("bad token_in: {e}"))?;

        let overrides = build_fynd_overrides(&self.provider, self.sender, token_in, router)
            .await
            .map_err(|e| format!("overrides: {e}"))?;

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

        let payload = client
            .swap_payload(quote.clone(), &hints)
            .await
            .map_err(|e| format!("swap_payload: {e}"))?;
        let signed = SignedSwap::assemble(payload, Signature::test_signature());

        let options = ExecutionOptions {
            dry_run: true,
            storage_overrides: Some(overrides),
            fetch_revert_reason: false,
        };
        let receipt = client
            .execute_swap(signed, &options)
            .await
            .map_err(|e| format!("execute_swap: {e}"))?;
        let settled = receipt
            .await
            .map_err(|e| format!("receipt: {e}"))?;

        // gas_cost = gas_used * MAX_FEE_PER_GAS_WEI (exact). Divide to recover gas_used.
        let gas_used_big = settled.gas_cost() / BigUint::from(MAX_FEE_PER_GAS_WEI);
        gas_used_big
            .to_u64()
            .ok_or_else(|| format!("gas_used overflows u64: {gas_used_big}"))
    }
}
