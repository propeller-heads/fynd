//! Builds the controller-signed `user_data` payload for exclusive swaps.
//!
//! An exclusive leg (one carrying a committed amount, [`Swap::committed_amount_out`]) executes
//! through an on-chain extension that charges a per-swap fee so the taker's output tracks the
//! committed amount and the surplus goes to the pool's LPs. The committed amount is best-effort,
//! not a hard floor — the ≥committed guarantee lives upstream in the quote/router and price-guard
//! layer.
//!
//! For Ekubo's `SignedExclusiveSwap` extension the payload is the tycho
//! [`Swap::user_data`](tycho_execution) bytes `fee(8) | meta(32) | minBalanceUpdate(32) |
//! signature(65)`, where `fee` is the pool's config fee (a big-endian u64) that
//! `EkuboV3SwapEncoder` uses to rebuild the hop's poolConfig. The encoder appends the 2-byte
//! `sigLen` before the signature, so this module does not.
//!
//! Requires the Ekubo `user_data` support added in `tycho-execution` 0.338.0 (the workspace pins
//! `>= 0.338.0`). All byte layouts and the EIP-712 digest are mirrored from the Ekubo contracts.

use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{keccak256, Address, B256, U256},
    signers::{local::PrivateKeySigner, SignerSync},
};
use num_bigint::BigUint;
use tycho_simulation::tycho_common::{models::protocol::ProtocolComponent, Bytes};

use crate::{SolveError, Swap};

/// Environment variable holding the pool controller's private key (hex, with or without `0x`).
const ENV_CONTROLLER_KEY: &str = "EXCLUSIVE_SWAP_CONTROLLER_KEY";

/// Default validity window for a signed quote, in seconds. Well under the extension's 30-day cap.
const DEFAULT_DEADLINE_WINDOW_SECS: u32 = 120;

/// Produces controller-signed `user_data` payloads for exclusive swaps.
///
/// Holds the controller key, the chain id (bound into the EIP-712 domain), the nonce source, and
/// the deadline window. Construct it from the environment with [`ExclusiveSwapSigner::from_env`];
/// when the key variable is unset the encoder simply leaves exclusive legs unsigned.
///
/// The extension rejects a nonce it has already seen, so a nonce is `nonce_prefix` (drawn once per
/// process) in the high 32 bits and `nonce_counter` (payloads signed in this process) in the low
/// 32. A restart or a second replica draws its own prefix instead of resigning a spent range.
pub struct ExclusiveSwapSigner {
    signer: PrivateKeySigner,
    chain_id: u64,
    nonce_prefix: u32,
    nonce_counter: AtomicU32,
    deadline_window_secs: u32,
}

impl ExclusiveSwapSigner {
    /// Builds a signer from the `EXCLUSIVE_SWAP_CONTROLLER_KEY` env var: `Ok(None)` when unset
    /// (signing disabled), an error when set but invalid.
    ///
    /// The nonce prefix is random, so no state has to persist across restarts and replicas need no
    /// coordination.
    pub fn from_env(chain_id: u64) -> Result<Option<Self>, SolveError> {
        let Ok(key) = std::env::var(ENV_CONTROLLER_KEY) else {
            return Ok(None);
        };
        let signer = key
            .parse::<PrivateKeySigner>()
            .map_err(|e| {
                SolveError::FailedEncoding(format!("invalid {ENV_CONTROLLER_KEY}: {e}"))
            })?;
        Ok(Some(Self::new(signer, chain_id, rand::random(), DEFAULT_DEADLINE_WINDOW_SECS)))
    }

    /// Creates a signer from explicit parts.
    ///
    /// `nonce_prefix` is the high 32 bits of every nonce handed out; `deadline_window_secs` is
    /// added to the signing-time timestamp to form each payload's deadline.
    pub fn new(
        signer: PrivateKeySigner,
        chain_id: u64,
        nonce_prefix: u32,
        deadline_window_secs: u32,
    ) -> Self {
        Self {
            signer,
            chain_id,
            nonce_prefix,
            nonce_counter: AtomicU32::new(0),
            deadline_window_secs,
        }
    }

