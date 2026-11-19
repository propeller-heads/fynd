//! Builds the controller-signed grant that lets a swap through a dual Curve pool pay a lower fee.
//!
//! The pool's exclusive view prices it at the policy's minimum fee, so a swap's simulated output is
//! the most the grant can give. The grant fee brings the output down to the committed amount, and
//! the surplus stays with the pool's LPs.

use alloy::{
    primitives::{Address, B256, U256},
    sol,
    sol_types::{eip712_domain, SolStruct},
};
use num_bigint::BigUint;
use tycho_simulation::{
    evm::protocol::curve::CurvePolicy,
    tycho_common::{models::protocol::ProtocolComponent, Bytes},
};

use crate::{bps, SolveError, Swap};

/// Denominator of a Curve fee.
const FEE_DENOMINATOR: u64 = 10_000_000_000;

sol! {
    /// Mirrors the `Grant` struct of Curve's `FyndPolicy`.
    struct Grant {
        address taker;
        uint8 coin_in;
        uint256 min_amount_in;
        uint256 max_amount_in;
        uint256 fee;
        uint64 nonce;
        uint32 deadline;
    }
}

/// The parts of a grant that the policy checks besides the signature.
pub(crate) struct GrantTerms {
    pub(crate) min_amount_in: U256,
    pub(crate) max_amount_in: U256,
    pub(crate) fee: U256,
    pub(crate) nonce: u64,
    pub(crate) deadline: u32,
}

impl GrantTerms {
    /// Derives the terms for `swap`. The input band is `swap`'s input scaled by
    /// `input_floor_factor_bps` and `input_cap_factor_bps`.
    ///
    /// # Errors
    /// Errors if `swap` has no committed amount or an amount overflows `U256`.
    pub(crate) fn for_swap(
        swap: &Swap,
        policy: &CurvePolicy,
        input_floor_factor_bps: u32,
        input_cap_factor_bps: u32,
        nonce: u64,
        deadline: u32,
    ) -> Result<Self, SolveError> {
        let committed = swap
            .committed_amount_out()
            .ok_or_else(|| {
                SolveError::FailedEncoding("Curve grant leg is missing committed_amount_out".into())
            })?;
        let min_fee = u64::try_from(policy.min_fee())
            .map_err(|_| SolveError::FailedEncoding("Curve policy min_fee overflows u64".into()))?;
        let scale = |factor_bps: u32| {
            to_u256(&(swap.amount_in() * factor_bps / bps::DENOMINATOR), "grant input amount")
        };
        Ok(Self {
            min_amount_in: scale(input_floor_factor_bps)?,
            max_amount_in: scale(input_cap_factor_bps)?,
            fee: U256::from(derive_grant_fee(swap.amount_out(), committed, min_fee)),
            nonce,
            deadline,
        })
    }

    /// Signs the grant and packs it into the swap's `user_data`.
    ///
    /// # Errors
    /// Errors if the policy has no address, the pool's coins do not hold `swap`'s input token, or
    /// signing fails.
    pub(crate) fn sign(
        self,
        swap: &Swap,
        policy: &CurvePolicy,
        taker: &Bytes,
        chain_id: u64,
        signer: &impl alloy::signers::SignerSync,
    ) -> Result<Bytes, SolveError> {
        let grant = Grant {
            taker: to_address(taker, "taker")?,
            coin_in: find_coin_index(swap.protocol_component(), swap.token_in())?,
            min_amount_in: self.min_amount_in,
            max_amount_in: self.max_amount_in,
            fee: self.fee,
            nonce: self.nonce,
            deadline: self.deadline,
        };
        let policy = policy
            .address()
            .ok_or_else(|| SolveError::FailedEncoding("Curve pool has no policy".into()))?;
        let digest = grant_digest(&grant, chain_id, to_address(policy, "policy")?);
        let signature = signer
            .sign_hash_sync(&digest)
            .map_err(|e| SolveError::FailedEncoding(format!("Curve grant signing failed: {e}")))?;

        let mut user_data = Vec::with_capacity(20 + 32 * 3 + 8 + 4 + 65);
        user_data.extend_from_slice(grant.taker.as_slice());
        user_data.extend_from_slice(&grant.min_amount_in.to_be_bytes::<32>());
        user_data.extend_from_slice(&grant.max_amount_in.to_be_bytes::<32>());
        user_data.extend_from_slice(&grant.fee.to_be_bytes::<32>());
        user_data.extend_from_slice(&grant.nonce.to_be_bytes());
        user_data.extend_from_slice(&grant.deadline.to_be_bytes());
        user_data.extend_from_slice(&signature.as_bytes());
        Ok(Bytes::from(user_data))
    }
}

