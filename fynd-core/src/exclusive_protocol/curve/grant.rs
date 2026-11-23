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