    /// Hands out the next unused nonce: `nonce_prefix` in the high 32 bits, the counter in the low
    /// 32.
    ///
    /// # Errors
    /// Errors from the 2³²-th payload on. Wrapping the counter would resign a spent nonce, so the
    /// signer stops instead and a restart draws a fresh prefix.
    fn next_nonce(&self) -> Result<u64, SolveError> {
        let counter = self
            .nonce_counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |counter| counter.checked_add(1))
            .map_err(|_| {
                SolveError::FailedEncoding(
                    "exclusive swap nonce counter is exhausted; restart to draw a new prefix"
                        .to_string(),
                )
            })?;
        Ok((u64::from(self.nonce_prefix) << 32) | u64::from(counter))
    }

    /// Builds the signed `user_data` for one exclusive swap leg.
    ///
    /// The leg must carry a committed amount ([`Swap::committed_amount_out`]); the fee is derived
    /// from the quoted output so the realized output tracks it (best-effort — see the module docs).
    ///
    /// v1 supports only standard ERC-20 pools: `poolId` is rebuilt from the swap's token addresses,
    /// so a native-ETH pool (whose on-chain `PoolKey` uses `address(0)`) would yield a mismatched
    /// signature and revert.
    ///
    /// # Errors
    /// Errors if the leg lacks a committed amount, the component is missing Ekubo pool attributes,
    /// a token address exceeds 32 bytes, the deadline overflows `u32`, the nonce counter is
    /// exhausted, or signing fails.
    pub fn build_user_data(&self, swap: &Swap) -> Result<Bytes, SolveError> {
        let committed = swap
            .committed_amount_out()
            .ok_or_else(|| {
                SolveError::FailedEncoding(
                    "signed swap leg is missing committed_amount_out".to_string(),
                )
            })?;

        let fee = derive_fee_q32(swap.amount_out(), committed);
        let nonce = self.next_nonce()?;
        let deadline = now_unix_secs().saturating_add(u64::from(self.deadline_window_secs));
        let deadline = u32::try_from(deadline).map_err(|_| {
            SolveError::FailedEncoding("signed swap deadline overflows u32".to_string())
        })?;

        // No authorized locker: address(0) leaves the swap usable by any locker (see the
        // extension's `isAuthorized`), matching the reference wiring.
        let meta = signed_swap_meta(deadline, fee, nonce, Address::ZERO);
        let min_balance_update = min_balance_update_accept_any();

        let component = swap.protocol_component();
        let extension = pool_extension(component)?;
        let config = pool_config_word(component)?;
        let (token0, token1) = sorted_tokens(swap.token_in(), swap.token_out());
        let pool_id = pool_id(token0, token1, config)?;

        let digest = eip712_digest(self.chain_id, extension, pool_id, meta, min_balance_update);
        let signature = self
            .signer
            .sign_hash_sync(&digest)
            .map_err(|e| SolveError::FailedEncoding(format!("signed swap signing failed: {e}")))?;

        // `EkuboV3SwapEncoder` reads the leading fee(8) as the pool's config fee to rebuild the
        // hop's poolConfig. Take it from the same `config` word the poolId is derived from (bytes
        // [20..28]) so the executor resolves the pool the signature was made over.
        let config_fee = &config.as_slice()[20..28];

        let mut user_data = Vec::with_capacity(8 + 32 + 32 + 65);
        user_data.extend_from_slice(config_fee);
        user_data.extend_from_slice(meta.as_slice());
        user_data.extend_from_slice(min_balance_update.as_slice());
        user_data.extend_from_slice(&signature.as_bytes());
        Ok(Bytes::from(user_data))
    }
}

/// Packs a `SignedSwapMeta` word: `[255..224]` deadline(u32), `[223..192]` fee(u32, 0.32 fixed
/// point), `[191..128]` nonce(u64), `[127..0]` authorized-locker low 128 bits.
fn signed_swap_meta(deadline: u32, fee: u32, nonce: u64, authorized_locker: Address) -> B256 {
    let mut word = [0u8; 32];
    word[0..4].copy_from_slice(&deadline.to_be_bytes());
    word[4..8].copy_from_slice(&fee.to_be_bytes());
    word[8..16].copy_from_slice(&nonce.to_be_bytes());
    // Low 128 bits of the 20-byte locker address = its least-significant 16 bytes.
    word[16..32].copy_from_slice(&authorized_locker.as_slice()[4..20]);
    B256::from(word)
}