/// The highest fee that still pays out `committed`, given the output `gross` at `min_fee`. Never
/// below `min_fee`, which the policy enforces.
fn derive_grant_fee(gross: &BigUint, committed: &BigUint, min_fee: u64) -> u64 {
    if committed >= gross {
        return min_fee;
    }
    // The output scales with `FEE_DENOMINATOR - fee`, so the fee may rise until the output falls
    // to `committed`. Rounding the kept share up rounds the fee down.
    let kept_share = committed * (FEE_DENOMINATOR - min_fee);
    let kept = (kept_share + gross - 1u32) / gross;
    let fee = BigUint::from(FEE_DENOMINATOR) - kept;
    u64::try_from(fee)
        .unwrap_or(min_fee)
        .max(min_fee)
}

/// Finds `token`'s index in the pool's `coins` attribute.
fn find_coin_index(component: &ProtocolComponent, token: &Bytes) -> Result<u8, SolveError> {
    let coins = component
        .static_attributes
        .get("coins")
        .ok_or_else(|| SolveError::FailedEncoding("Curve pool has no coins attribute".into()))?;
    let coins: Vec<Bytes> = serde_json::from_slice(coins)
        .map_err(|e| SolveError::FailedEncoding(format!("invalid Curve coins attribute: {e}")))?;
    let index = coins
        .iter()
        .position(|coin| coin == token)
        .ok_or_else(|| {
            SolveError::FailedEncoding(format!("Curve pool coins do not hold token {token}"))
        })?;
    u8::try_from(index).map_err(|_| SolveError::FailedEncoding("Curve coin index over u8".into()))
}

/// The EIP-712 hash the policy recovers the controller from.
fn grant_digest(grant: &Grant, chain_id: u64, policy: Address) -> B256 {
    let domain = eip712_domain! {
        name: "FyndPolicy",
        version: "1",
        chain_id: chain_id,
        verifying_contract: policy,
    };
    grant.eip712_signing_hash(&domain)
}

fn to_u256(amount: &BigUint, what: &str) -> Result<U256, SolveError> {
    let bytes = amount.to_bytes_be();
    if bytes.len() > 32 {
        return Err(SolveError::FailedEncoding(format!("{what} overflows U256")));
    }
    Ok(U256::from_be_slice(&bytes))
}

