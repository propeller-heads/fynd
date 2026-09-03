//! Trace-guided ERC-20 storage-layout discovery for quote simulation.
//!
//! State overrides only help when they land on the slots a token actually reads. Most ERC-20s
//! use Solidity's `keccak256(holder || base_slot)` mapping convention, but real tokens also use
//! Vyper's reversed order, deep inheritance slots, proxies whose storage lives elsewhere, and
//! rebasing shares. This module traces the token's read-only access, validates the observed slot
//! with a sentinel override, then recovers the mapping convention needed to fund a simulated swap.

use alloy::{
    eips::BlockId,
    network::Ethereum,
    primitives::{address, keccak256, map::B256HashMap, Address, Bytes, TxKind, B256, U256},
    providers::{ext::DebugApi, Provider, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        trace::geth::{GethDebugTracingCallOptions, GethDebugTracingOptions, PreStateConfig},
        TransactionRequest,
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
/// Slots per traced account that are sentinel-verified.
///
/// Each one costs an `eth_call`, and they run against the slot-discovery timeout. A `balanceOf`
/// that touches more than this many slots is a token whose layout this module does not resolve.
const MAX_SLOTS_TO_VERIFY: usize = 32;
/// A value that survives common packed-balance flags and narrow integer casts.
const PROBE_SENTINEL: U256 = U256::from_limbs([0xdead_beef_cafe_babe, 0, 0, 0]);
/// Mainnet Lido stETH.
///
/// Its `balanceOf` multiplies a holder's shares by the pooled-ETH rate, so tracing it finds the
/// arithmetic rather than the mapping. The shares view is traced instead. The address is mainnet's
/// and matches nothing on another chain, which leaves every other token on the ordinary path.
const STETH: Address = address!("ae7ab96520de3a18e5e111b5eaab095312d7fe84");
/// OpenZeppelin v5's standard ERC-7201 balance namespace.
const OZ_V5_BALANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00"));
/// Field one of OpenZeppelin v5's ERC-20 storage struct.
const OZ_V5_ALLOWANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace01"));

sol! {
    interface IERC20LayoutProbe {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    interface ILidoStEth {
        function sharesOf(address account) external view returns (uint256);
    }
}

/// Mapping-key convention used by a token implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyOrder {
    /// Solidity: `keccak256(pad32(address) || pad32(slot))`.
    Solidity,
    /// Vyper: `keccak256(pad32(slot) || pad32(address))`.
    Vyper,
}

/// The base of one balance or allowance mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MappingPosition {
    /// A small integer base slot under Solidity or Vyper mapping layout.
    Direct { base: u16, key_order: KeyOrder },
    /// The canonical OpenZeppelin v5 ERC-7201 balances namespace.
    OpenZeppelinV5Balances,
    /// The canonical OpenZeppelin v5 ERC-7201 allowances namespace.
    OpenZeppelinV5Allowances,
}

/// The slots needed to fund and approve one simulated token input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TokenLayout {
    storage_contract: Address,
    balance: MappingPosition,
    allowance: MappingPosition,
}

impl TokenLayout {
    /// Creates a layout from known positions. [`discover_layout`] resolves them from a live token;
    /// this builds one directly, which is what the tests need.
    pub(crate) const fn new(
        storage_contract: Address,
        balance: MappingPosition,
        allowance: MappingPosition,
    ) -> Self {
        Self { storage_contract, balance, allowance }
    }

    /// Contract whose state holds this token's balances and allowances.
    pub(crate) fn storage_contract(self) -> Address {
        self.storage_contract
    }

    /// Slot of `balanceOf(holder)` or, for stETH, `sharesOf(holder)`.
    pub(crate) fn balance_slot(self, holder: Address) -> B256 {
        mapping_slot(holder, self.balance)
    }

    /// Slot of `allowance(owner, spender)`.
    pub(crate) fn allowance_slot(self, owner: Address, spender: Address) -> B256 {
        allowance_slot(owner, spender, self.allowance)
    }
}

/// Why a token's storage layout could not be resolved.
///
/// The two are cached differently: a layout this module cannot resolve is a property of the token
/// and stays decided, while a node that failed to answer says nothing about the token and is
/// retried on the next quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryError {
    /// The token's layout is not one this module recovers.
    Unsupported(String),
    /// The node did not answer a probe.
    Rpc(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) | Self::Rpc(reason) => formatter.write_str(reason),
        }
    }
}

