//! ERC-20 balance and allowance storage-slot detection for state overrides.

use alloy::{
    network::Ethereum,
    primitives::{keccak256, map::B256HashMap, Address, Bytes, TxKind, B256, U256},
    providers::{Provider, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionRequest,
    },
    sol,
    sol_types::SolCall,
};
use futures::future::try_join_all;

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// Highest standard Solidity mapping position probed before trying OpenZeppelin v5 storage.
pub const MAX_PROBE_SLOT: u64 = 20;
/// Sentinel written into a candidate storage slot during detection.
///
/// Deliberately avoids high bits so tokens that mask upper bits (such as USDC, which packs a
/// blacklist flag in bit 255 of its balance slot) still return this value exactly.
const PROBE_SENTINEL: U256 = U256::from_limbs([0xdead_beef, 0, 0, 0]);
/// OpenZeppelin v5 namespace for the ERC-20 balances mapping.
///
/// This is `keccak256(abi.encode(uint256(keccak256("openzeppelin.storage.ERC20")) - 1)) &
/// ~bytes32(uint256(0xff))`, which is the namespace prescribed by ERC-7201.
const OZ_V5_BALANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00"));
/// OpenZeppelin v5 namespace for the ERC-20 allowances mapping.
///
/// Allowances are field 1 in `ERC20Storage`, so their namespace is the balances namespace plus
/// one.
const OZ_V5_ALLOWANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace01"));

/// Error returned while resolving an ERC-20 storage mapping position.
#[derive(Debug, thiserror::Error)]
pub enum Erc20SlotError {
    /// A probe call to the token contract failed.
    #[error("ERC-20 slot probe for {token:#x} failed: {reason}")]
    Probe {
        /// Token contract that was probed.
        token: Address,
        /// RPC failure returned by the node.
        reason: String,
    },
    /// No supported mapping layout produced the sentinel value.
    #[error("could not detect {mapping} slot for {token:#x}; the token may use a non-standard storage layout")]
    NotFound {
        /// Token contract that was probed.
        token: Address,
        /// Mapping name used in the error message.
        mapping: &'static str,
    },
}

/// Resolved base position of an ERC-20 storage mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingPosition {
    /// A standard Solidity mapping position.
    Standard(u64),
    /// The OpenZeppelin v5 ERC-20 namespaced storage layout.
    OpenZeppelinV5,
}

/// Computes a standard Solidity balance mapping slot.
pub fn balance_slot_at(holder: Address, position: u64) -> B256 {
    let mut buffer = [0_u8; 64];
    buffer[12..32].copy_from_slice(holder.as_slice());
    buffer[56..64].copy_from_slice(&position.to_be_bytes());
    keccak256(buffer)
}

/// Computes a balance mapping slot using a full 32-byte base position.
pub fn balance_slot_at_b256(holder: Address, base: B256) -> B256 {
    let mut buffer = [0_u8; 64];
    buffer[12..32].copy_from_slice(holder.as_slice());
    buffer[32..64].copy_from_slice(base.as_slice());
    keccak256(buffer)
}

/// Computes a standard Solidity allowance nested-mapping slot.
pub fn allowance_slot_at(owner: Address, spender: Address, position: u64) -> B256 {
    let inner = balance_slot_at(owner, position);
    let mut buffer = [0_u8; 64];
    buffer[12..32].copy_from_slice(spender.as_slice());
    buffer[32..64].copy_from_slice(inner.as_slice());
    keccak256(buffer)
}

/// Computes an allowance nested-mapping slot using a full 32-byte base position.
pub fn allowance_slot_at_b256(owner: Address, spender: Address, base: B256) -> B256 {
    let inner = balance_slot_at_b256(owner, base);
    let mut buffer = [0_u8; 64];
    buffer[12..32].copy_from_slice(spender.as_slice());
    buffer[32..64].copy_from_slice(inner.as_slice());
    keccak256(buffer)
}