/// Returns the `PoolBalanceUpdate` that accepts any swap output (`delta0 = delta1 = i128::MIN`).
fn min_balance_update_accept_any() -> B256 {
    let mut word = [0u8; 32];
    word[0] = 0x80;
    word[16] = 0x80;
    B256::from(word)
}

/// Derives the extension's 0.32 fixed-point fee so the taker's realized output tracks `committed`.
///
/// The fee is rounded down so the taker always receives at least the committed amount.
fn derive_fee_q32(gross: &BigUint, committed: &BigUint) -> u32 {
    if gross <= committed {
        return 0;
    }
    // The extension charges `ceil(gross · (fee << 32) / 2⁶⁴)`, so the largest fee that never takes
    // more than the surplus above the committed amount is `floor((gross − committed) · 2³² /
    // gross)`. Rounding down biases toward under-capture, keeping the taker at or above it.
    let surplus = gross - committed;
    let scaled = (surplus * BigUint::from(1u64 << 32)) / gross;
    scaled
        .min(BigUint::from(u32::MAX))
        .iter_u32_digits()
        .next()
        .unwrap_or(0)
}

/// Reads the Ekubo extension address from a component's static attributes.
fn pool_extension(component: &ProtocolComponent) -> Result<Address, SolveError> {
    let bytes = attribute(component, "extension")?;
    Address::try_from(bytes).map_err(|_| {
        SolveError::FailedEncoding("extension attribute is not a 20-byte address".to_string())
    })
}

/// Rebuilds the 32-byte `PoolConfig` word: `extension(20) ‖ fee(u64, 8) ‖ pool_type_config(4)`.
fn pool_config_word(component: &ProtocolComponent) -> Result<B256, SolveError> {
    let extension = attribute(component, "extension")?;
    let fee = attribute(component, "fee")?;
    let pool_type_config = attribute(component, "pool_type_config")?;

    if extension.len() != 20 {
        return Err(SolveError::FailedEncoding("extension attribute must be 20 bytes".to_string()));
    }
    if fee.len() != 8 {
        return Err(SolveError::FailedEncoding("fee attribute must be 8 bytes".to_string()));
    }
    if pool_type_config.len() != 4 {
        return Err(SolveError::FailedEncoding(
            "pool_type_config attribute must be 4 bytes".to_string(),
        ));
    }

    let mut word = [0u8; 32];
    word[0..20].copy_from_slice(extension);
    word[20..28].copy_from_slice(fee);
    word[28..32].copy_from_slice(pool_type_config);
    Ok(B256::from(word))
}

/// Computes `poolId = keccak256(token0₃₂ ‖ token1₃₂ ‖ poolConfig₃₂)`.
///
/// Each token is left-padded into a 32-byte word, so a token longer than 32 bytes is rejected
/// rather than panicking on the slice index.
fn pool_id(token0: &[u8], token1: &[u8], config: B256) -> Result<B256, SolveError> {
    if token0.len() > 32 || token1.len() > 32 {
        return Err(SolveError::FailedEncoding(
            "token address exceeds 32 bytes; cannot build poolId".to_string(),
        ));
    }
    let mut buf = [0u8; 96];
    buf[32 - token0.len()..32].copy_from_slice(token0);
    buf[64 - token1.len()..64].copy_from_slice(token1);
    buf[64..96].copy_from_slice(config.as_slice());
    Ok(keccak256(buf))
}

/// Orders two token addresses so `token0 < token1`, matching the on-chain `PoolKey`.
fn sorted_tokens<'a>(token_in: &'a [u8], token_out: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    if token_in <= token_out {
        (token_in, token_out)
    } else {
        (token_out, token_in)
    }
}

