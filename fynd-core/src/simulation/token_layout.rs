//! Trace-guided ERC-20 storage-layout discovery.
//!
//! State overrides only help when they land on the slots a token actually reads. This module
//! traces the token's read-only access, validates the observed slot with a sentinel override,
//! then records the keccak256 inputs the token hashed to reach that slot. Hashing the same inputs
//! with another holder in place gives that holder's slot, so one discovery funds any account.
//! Recording the inputs, rather than matching the slot against a list of known conventions,
//! covers Solidity and Vyper mappings at any base, namespaced storage (ERC-7201), Solady's seeded
//! slots, and proxies whose storage lives elsewhere.

use alloy::{
    eips::BlockId,
    hex,
    network::Ethereum,
    primitives::{address, keccak256, map::B256HashMap, Address, Bytes, TxKind, B256, U256},
    providers::{ext::DebugApi, Provider, RootProvider},
    rpc::{
        json_rpc::ErrorPayload,
        types::{
            state::{AccountOverride, StateOverride},
            trace::geth::{
                DefaultFrame, GethDebugTracingCallOptions, GethDebugTracingOptions,
                GethDefaultTracingOptions, PreStateConfig, StructLog,
            },
            TransactionRequest,
        },
    },
    sol,
    sol_types::SolCall,
};

/// Slots sentinel-verified per probe, across every account the trace touched.
///
/// Each one costs an `eth_call`, and they all run against the layout-discovery timeout. The bound
/// covers the whole probe rather than one account, so a proxy whose read spans several accounts
/// costs no more than a token that keeps everything in one.
const MAX_SLOTS_TO_VERIFY: usize = 48;
/// Longest keccak256 input kept from the trace.
///
/// A mapping slot hashes one or two keys and a base, 96 bytes at most. Longer inputs are string
/// or code hashes, which never key a balance, so they are dropped rather than copied.
const MAX_PREIMAGE_LEN: usize = 256;
/// The holder, or allowance owner, discovery traces with.
///
/// The template finds a key by searching the hashed bytes for it, so the probe keys must not
/// appear there by chance. A short address such as `0x…01` matches the zero padding of a base
/// word, so these are the last 20 bytes of `keccak256("fynd.token_layout.probe_owner")` and of
/// `keccak256("fynd.token_layout.probe_spender")`.
const PROBE_OWNER: Address = address!("0x24cd62e153ebf887b35fc274b28a89d3ba3c06c8");
/// The allowance spender discovery traces with.
const PROBE_SPENDER: Address = address!("0x589adb4c640194bc15370fdce84fca1957accd9f");
/// A value that survives common packed-balance flags and narrow integer casts.
const PROBE_SENTINEL: U256 = U256::from_limbs([0xdead_beef_cafe_babe, 0, 0, 0]);

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

/// What one byte range of a recorded keccak256 input is replaced with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Substitution {
    /// The mapping key at this index: the holder of a balance, or the owner then the spender of
    /// an allowance.
    Key(usize),
    /// The output of the previous hash in the chain, as a nested mapping hashes its outer key.
    PreviousHash,
}

/// One keccak256 input, with the byte offsets that change from one holder to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HashStep {
    preimage: Vec<u8>,
    substitutions: Vec<(usize, Substitution)>,
}

/// The chain of keccak256 inputs a token hashes to reach the slot of `KEYS` mapping keys.
///
/// Recorded from one traced read, then replayed with other keys to find their slots.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SlotTemplate<const KEYS: usize> {
    steps: Vec<HashStep>,
}

