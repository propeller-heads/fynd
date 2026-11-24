//! Signs the grant that lets a swap through a dual Curve pool pay a lower fee.
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

use crate::{
    bps,
    encoding::exclusive_swap::{input_floor_factor_bps, ExclusiveSwapSigner},
    SolveError, Swap,
};

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

/// Input the grant allows above the quoted input, in basis points.
///
/// A leg after the first hop swaps whatever the hops before it deliver, so its input drifts with
/// every pool move between quoting and execution, and a tight cap would revert ordinary trades. At
/// 50% the cap still stops a grant being spent at a multiple of the size it was quoted for.
const INPUT_CAP_TOLERANCE_BPS: u32 = 5_000;

/// The policy's minimum fee, when it is a valid Curve fee: at most `FEE_DENOMINATOR`.
pub(super) fn min_fee(policy: &CurvePolicy) -> Option<u64> {
    u64::try_from(policy.min_fee())
        .ok()
        .filter(|min_fee| *min_fee <= FEE_DENOMINATOR)
}

/// Signs a grant for `swap` through a dual Curve pool under `policy`, and returns it packed as the
/// swap's `user_data`.
///
/// `taker` is the order's sender; the executor accepts the grant when `taker` is the router's
/// caller or `tx.origin`. The grant's input band is the swap's input scaled down by `slippage` and
/// up by `INPUT_CAP_TOLERANCE_BPS`, and its fee brings the output at the policy's minimum fee down
/// to the swap's committed amount.
///
/// # Errors
/// Errors if `swap` has no committed amount, the policy's `min_fee` is above `FEE_DENOMINATOR`, the
/// policy has no address, the taker or policy is not a 20-byte address, the pool's `coins`
/// attribute is missing, malformed or does not hold `swap`'s input token, an amount overflows
/// `U256`, the nonce counter is exhausted, or signing fails.
pub(super) fn sign_grant(
    signer: &ExclusiveSwapSigner,
    swap: &Swap,
    policy: &CurvePolicy,
    taker: &Bytes,
    slippage: f64,
) -> Result<Bytes, SolveError> {
    let committed = swap
        .committed_amount_out()
        .ok_or_else(|| {
            SolveError::FailedEncoding("Curve grant leg is missing committed_amount_out".into())
        })?;
    let min_fee = min_fee(policy).ok_or_else(|| {
        SolveError::FailedEncoding(format!(
            "Curve policy min_fee {} is above the fee denominator {FEE_DENOMINATOR}",
            policy.min_fee()
        ))
    })?;
    let scale_input = |factor_bps: u32| {
        to_u256(&bps::scale_truncating(swap.amount_in(), factor_bps), "grant input amount")
    };
    let grant = Grant {
        taker: to_address(taker, "taker")?,
        coin_in: find_coin_index(swap.protocol_component(), swap.token_in())?,
        min_amount_in: scale_input(input_floor_factor_bps(slippage))?,
        max_amount_in: scale_input(bps::DENOMINATOR.saturating_add(INPUT_CAP_TOLERANCE_BPS))?,
        fee: U256::from(derive_grant_fee(swap.amount_out(), committed, min_fee)?),
        nonce: signer.next_nonce()?,
        deadline: signer.next_deadline()?,
    };
    let policy = policy
        .address()
        .ok_or_else(|| SolveError::FailedEncoding("Curve pool has no policy".into()))?;
    let digest = grant_digest(&grant, signer.chain_id(), to_address(policy, "policy")?);
    let signature = signer.sign_hash(&digest)?;

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

/// The highest fee that still pays out `committed`, given the output `gross` at `min_fee`. Never
/// below `min_fee`, which the policy enforces. `min_fee` must be at most `FEE_DENOMINATOR`.
///
/// # Errors
/// Errors if the fee does not fit in a `u64`, which a `min_fee` within the denominator rules out.
fn derive_grant_fee(gross: &BigUint, committed: &BigUint, min_fee: u64) -> Result<u64, SolveError> {
    if committed >= gross {
        return Ok(min_fee);
    }
    // The output scales with `FEE_DENOMINATOR - fee`, so the fee may rise until the output falls
    // to `committed`. Rounding the kept share up rounds the fee down.
    let kept_share = committed * (FEE_DENOMINATOR - min_fee);
    let kept = (kept_share + gross - 1u32) / gross;
    let fee = BigUint::from(FEE_DENOMINATOR) - kept;
    u64::try_from(&fee)
        .map_err(|_| SolveError::FailedEncoding(format!("Curve grant fee {fee} overflows u64")))
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
    u8::try_from(index).map_err(|_| {
        SolveError::FailedEncoding(format!("Curve coin index {index} does not fit in u8"))
    })
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
    fn test_grant_eip712_type() {
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
        let fee = derive_grant_fee(&BigUint::from(gross), &BigUint::from(committed), min_fee)
            .expect("fee fits in u64");

        assert_eq!(fee, expected);
    }

    /// The taker's output at the grant fee is at least the committed amount, less one unit of
    /// rounding.
    #[rstest]
    #[case(123_456_789, 120_000_000, 1_000_000)]
    #[case(10_u64.pow(18), 9 * 10_u64.pow(17), 3_000_000)]
    fn test_derive_grant_fee_paid_output(
        #[case] gross: u64,
        #[case] committed: u64,
        #[case] min_fee: u64,
    ) {
        let fee = derive_grant_fee(&BigUint::from(gross), &BigUint::from(committed), min_fee)
            .expect("fee fits in u64");
        let raw = BigUint::from(gross) * FEE_DENOMINATOR / (FEE_DENOMINATOR - min_fee);
        let paid = raw * (FEE_DENOMINATOR - fee) / FEE_DENOMINATOR;

        assert!(paid >= BigUint::from(committed) - 1u32, "paid {paid} below {committed}");
    }

    /// `grant_digest` matches a digest hashed by hand from the policy's typehash string and EIP-712
    /// domain fields.
    #[test]
    fn test_grant_digest() {
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

    const SIGNER_KEY: &str = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";

    /// A swap of 1_000 of `0x01..` for 990 of `0x02..`, committed to 980, through a pool whose
    /// coins are `0x02..` then `0x01..`.
    fn grant_swap() -> Swap {
        let mut component = ProtocolComponent::default();
        component.static_attributes.insert(
            "coins".to_string(),
            Bytes::from(br#"["0x0202020202020202020202020202020202020202","0x0101010101010101010101010101010101010101"]"#.to_vec()),
        );
        let mut swap = Swap::new(
            "curve".to_string(),
            "vm:curve".to_string(),
            Bytes::from([0x01; 20]),
            Bytes::from([0x02; 20]),
            BigUint::from(1_000u32),
            BigUint::from(990u32),
            BigUint::ZERO,
            component,
            Box::new(crate::algorithm::test_utils::MockProtocolSim::default()),
        );
        swap.set_committed_amount_out(BigUint::from(980u32));
        swap
    }

    fn grant_policy() -> CurvePolicy {
        crate::exclusive_protocol::test_curve::grant_policy([0xc0; 20], false)
    }

    #[test]
    fn test_sign_grant() {
        let key: PrivateKeySigner = SIGNER_KEY.parse().expect("key");
        let signer = ExclusiveSwapSigner::new(key.clone(), 1, 3, 120, Address::repeat_byte(0x77));
        let taker = Bytes::from([0xaa; 20]);
        let swap = grant_swap();
        let policy = grant_policy();

        let user_data = sign_grant(&signer, &swap, &policy, &taker, 0.01).expect("signed");

        let word = |range: std::ops::Range<usize>| U256::from_be_slice(&user_data[range]);
        let deadline = u32::from_be_bytes(
            user_data[124..128]
                .try_into()
                .expect("4 bytes"),
        );
        let expected = Grant {
            taker: Address::repeat_byte(0xaa),
            coin_in: 1,
            min_amount_in: U256::from(990),
            max_amount_in: U256::from(1_500),
            fee: U256::from(
                derive_grant_fee(&BigUint::from(990u32), &BigUint::from(980u32), 1_000_000)
                    .expect("fee"),
            ),
            nonce: 3 << 32,
            deadline,
        };
        assert_eq!(user_data.len(), 193);
        assert_eq!(&user_data[..20], expected.taker.as_slice());
        assert_eq!(word(20..52), expected.min_amount_in);
        assert_eq!(word(52..84), expected.max_amount_in);
        assert_eq!(word(84..116), expected.fee);
        assert_eq!(&user_data[116..124], &expected.nonce.to_be_bytes());
        let digest = grant_digest(&expected, 1, Address::repeat_byte(0xfd));
        let signature =
            alloy::primitives::Signature::try_from(&user_data[128..]).expect("signature");
        assert_eq!(
            signature
                .recover_address_from_prehash(&digest)
                .expect("recover"),
            key.address()
        );
    }

    #[test]
    fn test_sign_grant_min_fee_above_denominator() {
        let key: PrivateKeySigner = SIGNER_KEY.parse().expect("key");
        let signer = ExclusiveSwapSigner::new(key, 1, 3, 120, Address::repeat_byte(0x77));
        let policy = crate::exclusive_protocol::test_curve::grant_policy_with_min_fee(
            [0xc0; 20],
            false,
            FEE_DENOMINATOR + 1,
        );

        let result = sign_grant(&signer, &grant_swap(), &policy, &Bytes::from([0xaa; 20]), 0.01);

        assert!(matches!(result, Err(SolveError::FailedEncoding(_))));
    }
}