/// Computes the EIP-712 digest the `SignedExclusiveSwap` extension recovers the signer from.
fn eip712_digest(
    chain_id: u64,
    extension: Address,
    pool_id: B256,
    meta: B256,
    min_balance_update: B256,
) -> B256 {
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name_hash = keccak256(b"Ekubo SignedExclusiveSwap");
    let version_hash = keccak256(b"1");

    let mut domain = Vec::with_capacity(32 * 5);
    domain.extend_from_slice(domain_typehash.as_slice());
    domain.extend_from_slice(name_hash.as_slice());
    domain.extend_from_slice(version_hash.as_slice());
    domain.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    domain.extend_from_slice(B256::left_padding_from(extension.as_slice()).as_slice());
    let domain_separator = keccak256(&domain);

    let struct_typehash =
        keccak256(b"SignedSwap(bytes32 poolId,uint256 meta,bytes32 minBalanceUpdate)");
    let mut struct_input = Vec::with_capacity(32 * 4);
    struct_input.extend_from_slice(struct_typehash.as_slice());
    struct_input.extend_from_slice(pool_id.as_slice());
    struct_input.extend_from_slice(meta.as_slice());
    struct_input.extend_from_slice(min_balance_update.as_slice());
    let struct_hash = keccak256(&struct_input);

    let mut digest_input = Vec::with_capacity(2 + 64);
    digest_input.extend_from_slice(&[0x19, 0x01]);
    digest_input.extend_from_slice(domain_separator.as_slice());
    digest_input.extend_from_slice(struct_hash.as_slice());
    keccak256(&digest_input)
}

/// Reads a required static attribute as a byte slice.
fn attribute<'a>(component: &'a ProtocolComponent, key: &str) -> Result<&'a [u8], SolveError> {
    component
        .static_attributes
        .get(key)
        .map(AsRef::as_ref)
        .ok_or_else(|| SolveError::FailedEncoding(format!("component missing `{key}` attribute")))
}