/// Resolves the storage a quote's input token reads, so an override can fund it.
pub(crate) async fn discover_layout(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
    spender: Address,
) -> Result<TokenLayout, DiscoveryError> {
    let balance_calldata = balance_calldata(token, holder);
    let (storage_contract, balance_slot) =
        find_accessed_slot(provider, token, &balance_calldata).await?;
    let balance = recover_balance_position(balance_slot, holder).ok_or_else(|| {
        DiscoveryError::Unsupported(format!(
            "could not recover a supported balance mapping for {token:#x}; observed slot {balance_slot:#x}"
        ))
    })?;

    let allowance_calldata =
        IERC20LayoutProbe::allowanceCall { owner: holder, spender }.abi_encode();
    let (allowance_contract, allowance_slot) =
        find_accessed_slot(provider, token, &allowance_calldata).await?;
    if allowance_contract != storage_contract {
        return Err(DiscoveryError::Unsupported(format!(
            "token {token:#x} stores balance and allowance in different contracts ({storage_contract:#x}, {allowance_contract:#x})"
        )));
    }
    let allowance = recover_allowance_position(allowance_slot, holder, spender).ok_or_else(|| {
        DiscoveryError::Unsupported(format!(
            "could not recover a supported allowance mapping for {token:#x}; observed slot {allowance_slot:#x}"
        ))
    })?;

    Ok(TokenLayout::new(storage_contract, balance, allowance))
}

fn balance_calldata(token: Address, holder: Address) -> Vec<u8> {
    if token == STETH {
        ILidoStEth::sharesOfCall { account: holder }.abi_encode()
    } else {
        IERC20LayoutProbe::balanceOfCall { account: holder }.abi_encode()
    }
}