/// Computes a balance slot from a resolved mapping position.
pub fn balance_slot_for_position(holder: Address, position: MappingPosition) -> B256 {
    match position {
        MappingPosition::Standard(index) => balance_slot_at(holder, index),
        MappingPosition::OpenZeppelinV5 => balance_slot_at_b256(holder, OZ_V5_BALANCES_NS),
    }
}

/// Computes an allowance slot from a resolved mapping position.
pub fn allowance_slot_for_position(
    owner: Address,
    spender: Address,
    position: MappingPosition,
) -> B256 {
    match position {
        MappingPosition::Standard(index) => allowance_slot_at(owner, spender, index),
        MappingPosition::OpenZeppelinV5 => {
            allowance_slot_at_b256(owner, spender, OZ_V5_ALLOWANCES_NS)
        }
    }
}

/// Builds an override that writes one storage value for a contract.
pub fn state_override_single(contract: Address, slot: B256, value: B256) -> StateOverride {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(slot, value);
    StateOverride::from_iter([(
        contract,
        AccountOverride { state_diff: Some(state_diff), ..Default::default() },
    )])
}

async fn probe_position(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
    slot_for: impl Fn(MappingPosition) -> B256,
) -> Result<Option<MappingPosition>, Erc20SlotError> {
    let candidates = (0..=MAX_PROBE_SLOT)
        .map(MappingPosition::Standard)
        .chain(std::iter::once(MappingPosition::OpenZeppelinV5))
        .collect::<Vec<_>>();
    let probes = candidates.into_iter().map(|position| {
        let slot = slot_for(position);
        async move {
            Ok::<_, Erc20SlotError>((
                position,
                slot_matches(provider, token, calldata, slot).await?,
            ))
        }
    });
    let results = try_join_all(probes).await?;
    Ok(results
        .into_iter()
        .find_map(|(position, matches)| matches.then_some(position)))
}

async fn slot_matches(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
    slot: B256,
) -> Result<bool, Erc20SlotError> {
    let sentinel = B256::from(PROBE_SENTINEL);
    let response = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(token)),
            input: Bytes::copy_from_slice(calldata).into(),
            ..Default::default()
        })
        .overrides(state_override_single(token, slot, sentinel))
        .await
        .map_err(|error| Erc20SlotError::Probe { token, reason: error.to_string() })?;
    Ok(response.len() >= 32 && response[..32] == *sentinel.as_slice())
}

/// Resolves the balance mapping position for an ERC-20 token.
pub async fn find_balance_position(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> Result<MappingPosition, Erc20SlotError> {
    let calldata = IERC20::balanceOfCall { account: holder }.abi_encode();
    let Some(position) = probe_position(provider, token, &calldata, |candidate| {
        balance_slot_for_position(holder, candidate)
    })
    .await?
    else {
        return Err(Erc20SlotError::NotFound { token, mapping: "balance" });
    };
    Ok(position)
}

/// Resolves the allowance mapping position for an ERC-20 token.
pub async fn find_allowance_position(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<MappingPosition, Erc20SlotError> {
    let calldata = IERC20::allowanceCall { owner, spender }.abi_encode();
    let Some(position) = probe_position(provider, token, &calldata, |candidate| {
        allowance_slot_for_position(owner, spender, candidate)
    })
    .await?
    else {
        return Err(Erc20SlotError::NotFound { token, mapping: "allowance" });
    };
    Ok(position)
}

/// Finds the full storage slot that holds an ERC-20 balance.
pub async fn find_balance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> Result<B256, Erc20SlotError> {
    Ok(balance_slot_for_position(holder, find_balance_position(provider, token, holder).await?))
}

/// Finds the full storage slot that holds an ERC-20 allowance.
pub async fn find_allowance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<B256, Erc20SlotError> {
    Ok(allowance_slot_for_position(
        owner,
        spender,
        find_allowance_position(provider, token, owner, spender).await?,
    ))
}

#[cfg(test)]
#[path = "../tests/simulation/erc20_slots.rs"]
mod tests;