/// Current Unix time in seconds, or 0 if the clock is before the epoch.
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use alloy::primitives::{Address as EvmAddress, Signature};
    use chrono::NaiveDateTime;
    use rstest::rstest;
    use tycho_simulation::tycho_common::models::Chain as CommonChain;

    use super::*;
    use crate::algorithm::test_utils::MockProtocolSim;

    const CONTROLLER_KEY: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";
    // SignedExclusiveSwap extension placeholder used by the tycho reference PR.
    const EXTENSION: &str = "0x5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";

    // Reimplements the extension's `computeFee` (ceil(amount * fee / 2^64)) to check the taker is
    // never charged more than the surplus.
    fn compute_fee(amount: u128, fee_x64: u64) -> u128 {
        let numerator = U256::from(amount) * U256::from(fee_x64) + U256::from(u64::MAX);
        u128::try_from(numerator >> 64).expect("fee fits in u128")
    }

    #[test]
    fn test_signed_swap_meta_packs_fields_in_order() {
        let deadline = 0x1122_3344u32;
        let fee = 0x5566_7788u32;
        let nonce = 0x99AA_BBCC_DDEE_FF00u64;
        let locker = EvmAddress::from([0xAB; 20]);

        let meta = signed_swap_meta(deadline, fee, nonce, locker);
        let bytes = meta.as_slice();

        assert_eq!(&bytes[0..4], &deadline.to_be_bytes());
        assert_eq!(&bytes[4..8], &fee.to_be_bytes());
        assert_eq!(&bytes[8..16], &nonce.to_be_bytes());
        // Low 128 bits of the locker = its last 16 bytes.
        assert_eq!(&bytes[16..32], &[0xABu8; 16]);
    }

    #[test]
    fn test_min_balance_update_accepts_any_output() {
        let expected = "8000000000000000000000000000000080000000000000000000000000000000";
        assert_eq!(alloy::hex::encode(min_balance_update_accept_any()), expected);
    }

    #[rstest]
    #[case::committed_equals_gross(1000, 1000)]
    #[case::committed_exceeds_gross(1000, 2000)]
    fn test_derive_fee_q32_without_surplus(#[case] gross: u64, #[case] committed: u64) {
        assert_eq!(derive_fee_q32(&BigUint::from(gross), &BigUint::from(committed)), 0);
    }

    #[rstest]
    #[case::no_surplus(1_000_000, 1_000_000)]
    #[case::surplus_1_percent(1_000_000, 990_000)]
    #[case::surplus_50_percent(1_000_000, 500_000)]
    #[case::near_total_surplus(1_000_000, 1)]
    fn test_derive_fee_q32_never_shorts_taker(#[case] gross: u128, #[case] committed: u128) {
        let fee = derive_fee_q32(&BigUint::from(gross), &BigUint::from(committed));

        let fee_amount = compute_fee(gross, u64::from(fee) << 32);
        assert!(fee_amount <= gross - committed, "capture exceeds surplus");
        assert!(gross - fee_amount >= committed, "taker receives less than committed");
    }

    fn ekubo_component() -> ProtocolComponent {
        let static_attributes = HashMap::from([
            ("extension".to_string(), Bytes::from_str(EXTENSION).unwrap()),
            ("fee".to_string(), Bytes::from(0u64)),
            ("pool_type_config".to_string(), Bytes::from(0u32)),
        ]);
        ProtocolComponent::new(
            "ekubo-signed-pool",
            "ekubo_v3",
            "swap",
            CommonChain::Ethereum,
            vec![],
            vec![],
            static_attributes,
            Default::default(),
            Default::default(),
            NaiveDateTime::default(),
        )
    }

    #[test]
    fn test_pool_config_word_matches_packed_layout() {
        let config = pool_config_word(&ekubo_component()).unwrap();
        let expected = format!("{}{}", &EXTENSION[2..], "0".repeat(24));
        assert_eq!(alloy::hex::encode(config), expected);
    }

    #[test]
    fn test_sorted_tokens_orders_ascending() {
        let low: &[u8] = &[0x11u8; 20];
        let high: &[u8] = &[0x22u8; 20];
        assert_eq!(sorted_tokens(low, high), (low, high));
        assert_eq!(sorted_tokens(high, low), (low, high));
    }

    #[test]
    fn test_pool_id_independent_of_swap_direction() {
        let config = B256::ZERO;
        let low: &[u8] = &[0x11u8; 20];
        let high: &[u8] = &[0x22u8; 20];
        // Swapping token_in/token_out yields the same pool once sorted.
        let (a0, a1) = sorted_tokens(low, high);
        let (b0, b1) = sorted_tokens(high, low);
        assert_eq!(pool_id(a0, a1, config).unwrap(), pool_id(b0, b1, config).unwrap());
    }

    #[test]
    fn test_pool_id_rejects_over_long_token() {
        assert!(pool_id(&[0u8; 33], &[0x22u8; 20], B256::ZERO).is_err());
    }

    #[test]
    fn test_eip712_digest_signature_recovers_signer() {
        let signer: PrivateKeySigner = CONTROLLER_KEY.parse().unwrap();
        let extension = EvmAddress::from_str(EXTENSION).unwrap();
        let digest = eip712_digest(
            1,
            extension,
            keccak256(b"pool"),
            signed_swap_meta(1_000, 42, 7, EvmAddress::ZERO),
            min_balance_update_accept_any(),
        );

        let signature = signer.sign_hash_sync(&digest).unwrap();
        let recovered = signature
            .recover_address_from_prehash(&digest)
            .unwrap();
        assert_eq!(recovered, signer.address());
        // v byte is normalized to 27/28, matching the on-chain `abi.encodePacked(r, s, v)`.
        assert!(matches!(signature.as_bytes()[64], 27 | 28));
    }

    #[test]
    fn test_eip712_digest_matches_independent_oracle() {
        // Known-answer vector: the expected digest was computed independently with `cast keccak`
        // (foundry) from the Ekubo domain/struct definitions, pinning the domain strings, type
        // hashes, field ordering, and the meta packing against a non-alloy Keccak implementation.
        // Inputs: chainId=1, extension=EXTENSION, poolId=0x11..11, deadline=1000, fee=42, nonce=7,
        // locker=0, minBalanceUpdate=accept-any.
        let extension = EvmAddress::from_str(EXTENSION).unwrap();
        let pool_id = B256::from([0x11u8; 32]);
        let meta = signed_swap_meta(1_000, 42, 7, EvmAddress::ZERO);
        let min_bu = min_balance_update_accept_any();

        let digest = eip712_digest(1, extension, pool_id, meta, min_bu);

        let expected =
            B256::from_str("0xd47eb1b9f473ba6fa851d6dee23ab3ae57ee989187256835206411cea3baa0e0")
                .unwrap();
        assert_eq!(digest, expected);
    }

    fn signed_swap(committed: Option<u64>) -> Swap {
        let token_in = tycho_simulation::tycho_common::Bytes::from([0x11u8; 20].as_ref());
        let token_out = tycho_simulation::tycho_common::Bytes::from([0x22u8; 20].as_ref());
        let mut swap = Swap::new(
            "ekubo-signed-pool".to_string(),
            "ekubo_v3".to_string(),
            token_in,
            token_out,
            BigUint::from(1_000_000u64),
            BigUint::from(1_000_000u64),
            BigUint::from(50_000u64),
            ekubo_component(),
            Box::new(MockProtocolSim::default()),
        );
        if let Some(committed) = committed {
            swap.set_committed_amount_out(BigUint::from(committed));
        }
        swap
    }

    #[test]
    fn test_build_user_data_layout() {
        let signer = ExclusiveSwapSigner::new(CONTROLLER_KEY.parse().unwrap(), 1, 0, 120);
        let swap = signed_swap(Some(990_000));

        let user_data = signer.build_user_data(&swap).unwrap();
        let bytes = user_data.as_ref();

        // Offsets mirror tycho `EkuboV3SwapEncoder::parse_signed_user_data`:
        // fee(8) | meta(32) | minBalanceUpdate(32) | signature(65).
        assert_eq!(bytes.len(), 8 + 32 + 32 + 65);
        let config = pool_config_word(swap.protocol_component()).unwrap();
        assert_eq!(&bytes[0..8], &config.as_slice()[20..28]); // pool config fee
        assert_eq!(&bytes[40..72], min_balance_update_accept_any().as_slice());

        // The signature recovers over the digest rebuilt from the payload's meta and minBU.
        let extension = EvmAddress::from_str(EXTENSION).unwrap();
        let meta = B256::from_slice(&bytes[8..40]);
        let min_bu = B256::from_slice(&bytes[40..72]);
        let (token0, token1) = sorted_tokens(swap.token_in(), swap.token_out());
        let pool_id = pool_id(token0, token1, config).unwrap();
        let digest = eip712_digest(1, extension, pool_id, meta, min_bu);

        let signature = Signature::try_from(&bytes[72..]).unwrap();
        assert_eq!(
            signature
                .recover_address_from_prehash(&digest)
                .unwrap(),
            signer.signer.address()
        );
    }

    #[test]
    fn test_build_user_data_requires_committed_amount() {
        let signer = ExclusiveSwapSigner::new(CONTROLLER_KEY.parse().unwrap(), 1, 0, 120);
        assert!(signer
            .build_user_data(&signed_swap(None))
            .is_err());
    }

    /// Reads the nonce out of a `user_data` payload: it lives in meta bits [191..128] = meta bytes
    /// [8..16], and meta starts after the fee(8) prefix.
    fn payload_nonce(user_data: &Bytes) -> u64 {
        u64::from_be_bytes(
            user_data.as_ref()[16..24]
                .try_into()
                .unwrap(),
        )
    }

    #[test]
    fn test_nonce_increments_per_payload() {
        let signer = ExclusiveSwapSigner::new(CONTROLLER_KEY.parse().unwrap(), 42, 7, 120);
        let swap = signed_swap(Some(990_000));

        let first = signer.build_user_data(&swap).unwrap();
        let second = signer.build_user_data(&swap).unwrap();

        assert_eq!(payload_nonce(&first), 7 << 32);
        assert_eq!(payload_nonce(&second), (7 << 32) + 1);
    }

    #[test]
    fn test_nonce_ranges_disjoint_across_prefixes() {
        let key: PrivateKeySigner = CONTROLLER_KEY.parse().unwrap();
        let swap = signed_swap(Some(990_000));
        // Two processes (a restart, or a second replica) each draw their own prefix.
        let first = ExclusiveSwapSigner::new(key.clone(), 42, 1, 120);
        let second = ExclusiveSwapSigner::new(key, 42, 2, 120);

        let mut nonces = Vec::new();
        for _ in 0..4 {
            nonces.push(payload_nonce(&first.build_user_data(&swap).unwrap()));
            nonces.push(payload_nonce(&second.build_user_data(&swap).unwrap()));
        }

        let unique: std::collections::HashSet<u64> = nonces.iter().copied().collect();
        assert_eq!(unique.len(), nonces.len());
    }

    #[test]
    fn test_exhausted_nonce_counter() {
        let signer = ExclusiveSwapSigner::new(CONTROLLER_KEY.parse().unwrap(), 42, 1, 120);
        let swap = signed_swap(Some(990_000));
        signer
            .nonce_counter
            .store(u32::MAX, Ordering::Relaxed);

        // The counter must stop rather than wrap onto nonces this prefix already spent.
        assert!(signer.build_user_data(&swap).is_err());
        assert!(signer.build_user_data(&swap).is_err());
    }
}