impl<const KEYS: usize> SlotTemplate<KEYS> {
    /// Records the hash chain that yields `slot`, from the keccak256 inputs a traced call hashed.
    ///
    /// Walks back from the hash equal to `slot`: each input may hold the keys and the output of
    /// one earlier hash, which is the next link. Returns `None` when no traced hash yields the
    /// slot, when a key appears in no link, or when one input holds two different earlier hashes.
    fn record(preimages: &[Vec<u8>], slot: B256, keys: &[Address; KEYS]) -> Option<Self> {
        let hashes: Vec<B256> = preimages
            .iter()
            .map(keccak256)
            .collect();
        let mut current = hashes
            .iter()
            .rposition(|&hash| hash == slot)?;
        let mut steps = Vec::new();
        loop {
            let (step, inner) = record_step(&preimages[current], keys, &hashes[..current])?;
            steps.push(step);
            match inner {
                Some(earlier) => current = earlier,
                None => break,
            }
        }
        steps.reverse();
        // A key the chain never hashes would replay the probe key's slot for every real one.
        let every_key_hashed = (0..KEYS).all(|index| {
            steps.iter().any(|step| {
                step.substitutions
                    .iter()
                    .any(|&(_, substitution)| substitution == Substitution::Key(index))
            })
        });
        every_key_hashed.then_some(Self { steps })
    }

    /// The slot these keys map to, in the order the template was recorded with.
    fn slot(&self, keys: &[Address; KEYS]) -> B256 {
        let mut previous = B256::ZERO;
        for step in &self.steps {
            let mut preimage = step.preimage.clone();
            for &(offset, substitution) in &step.substitutions {
                let bytes = match substitution {
                    Substitution::Key(index) => keys[index].as_slice(),
                    Substitution::PreviousHash => previous.as_slice(),
                };
                preimage[offset..offset + bytes.len()].copy_from_slice(bytes);
            }
            previous = keccak256(&preimage);
        }
        previous
    }
}

/// Marks where the keys and at most one earlier hash sit in one keccak256 input.
///
/// Returns the step and the index of the earlier hash, or `None` when the input holds two
/// different earlier hashes and the chain would be ambiguous.
fn record_step(
    preimage: &[u8],
    keys: &[Address],
    earlier_hashes: &[B256],
) -> Option<(HashStep, Option<usize>)> {
    let mut substitutions = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        for offset in offsets_of(preimage, key.as_slice()) {
            substitutions.push((offset, Substitution::Key(index)));
        }
    }
    // The latest index of each hash: a token that hashes the same input twice produced one value.
    let mut inner: Option<(usize, B256)> = None;
    for (index, &hash) in earlier_hashes.iter().enumerate() {
        if offsets_of(preimage, hash.as_slice())
            .next()
            .is_none()
        {
            continue;
        }
        if inner.is_some_and(|(_, found)| found != hash) {
            return None;
        }
        inner = Some((index, hash));
    }
    if let Some((_, hash)) = inner {
        for offset in offsets_of(preimage, hash.as_slice()) {
            substitutions.push((offset, Substitution::PreviousHash));
        }
    }
    let step = HashStep { preimage: preimage.to_vec(), substitutions };
    Some((step, inner.map(|(index, _)| index)))
}

fn offsets_of<'a>(haystack: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(move |(_, window)| *window == needle)
        .map(|(offset, _)| offset)
}

/// The slots needed to fund and approve one simulated token input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenLayout {
    storage_contract: Address,
    balance: SlotTemplate<1>,
    allowance: SlotTemplate<2>,
}

impl TokenLayout {
    /// Contract whose state holds this token's balances and allowances.
    ///
    /// A proxy keeps them somewhere other than the address the swap calls, so an override goes to
    /// this contract rather than to the token.
    pub fn storage_contract(&self) -> Address {
        self.storage_contract
    }

    /// Slot holding one holder's balance, or its share balance on a rebasing token.
    pub fn balance_slot(&self, holder: Address) -> B256 {
        self.balance.slot(&[holder])
    }

