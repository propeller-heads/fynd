//! Trace-guided ERC-20 storage-layout discovery.
//!
//! State overrides only help when they land on the slots a token actually reads. Most ERC-20s
//! use Solidity's `keccak256(holder || base_slot)` mapping convention, but real tokens also use
//! Vyper's reversed order, deep inheritance slots, proxies whose storage lives elsewhere, and
//! rebasing shares. This module traces the token's read-only access, validates the observed slot
//! with a sentinel override, then recovers the mapping convention needed to fund a simulated swap.

use alloy::{
    eips::BlockId,
    network::Ethereum,
    primitives::{keccak256, map::B256HashMap, Address, Bytes, TxKind, B256, U256},
    providers::{ext::DebugApi, Provider, RootProvider},
    rpc::{
        json_rpc::ErrorPayload,
        types::{
            state::{AccountOverride, StateOverride},
            trace::geth::{GethDebugTracingCallOptions, GethDebugTracingOptions, PreStateConfig},
            TransactionRequest,
        },
    },
    sol,
    sol_types::SolCall,
};

/// Highest mapping base searched when recovering a slot's convention.
///
/// Recovery is local keccak arithmetic, not RPC, so the bound only caps CPU: 640 bases across two
/// key orders is a few thousand hashes. It sits well past the deepest base a token in the Tycho
/// set uses, and a token beyond it fails discovery rather than being funded wrongly.
const MAX_BASE_SLOT: u16 = 640;
/// Slots sentinel-verified per probe, across every account the trace touched.
///
/// Each one costs an `eth_call`, and they all run against the layout-discovery timeout. The bound
/// covers the whole probe rather than one account, so a proxy whose read spans several accounts
/// costs no more than a token that keeps everything in one.
const MAX_SLOTS_TO_VERIFY: usize = 48;
/// A value that survives common packed-balance flags and narrow integer casts.
pub(crate) const PROBE_SENTINEL: U256 = U256::from_limbs([0xdead_beef_cafe_babe, 0, 0, 0]);
/// OpenZeppelin v5's ERC-20 balances mapping, under the namespace ERC-7201 prescribes.
///
/// This is `keccak256(abi.encode(uint256(keccak256("openzeppelin.storage.ERC20")) - 1)) &
/// ~bytes32(uint256(0xff))`.
const OZ_V5_BALANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00"));
/// OpenZeppelin v5's ERC-20 allowances mapping.
///
/// Allowances are field 1 of `ERC20Storage`, so their namespace is the balances namespace plus one.
const OZ_V5_ALLOWANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace01"));

sol! {
    interface IERC20LayoutProbe {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    /// The view a share-accounted rebasing token keeps its mapping under. stETH is the one in the
    /// Tycho set; the rest of the family answers the same call.
    interface ISharesToken {
        function sharesOf(address account) external view returns (uint256);
    }
}

/// Mapping-key convention used by a token implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOrder {
    /// Solidity: `keccak256(pad32(address) || pad32(slot))`.
    Solidity,
    /// Vyper: `keccak256(pad32(slot) || pad32(address))`.
    Vyper,
}

/// The base of one balance or allowance mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingPosition {
    /// A small integer base slot under Solidity or Vyper mapping layout.
    Direct {
        /// Declaration order of the mapping in the contract's storage.
        base: u16,
        /// Which way the implementation hashes the key and the base.
        key_order: KeyOrder,
    },
    /// OpenZeppelin v5's namespaced storage. Which namespace applies follows from the mapping
    /// being addressed, so a balance reads the balances one and an allowance the allowances one.
    OpenZeppelinV5,
}

/// The slots needed to fund and approve one simulated token input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenLayout {
    storage_contract: Address,
    balance: MappingPosition,
    allowance: MappingPosition,
}

impl TokenLayout {
    /// Creates a layout from known positions.
    pub const fn new(
        storage_contract: Address,
        balance: MappingPosition,
        allowance: MappingPosition,
    ) -> Self {
        Self { storage_contract, balance, allowance }
    }

    /// Contract whose state holds this token's balances and allowances.
    ///
    /// A proxy keeps them somewhere other than the address the swap calls, so an override goes to
    /// this contract rather than to the token.
    pub fn storage_contract(self) -> Address {
        self.storage_contract
    }

    /// Slot holding one holder's balance, or its share balance on a rebasing token.
    pub fn balance_slot(self, holder: Address) -> B256 {
        balance_slot(holder, self.balance)
    }

    /// Slot holding what one owner has approved one spender to spend.
    pub fn allowance_slot(self, owner: Address, spender: Address) -> B256 {
        allowance_slot(owner, spender, self.allowance)
    }
}