fn to_address(bytes: &Bytes, what: &str) -> Result<Address, SolveError> {
    Address::try_from(bytes.as_ref())
        .map_err(|_| SolveError::FailedEncoding(format!("{what} is not a 20-byte address")))
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::keccak256, signers::local::PrivateKeySigner};
    use rstest::rstest;

    use super::*;

    #[test]
    fn test_grant_type_matches_the_policy() {
        assert_eq!(
            Grant::eip712_encode_type(),
            "Grant(address taker,uint8 coin_in,uint256 min_amount_in,uint256 max_amount_in,\
             uint256 fee,uint64 nonce,uint32 deadline)"
        );
    }

    #[rstest]
    #[case::no_surplus(1_000, 1_000, 1_000_000, 1_000_000)]
    #[case::committed_above_gross(1_000, 1_001, 1_000_000, 1_000_000)]
    #[case::one_percent_surplus(1_000_000, 990_000, 0, 100_000_000)]
    #[case::surplus_above_min_fee(1_000_000, 990_000, 1_000_000, 100_990_000)]
    fn test_derive_grant_fee(
        #[case] gross: u64,
        #[case] committed: u64,
        #[case] min_fee: u64,
        #[case] expected: u64,
    ) {
        let fee = derive_grant_fee(&BigUint::from(gross), &BigUint::from(committed), min_fee);

        assert_eq!(fee, expected);
    }

    /// The taker's output at the grant fee never falls below the committed amount.
    #[rstest]
    #[case(123_456_789, 120_000_000, 1_000_000)]
    #[case(10_u64.pow(18), 9 * 10_u64.pow(17), 3_000_000)]
    fn test_derive_grant_fee_pays_committed(
        #[case] gross: u64,
        #[case] committed: u64,
        #[case] min_fee: u64,
    ) {
        let fee = derive_grant_fee(&BigUint::from(gross), &BigUint::from(committed), min_fee);
        let raw = BigUint::from(gross) * FEE_DENOMINATOR / (FEE_DENOMINATOR - min_fee);
        let paid = raw * (FEE_DENOMINATOR - fee) / FEE_DENOMINATOR;

        assert!(paid >= BigUint::from(committed) - 1u32, "paid {paid} below {committed}");
    }

    /// An independent digest built from the policy's own typehash string and domain fields.
    #[test]
    fn test_grant_digest_matches_manual_encoding() {
        let policy = Address::repeat_byte(0xfd);
        let grant = Grant {
            taker: Address::repeat_byte(0xaa),
            coin_in: 1,
            min_amount_in: U256::from(10),
            max_amount_in: U256::from(20),
            fee: U256::from(30),
            nonce: 7,
            deadline: 1_000,
        };
        let word = |value: U256| value.to_be_bytes::<32>();
        let domain_separator = keccak256(
            [
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,\
                     address verifyingContract)",
                )
                .0,
                keccak256("FyndPolicy").0,
                keccak256("1").0,
                word(U256::from(1)),
                B256::left_padding_from(policy.as_slice()).0,
            ]
            .concat(),
        );
        let struct_hash = keccak256(
            [
                keccak256(
                    "Grant(address taker,uint8 coin_in,uint256 min_amount_in,\
                     uint256 max_amount_in,uint256 fee,uint64 nonce,uint32 deadline)",
                )
                .0,
                B256::left_padding_from(grant.taker.as_slice()).0,
                word(U256::from(1)),
                word(U256::from(10)),
                word(U256::from(20)),
                word(U256::from(30)),
                word(U256::from(7)),
                word(U256::from(1_000)),
            ]
            .concat(),
        );
        let expected = keccak256([&[0x19, 0x01][..], &domain_separator.0, &struct_hash.0].concat());

        assert_eq!(grant_digest(&grant, 1, policy), expected);
    }

    #[test]
    fn test_sign_packs_the_grant() {
        let signer: PrivateKeySigner =
            "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318"
                .parse()
                .expect("key");
        let policy = CurvePolicy::from_attributes(
            &[("policy".to_string(), Bytes::from([0xfd; 20]))]
                .into_iter()
                .collect(),
        )
        .expect("policy");
        let token_in = Bytes::from([0x01; 20]);
        let mut component = ProtocolComponent::default();
        component.static_attributes.insert(
            "coins".to_string(),
            Bytes::from(br#"["0x0202020202020202020202020202020202020202","0x0101010101010101010101010101010101010101"]"#.to_vec()),
        );
        let swap = Swap::new(
            "curve".to_string(),
            "vm:curve".to_string(),
            token_in,
            Bytes::from([0x02; 20]),
            BigUint::from(1_000u32),
            BigUint::from(990u32),
            BigUint::ZERO,
            component,
            Box::new(crate::algorithm::test_utils::MockProtocolSim::default()),
        );
        let terms = GrantTerms {
            min_amount_in: U256::from(900),
            max_amount_in: U256::from(1_500),
            fee: U256::from(5_000_000),
            nonce: 7,
            deadline: 1_000,
        };

        let user_data = terms
            .sign(&swap, &policy, &Bytes::from([0xaa; 20]), 1, &signer)
            .expect("signed");

        assert_eq!(user_data.len(), 193);
        assert_eq!(&user_data[..20], &[0xaa; 20]);
        assert_eq!(&user_data[20..52], &U256::from(900).to_be_bytes::<32>());
        assert_eq!(&user_data[116..124], &7u64.to_be_bytes());
        let grant = Grant {
            taker: Address::repeat_byte(0xaa),
            coin_in: 1,
            min_amount_in: U256::from(900),
            max_amount_in: U256::from(1_500),
            fee: U256::from(5_000_000),
            nonce: 7,
            deadline: 1_000,
        };
        let digest = grant_digest(&grant, 1, Address::repeat_byte(0xfd));
        let signature =
            alloy::primitives::Signature::try_from(&user_data[128..]).expect("signature");
        assert_eq!(
            signature
                .recover_address_from_prehash(&digest)
                .expect("recover"),
            signer.address()
        );
    }
}