    /// Slot holding what one owner has approved one spender to spend.
    pub fn allowance_slot(&self, owner: Address, spender: Address) -> B256 {
        self.allowance.slot(&[owner, spender])
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

/// Resolves the storage a token reads, so an override can fund and approve any account.
pub async fn discover_layout(
    provider: &RootProvider<Ethereum>,
    token: Address,
) -> Result<TokenLayout, DiscoveryError> {
    let (storage_contract, balance) = discover_balance(provider, token).await?;

    let allowance_calldata =
        IERC20LayoutProbe::allowanceCall { owner: PROBE_OWNER, spender: PROBE_SPENDER }
            .abi_encode();
    let (allowance_contract, allowance) =
        find_mapping(provider, token, &allowance_calldata, &[PROBE_OWNER, PROBE_SPENDER]).await?;
    if allowance_contract != storage_contract {
        return Err(DiscoveryError::Unsupported(format!(
            "token {token:#x} stores balance and allowance in different contracts ({storage_contract:#x}, {allowance_contract:#x})"
        )));
    }

    Ok(TokenLayout { storage_contract, balance, allowance })
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
) -> Result<(Address, SlotTemplate<1>), DiscoveryError> {
    let probes = [
        ("balanceOf", IERC20LayoutProbe::balanceOfCall { account: PROBE_OWNER }.abi_encode()),
        ("sharesOf", ISharesToken::sharesOfCall { account: PROBE_OWNER }.abi_encode()),
    ];
    // Every view's reason is kept: most tokens have no `sharesOf`, so its failure alone would
    // hide why `balanceOf` did not resolve.
    let mut failures = Vec::new();
    for (view, calldata) in probes {
        let reason = match find_mapping(provider, token, &calldata, &[PROBE_OWNER]).await {
            Ok(found) => return Ok(found),
            // A node that refused to answer says nothing about the token, so it ends discovery
            // rather than sending the caller on to a view this token may not even have.
            Err(error @ DiscoveryError::Rpc(_)) => return Err(error),
            Err(DiscoveryError::Unsupported(reason)) => reason,
        };
        failures.push(format!("{view}: {reason}"));
    }
    Err(DiscoveryError::Unsupported(failures.join("; ")))
}

/// Finds the slot a read-only call keys on, then records how the call hashed its keys into it.
async fn find_mapping<const KEYS: usize>(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
    keys: &[Address; KEYS],
) -> Result<(Address, SlotTemplate<KEYS>), DiscoveryError> {
    // The opcode trace goes first: a node that cannot serve it fails before the probes spend
    // their calls.
    let preimages = trace_hash_preimages(provider, token, calldata).await?;
    let AccessedSlot { storage_contract, slot: observed } =
        find_accessed_slot(provider, token, calldata).await?;
    let template = SlotTemplate::record(&preimages, observed, keys).ok_or_else(|| {
        DiscoveryError::Unsupported(format!(
            "could not recover the mapping for {token:#x}: no traced keccak256 of the keys yields slot {observed:#x}"
        ))
    })?;
    Ok((storage_contract, template))
}

/// The slot a read-only call depends on, as the sentinel probe found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AccessedSlot {
    storage_contract: Address,
    slot: B256,
}

/// Finds the slot a read-only call depends on, by overwriting each slot it touched in turn.
async fn find_accessed_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
) -> Result<AccessedSlot, DiscoveryError> {
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
            return Ok(AccessedSlot { storage_contract, slot });
        }
    }
    Err(DiscoveryError::Unsupported(format!(
        "could not identify a balance or allowance storage slot for {token:#x}"
    )))
}