/// Why a token's storage layout could not be resolved.
///
/// The two are cached differently: a layout this module cannot resolve is a property of the token
/// and stays decided, while a node that failed to answer says nothing about the token and is
/// retried on the next quote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    /// The token's layout is not one this module recovers.
    #[error("{0}")]
    Unsupported(String),
    /// The node did not answer a probe.
    #[error("{0}")]
    Rpc(String),
}

/// Resolves the storage a quote's input token reads, so an override can fund it.
pub async fn discover_layout(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
    spender: Address,
) -> Result<TokenLayout, DiscoveryError> {
    let (storage_contract, balance) = discover_balance(provider, token, holder).await?;

    let allowance_calldata =
        IERC20LayoutProbe::allowanceCall { owner: holder, spender }.abi_encode();
    let (allowance_contract, observed) =
        find_accessed_slot(provider, token, &allowance_calldata).await?;
    if allowance_contract != storage_contract {
        return Err(DiscoveryError::Unsupported(format!(
            "token {token:#x} stores balance and allowance in different contracts ({storage_contract:#x}, {allowance_contract:#x})"
        )));
    }
    let allowance = recover_position(observed, |position| {
        allowance_slot(holder, spender, position)
    })
    .ok_or_else(|| {
        DiscoveryError::Unsupported(format!(
            "could not recover a supported allowance mapping for {token:#x}; observed slot {observed:#x}"
        ))
    })?;

    Ok(TokenLayout::new(storage_contract, balance, allowance))
}