async fn find_accessed_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
) -> Result<(Address, B256), DiscoveryError> {
    let call = token_call(token, calldata);
    let trace = provider
        .debug_trace_call_prestate(
            call.clone(),
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

    // Every candidate is verified with its own `eth_call`, so they go out together: run in turn
    // they would spend the discovery timeout on round trips rather than on work.
    for (&storage_contract, account) in trace.pre_state() {
        // Highest keys first: a mapping slot is a keccak hash and lands near the top of the key
        // order, while a contract's fixed fields sit at 0, 1, 2 and sort to the bottom. Taking
        // the cap from that end reaches the mapping on a token that reads many fixed slots.
        let candidates: Vec<B256> = account
            .storage
            .keys()
            .rev()
            .copied()
            .take(MAX_SLOTS_TO_VERIFY)
            .collect();
        let verdicts = futures::future::join_all(
            candidates
                .iter()
                .map(|slot| slot_matches(provider, token, storage_contract, calldata, *slot)),
        )
        .await;
        for (slot, verdict) in candidates.iter().zip(verdicts) {
            if verdict? {
                return Ok((storage_contract, *slot));
            }
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

async fn slot_matches(
    provider: &RootProvider<Ethereum>,
    token: Address,
    storage_contract: Address,
    calldata: &[u8],
    slot: B256,
) -> Result<bool, DiscoveryError> {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(slot, B256::from(PROBE_SENTINEL));
    let overrides = StateOverride::from_iter([(
        storage_contract,
        AccountOverride { state_diff: Some(state_diff), ..Default::default() },
    )]);
    match provider
        .call(token_call(token, calldata))
        .overrides(overrides)
        .await
    {
        Ok(response) => {
            Ok(response.len() >= 32 && U256::from_be_slice(&response[..32]) == PROBE_SENTINEL)
        }
        // A guarded proxy reverts when its implementation slot is overwritten, which proves the
        // slot is not the mapping. A node that failed to answer proves nothing, so it is reported
        // rather than counted as a miss that would end in "could not identify".
        Err(error) if error.as_error_resp().is_some() => Ok(false),
        Err(error) => Err(DiscoveryError::Rpc(format!(
            "sentinel probe for {token:#x} slot {slot:#x} failed: {error}"
        ))),
    }
}

fn recover_balance_position(slot: B256, holder: Address) -> Option<MappingPosition> {
    for base in 0..=MAX_BASE_SLOT {
        let solidity = MappingPosition::Direct { base, key_order: KeyOrder::Solidity };
        if mapping_slot(holder, solidity) == slot {
            return Some(solidity);
        }
        let vyper = MappingPosition::Direct { base, key_order: KeyOrder::Vyper };
        if mapping_slot(holder, vyper) == slot {
            return Some(vyper);
        }
    }
    let oz = MappingPosition::OpenZeppelinV5Balances;
    (mapping_slot(holder, oz) == slot).then_some(oz)
}

fn recover_allowance_position(
    slot: B256,
    owner: Address,
    spender: Address,
) -> Option<MappingPosition> {
    for base in 0..=MAX_BASE_SLOT {
        let direct = MappingPosition::Direct { base, key_order: KeyOrder::Solidity };
        if allowance_slot(owner, spender, direct) == slot {
            return Some(direct);
        }
        let vyper = MappingPosition::Direct { base, key_order: KeyOrder::Vyper };
        if allowance_slot(owner, spender, vyper) == slot {
            return Some(vyper);
        }
    }
    let oz = MappingPosition::OpenZeppelinV5Allowances;
    (allowance_slot(owner, spender, oz) == slot).then_some(oz)
}

fn mapping_slot(holder: Address, position: MappingPosition) -> B256 {
    match position {
        MappingPosition::Direct { base, key_order: KeyOrder::Solidity } => {
            solidity_mapping(holder, B256::from(U256::from(base)))
        }
        MappingPosition::Direct { base, key_order: KeyOrder::Vyper } => vyper_mapping(holder, base),
        MappingPosition::OpenZeppelinV5Balances => solidity_mapping(holder, OZ_V5_BALANCES_NS),
        MappingPosition::OpenZeppelinV5Allowances => solidity_mapping(holder, OZ_V5_ALLOWANCES_NS),
    }
}

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
        MappingPosition::OpenZeppelinV5Allowances => {
            solidity_mapping(spender, solidity_mapping(owner, OZ_V5_ALLOWANCES_NS))
        }
        MappingPosition::OpenZeppelinV5Balances => {
            solidity_mapping(spender, solidity_mapping(owner, OZ_V5_BALANCES_NS))
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
mod tests {
    use alloy::{
        primitives::{address, Address},
        providers::ProviderBuilder,
    };

    use super::*;

    #[test]
    fn test_recovers_deep_solidity_balance_and_allowance_slots() {
        let owner = Address::repeat_byte(0x11);
        let spender = Address::repeat_byte(0x22);
        let position = MappingPosition::Direct { base: 516, key_order: KeyOrder::Solidity };

        assert_eq!(recover_balance_position(mapping_slot(owner, position), owner), Some(position));
        assert_eq!(
            recover_allowance_position(allowance_slot(owner, spender, position), owner, spender),
            Some(position)
        );
    }

    #[test]
    fn test_recovers_vyper_mapping_slots() {
        let owner = Address::repeat_byte(0x11);
        let spender = Address::repeat_byte(0x22);
        let position = MappingPosition::Direct { base: 17, key_order: KeyOrder::Vyper };

        assert_eq!(recover_balance_position(mapping_slot(owner, position), owner), Some(position));
        assert_eq!(
            recover_allowance_position(allowance_slot(owner, spender, position), owner, spender),
            Some(position)
        );
    }

    #[test]
    fn test_recovers_openzeppelin_v5_mapping_slots() {
        let owner = Address::repeat_byte(0x11);
        let spender = Address::repeat_byte(0x22);
        let balance = MappingPosition::OpenZeppelinV5Balances;
        let allowance = MappingPosition::OpenZeppelinV5Allowances;

        assert_eq!(recover_balance_position(mapping_slot(owner, balance), owner), Some(balance));
        assert_eq!(
            recover_allowance_position(allowance_slot(owner, spender, allowance), owner, spender),
            Some(allowance)
        );
    }

    /// Exercises the exact layouts that motivated the trace-guided path: USDT, whose storage the
    /// sentinel probe could not place, and stETH, whose balance is derived from shares.
    ///
    /// Asserts the property the discovery exists for -- writing the discovered slot changes what
    /// the token reports -- rather than that two slots differ, which two keccak hashes always do.
    /// Requires an endpoint serving `debug_traceCall`, so it stays opt-in.
    #[tokio::test]
    #[ignore = "requires RPC_URL with debug_traceCall support"]
    async fn test_discovers_mainnet_usdt_and_steth_layouts() {
        let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL for the live layout test");
        let provider = ProviderBuilder::default().connect_http(
            rpc_url
                .parse()
                .expect("RPC_URL must be a valid HTTP URL"),
        );
        let holder = address!("0000000000000000000000000000000000000001");
        let spender = address!("0000000000000000000000000000000000000002");

        for token in [address!("dAC17F958D2ee523a2206206994597C13D831ec7"), STETH] {
            let layout = discover_layout(&provider, token, holder, spender)
                .await
                .unwrap_or_else(|error| panic!("{token:#x} layout discovery failed: {error}"));
            let storage = layout.storage_contract();

            let funds = slot_matches(
                &provider,
                token,
                storage,
                &balance_calldata(token, holder),
                layout.balance_slot(holder),
            )
            .await
            .expect("the balance probe answers");
            assert!(funds, "{token:#x}: the discovered balance slot does not set the balance");

            let approves = slot_matches(
                &provider,
                token,
                storage,
                &IERC20LayoutProbe::allowanceCall { owner: holder, spender }.abi_encode(),
                layout.allowance_slot(holder, spender),
            )
            .await
            .expect("the allowance probe answers");
            assert!(
                approves,
                "{token:#x}: the discovered allowance slot does not set the allowance"
            );
        }
    }
}