/// The keccak256 inputs a read-only call hashed, in execution order.
///
/// Read from the opcode trace, which carries the stack and memory at each `KECCAK256`.
async fn trace_hash_preimages(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
) -> Result<Vec<Vec<u8>>, DiscoveryError> {
    let config = GethDefaultTracingOptions::default()
        .enable_memory()
        .disable_storage()
        .disable_return_data();
    let options = GethDebugTracingOptions { config, ..Default::default() };
    let frame: DefaultFrame = provider
        .debug_trace_call_as(
            token_call(token, calldata),
            BlockId::latest(),
            GethDebugTracingCallOptions::new(options),
        )
        .await
        .map_err(|error| {
            DiscoveryError::Rpc(format!(
                "debug_traceCall opcode probe for {token:#x} failed: {error}"
            ))
        })?;

    let mut preimages = Vec::new();
    for log in &frame.struct_logs {
        if log.op != "KECCAK256" && log.op != "SHA3" {
            continue;
        }
        let preimage = read_hash_input(log).map_err(|reason| {
            DiscoveryError::Rpc(format!(
                "opcode trace for {token:#x} is unreadable at pc {}: {reason}",
                log.pc
            ))
        })?;
        preimages.extend(preimage);
    }
    Ok(preimages)
}

/// The bytes one `KECCAK256` step hashes, or `None` for an input too long to key a mapping.
fn read_hash_input(log: &StructLog) -> Result<Option<Vec<u8>>, String> {
    let Some([.., size, offset]) = log.stack.as_deref() else {
        return Err("the step carries no offset and size on its stack".into());
    };
    let (Ok(offset), Ok(size)) = (usize::try_from(*offset), usize::try_from(*size)) else {
        return Ok(None);
    };
    if size == 0 {
        return Ok(Some(Vec::new()));
    }
    if size > MAX_PREIMAGE_LEN {
        return Ok(None);
    }
    // A node that ignored `enableMemory` sends no memory, which would read as zeros and record
    // the wrong input.
    let Some(words) = log.memory.as_deref() else {
        return Err("the node sent no memory; it may not honour enableMemory".into());
    };
    if offset > words.len() * 32 {
        return Ok(None);
    }
    // Only the words the input spans are decoded: a proxy's memory runs to kilobytes per step.
    let first_word = offset / 32;
    let last_word = (offset + size)
        .div_ceil(32)
        .min(words.len());
    let mut spanned = Vec::with_capacity((last_word - first_word) * 32);
    for word in &words[first_word..last_word] {
        let bytes = hex::decode(word.trim_start_matches("0x"))
            .map_err(|error| format!("memory word {word:?} is not hex: {error}"))?;
        if bytes.len() != 32 {
            return Err(format!("memory word {word:?} is not 32 bytes"));
        }
        spanned.extend(bytes);
    }
    // Reading past the end expands memory with zeros, so the tail the trace did not show is zero.
    let start = offset % 32;
    spanned.resize(spanned.len().max(start + size), 0);
    Ok(Some(spanned[start..start + size].to_vec()))
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

/// Layouts and mocked discovery answers shared by this module's tests and the simulator's.
#[cfg(test)]
pub(crate) mod fixtures {
    use alloy::{
        hex,
        primitives::{keccak256, Address, Bytes, B256, U256},
        transports::mock::Asserter,
    };

    use super::{SlotTemplate, TokenLayout, PROBE_OWNER, PROBE_SENTINEL, PROBE_SPENDER};

    /// A key as a Solidity mapping hashes it: left-padded to one word.
    pub(crate) fn padded(key: Address) -> [u8; 32] {
        B256::left_padding_from(key.as_slice()).0
    }

    /// A small mapping base as the word a Solidity mapping hashes.
    pub(crate) fn base_word(base: u16) -> [u8; 32] {
        U256::from(base).to_be_bytes()
    }

    /// The keccak256 inputs a Solidity mapping at `base` hashes, for one key or for an owner
    /// and a spender.
    pub(crate) fn solidity_preimages(base: [u8; 32], keys: &[Address]) -> Vec<Vec<u8>> {
        let mut preimages = Vec::new();
        let mut outer = base;
        for &key in keys {
            let preimage = [padded(key), outer].concat();
            outer = keccak256(&preimage).0;
            preimages.push(preimage);
        }
        preimages
    }

    fn record<const KEYS: usize>(base: u16, keys: &[Address; KEYS]) -> SlotTemplate<KEYS> {
        let preimages = solidity_preimages(base_word(base), keys);
        let slot = keccak256(
            preimages
                .last()
                .expect("a mapping hashes at least once"),
        );
        SlotTemplate::record(&preimages, slot, keys).expect("a Solidity mapping records")
    }

    /// A layout recorded from a Solidity token, as discovery records it.
    pub(crate) fn solidity_layout(
        contract: Address,
        balance_base: u16,
        allowance_base: u16,
    ) -> TokenLayout {
        TokenLayout {
            storage_contract: contract,
            balance: record(balance_base, &[PROBE_OWNER]),
            allowance: record(allowance_base, &[PROBE_OWNER, PROBE_SPENDER]),
        }
    }

    /// A prestate trace naming one account and the slots its read touched.
    pub(crate) fn prestate(contract: Address, slots: &[B256]) -> serde_json::Value {
        let storage: serde_json::Map<String, serde_json::Value> = slots
            .iter()
            .map(|slot| (format!("{slot:#x}"), serde_json::json!(format!("{:#x}", B256::ZERO))))
            .collect();
        serde_json::json!({ format!("{contract:#x}"): { "storage": storage } })
    }

    /// An opcode trace that hashes each input in turn, each one written at the start of memory.
    pub(crate) fn hash_trace(preimages: &[Vec<u8>]) -> serde_json::Value {
        let struct_logs: Vec<serde_json::Value> = preimages
            .iter()
            .map(|preimage| {
                let mut memory = preimage.clone();
                memory.resize(preimage.len().div_ceil(32) * 32, 0);
                let words: Vec<String> = memory
                    .chunks(32)
                    .map(hex::encode)
                    .collect();
                serde_json::json!({
                    "pc": 0, "op": "KECCAK256", "gas": 0, "gasCost": 0, "depth": 1,
                    "stack": [format!("{:#x}", preimage.len()), "0x0"],
                    "memory": words,
                })
            })
            .collect();
        serde_json::json!({
            "failed": false, "gas": 0, "returnValue": "0x", "structLogs": struct_logs
        })
    }

    /// A call answer that reports the sentinel as written.
    pub(crate) fn sentinel_response() -> Bytes {
        Bytes::from(B256::from(PROBE_SENTINEL).to_vec())
    }

    /// Queues the answers to one mapping's probes, in the order discovery asks for them: the
    /// opcode trace, the prestate trace, then the sentinel probe.
    pub(crate) fn push_mapping(
        asserter: &Asserter,
        contract: Address,
        slot: B256,
        preimages: &[Vec<u8>],
    ) {
        asserter.push_success(&hash_trace(preimages));
        asserter.push_success(&prestate(contract, &[slot]));
        asserter.push_success(&sentinel_response());
    }

    /// Queues the answers to a full discovery of a Solidity token at `contract`.
    pub(crate) fn push_solidity_discovery(
        asserter: &Asserter,
        contract: Address,
        balance_base: u16,
        allowance_base: u16,
    ) {
        let balance = solidity_preimages(base_word(balance_base), &[PROBE_OWNER]);
        let allowance =
            solidity_preimages(base_word(allowance_base), &[PROBE_OWNER, PROBE_SPENDER]);
        for preimages in [balance, allowance] {
            let slot = keccak256(
                preimages
                    .last()
                    .expect("a mapping hashes at least once"),
            );
            push_mapping(asserter, contract, slot, &preimages);
        }
    }

    /// Queues the answers to a discovery where both balance views read a slot that moves the
    /// answer but that no traced hash yields, so the token is unsupported.
    pub(crate) fn push_unrecoverable_discovery(asserter: &Asserter, contract: Address) {
        for _ in ["balanceOf", "sharesOf"] {
            push_mapping(asserter, contract, B256::repeat_byte(0x99), &[]);
        }
    }
}

#[cfg(test)]
#[path = "../tests/simulation/token_layout.rs"]
mod tests;