/// Places the balance mapping, trying the plain balance view before the share-accounted one.
///
/// A rebasing token multiplies shares by a pooled rate inside `balanceOf`, so tracing that call
/// finds the arithmetic and not the mapping; `sharesOf` reads the mapping directly. The retry
/// replaces a list of addresses, which would name only the tokens already known to need it and
/// would have to be kept per chain.
async fn discover_balance(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> Result<(Address, MappingPosition), DiscoveryError> {
    let probes = [
        IERC20LayoutProbe::balanceOfCall { account: holder }.abi_encode(),
        ISharesToken::sharesOfCall { account: holder }.abi_encode(),
    ];
    let mut failure = DiscoveryError::Unsupported(format!(
        "could not identify a balance storage slot for {token:#x}"
    ));
    for calldata in probes {
        match find_accessed_slot(provider, token, &calldata).await {
            Ok((storage_contract, observed)) => {
                if let Some(position) =
                    recover_position(observed, |position| balance_slot(holder, position))
                {
                    return Ok((storage_contract, position));
                }
                failure = DiscoveryError::Unsupported(format!(
                    "could not recover a supported balance mapping for {token:#x}; observed slot {observed:#x}"
                ));
            }
            // A node that refused to answer says nothing about the token, so it ends discovery
            // rather than sending the caller on to a view this token may not even have.
            Err(error @ DiscoveryError::Rpc(_)) => return Err(error),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

/// Finds the slot a read-only call depends on, by overwriting each slot it touched in turn.
async fn find_accessed_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
) -> Result<(Address, B256), DiscoveryError> {
    let trace = provider
        .debug_trace_call_prestate(
            token_call(token, calldata),
            BlockId::latest(),
            GethDebugTracingCallOptions::new(GethDebugTracingOptions::prestate_tracer(
                PreStateConfig::default(),
            )),
        )
        .await
        .map_err(|error| {
            DiscoveryError::Rpc(format!(
                "debug_traceCall prestate probe for {token:#x} failed: {error}"
            ))
        })?;

    // Highest keys first: a mapping slot is a keccak hash and lands near the top of the key order,
    // while a contract's fixed fields sit at 0, 1, 2 and sort to the bottom. Taking the cap from
    // that end reaches the mapping on a token that reads many fixed slots.
    let mut candidates: Vec<(Address, B256)> = Vec::new();
    for (&storage_contract, account) in trace.pre_state() {
        candidates.extend(
            account
                .storage
                .keys()
                .rev()
                .map(|&slot| (storage_contract, slot)),
        );
    }
    candidates.truncate(MAX_SLOTS_TO_VERIFY);

    // Every candidate is verified with its own `eth_call`, so they go out together: run in turn
    // they would spend the discovery timeout on round trips rather than on work.
    let verdicts = futures::future::join_all(
        candidates
            .iter()
            .map(|&(storage_contract, slot)| {
                slot_matches(provider, token, storage_contract, calldata, slot)
            }),
    )
    .await;
    for (&(storage_contract, slot), verdict) in candidates.iter().zip(verdicts) {
        if verdict? {
            return Ok((storage_contract, slot));
        }
    }
    Err(DiscoveryError::Unsupported(format!(
        "could not identify a balance or allowance storage slot for {token:#x}"
    )))
}

fn token_call(token: Address, calldata: &[u8]) -> TransactionRequest {
    TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: Bytes::copy_from_slice(calldata).into(),
        ..Default::default()
    }
}

/// Whether overwriting one slot changes what the token reports, which is what identifies it.
async fn slot_matches(
    provider: &RootProvider<Ethereum>,
    token: Address,
    storage_contract: Address,
    calldata: &[u8],
    slot: B256,
) -> Result<bool, DiscoveryError> {
    match provider
        .call(token_call(token, calldata))
        .overrides(state_override_single(storage_contract, slot, B256::from(PROBE_SENTINEL)))
        .await
    {
        Ok(response) => {
            Ok(response.len() >= 32 && U256::from_be_slice(&response[..32]) == PROBE_SENTINEL)
        }
        Err(error) => match error.as_error_resp() {
            // A guarded proxy reverts when its implementation slot is overwritten, which proves
            // the slot is not the mapping.
            Some(payload) if is_revert(payload) => Ok(false),
            // Every other error response -- a rate limit, a compute budget, a head that moved --
            // proves nothing about the slot. Counting it as a miss would end discovery in
            // "could not identify", and that verdict is cached for the life of the process.
            Some(payload) => Err(DiscoveryError::Rpc(format!(
                "sentinel probe for {token:#x} slot {slot:#x} was refused: {payload}"
            ))),
            None => Err(DiscoveryError::Rpc(format!(
                "sentinel probe for {token:#x} slot {slot:#x} failed: {error}"
            ))),
        },
    }
}

/// Whether an error response is the contract reverting rather than the node declining to run.
fn is_revert(payload: &ErrorPayload) -> bool {
    // 3 is the code geth returns for a reverted call; the message covers nodes that report the
    // same thing under a code of their own.
    payload.code == 3 || payload.message.contains("revert")
}

/// Builds an override that writes one storage value for a contract.
fn state_override_single(contract: Address, slot: B256, value: B256) -> StateOverride {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(slot, value);
    StateOverride::from_iter([(
        contract,
        AccountOverride { state_diff: Some(state_diff), ..Default::default() },
    )])
}

/// Finds the convention whose arithmetic reproduces an observed slot.
///
/// `slot_for` closes over the keys, so one search serves balances and allowances alike.
fn recover_position(
    slot: B256,
    slot_for: impl Fn(MappingPosition) -> B256,
) -> Option<MappingPosition> {
    for base in 0..=MAX_BASE_SLOT {
        for key_order in [KeyOrder::Solidity, KeyOrder::Vyper] {
            let direct = MappingPosition::Direct { base, key_order };
            if slot_for(direct) == slot {
                return Some(direct);
            }
        }
    }
    (slot_for(MappingPosition::OpenZeppelinV5) == slot).then_some(MappingPosition::OpenZeppelinV5)
}

/// Slot holding one holder's balance under a given convention.
fn balance_slot(holder: Address, position: MappingPosition) -> B256 {
    match position {
        MappingPosition::Direct { base, key_order: KeyOrder::Solidity } => {
            solidity_mapping(holder, B256::from(U256::from(base)))
        }
        MappingPosition::Direct { base, key_order: KeyOrder::Vyper } => vyper_mapping(holder, base),
        MappingPosition::OpenZeppelinV5 => solidity_mapping(holder, OZ_V5_BALANCES_NS),
    }
}

/// Slot holding one owner-and-spender allowance under a given convention.
fn allowance_slot(owner: Address, spender: Address, position: MappingPosition) -> B256 {
    match position {
        MappingPosition::Direct { base, key_order: KeyOrder::Solidity } => {
            solidity_mapping(spender, solidity_mapping(owner, B256::from(U256::from(base))))
        }
        MappingPosition::Direct { base, key_order: KeyOrder::Vyper } => {
            let inner = vyper_mapping(owner, base);
            let mut buffer = [0_u8; 64];
            buffer[..32].copy_from_slice(inner.as_slice());
            buffer[44..].copy_from_slice(spender.as_slice());
            keccak256(buffer)
        }
        MappingPosition::OpenZeppelinV5 => {
            solidity_mapping(spender, solidity_mapping(owner, OZ_V5_ALLOWANCES_NS))
        }
    }
}

fn solidity_mapping(holder: Address, base: B256) -> B256 {
    let mut buffer = [0_u8; 64];
    buffer[12..32].copy_from_slice(holder.as_slice());
    buffer[32..].copy_from_slice(base.as_slice());
    keccak256(buffer)
}

fn vyper_mapping(holder: Address, base: u16) -> B256 {
    let mut buffer = [0_u8; 64];
    buffer[30..32].copy_from_slice(&base.to_be_bytes());
    buffer[44..].copy_from_slice(holder.as_slice());
    keccak256(buffer)
}

#[cfg(test)]
#[path = "../tests/simulation/token_layout.rs"]
mod tests;
