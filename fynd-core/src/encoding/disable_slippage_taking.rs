//! Signs the Tycho Router's `ClientFee` payload for disable-slippage-taking encoding.
//!
//! The encoder attaches server-signed, zero-fee `ClientFeeParams` to the transaction, so the
//! on-chain FeeCalculator resolves this signer's address as the fee client and applies its
//! positive-slippage exemption. The exemption itself is FeeCalculator state; this module only
//! produces the attribution signature. The router recovers the signature against
//! `clientFeeReceiver`, so the signer's address doubles as the receiver; with a zero fee and
//! zero vault contribution, no funds ever move to it.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::{Address, U256},
    signers::{local::PrivateKeySigner, SignerSync},
    sol,
    sol_types::{eip712_domain, SolStruct},
};
use tycho_simulation::tycho_common::Bytes;

use crate::SolveError;

/// Environment variable holding the disable-slippage-taking signing key (hex, with or without
/// `0x`).
pub(crate) const ENV_DISABLE_SLIPPAGE_TAKING_KEY: &str = "DISABLE_SLIPPAGE_TAKING_SIGNER_KEY";

/// Validity window for a signed `ClientFee`, in seconds. The router rejects the params after
/// `deadline`, so the window must outlive quote delivery, user wallet signing, and submission.
const DEADLINE_WINDOW_SECS: u64 = 600;

sol! {
    /// Mirror of `TychoRouterV3.CLIENT_FEE_TYPEHASH`: the field names, types, and order must
    /// match the contract exactly, or the recovered signer changes and the router rejects the
    /// params.
    struct ClientFee {
        uint32 clientFeeBps;
        address clientFeeReceiver;
        uint256 maxClientContribution;
        uint256 deadline;
        uint256 amountIn;
        address tokenIn;
        address tokenOut;
        uint256 expectedAmountOut;
        uint256 minAmountOut;
        address receiver;
        bytes swaps;
    }
}

/// The swap-intent fields the `ClientFee` signature must cover, taken verbatim from the encoded
/// calldata: the router recomputes the signing hash from its calldata arguments, so any difference
/// (e.g. the native-ETH sentinel address) invalidates the signature.
pub struct SwapIntent<'a> {
    /// Unix timestamp after which the router rejects the params.
    pub deadline: u64,
    /// Exact input amount.
    pub amount_in: U256,
    /// Input token as encoded in calldata.
    pub token_in: Address,
    /// Output token as encoded in calldata.
    pub token_out: Address,
    /// Quoted output amount — the router's positive-slippage baseline.
    pub expected_amount_out: U256,
    /// Post-fee floor below which the router reverts.
    pub min_amount_out: U256,
    /// Address receiving the swap output.
    pub receiver: Address,
    /// ABI-encoded swap bytes.
    pub swaps: &'a [u8],
}

/// Signs zero-fee `ClientFee` payloads that identify this deployment as the router fee client.
pub struct DisableSlippageTakingSigner {
    signer: PrivateKeySigner,
    chain_id: u64,
    router_address: Address,
    deadline_window_secs: u64,
}

impl DisableSlippageTakingSigner {
    /// Builds a signer from the `DISABLE_SLIPPAGE_TAKING_SIGNER_KEY` env var: `Ok(None)` when
    /// unset (signing disabled), an error when set but invalid.
    pub fn from_env(chain_id: u64, router_address: &Bytes) -> Result<Option<Self>, SolveError> {
        let Ok(key) = std::env::var(ENV_DISABLE_SLIPPAGE_TAKING_KEY) else {
            return Ok(None);
        };
        let signer = key
            .parse::<PrivateKeySigner>()
            .map_err(|e| {
                SolveError::FailedEncoding(format!(
                    "invalid {ENV_DISABLE_SLIPPAGE_TAKING_KEY}: {e}"
                ))
            })?;
        let router = crate::rpc::to_address(router_address, "router address")
            .map_err(SolveError::FailedEncoding)?;
        Ok(Some(Self::new(signer, chain_id, router, DEADLINE_WINDOW_SECS)))
    }

    /// Creates a signer from explicit parts. `deadline_window_secs` is added to the
    /// signing-time timestamp to form each payload's deadline.
    pub fn new(
        signer: PrivateKeySigner,
        chain_id: u64,
        router_address: Address,
        deadline_window_secs: u64,
    ) -> Self {
        Self { signer, chain_id, router_address, deadline_window_secs }
    }

    /// The `clientFeeReceiver` every payload from this signer carries: the signing key's own
    /// address.
    pub fn receiver(&self) -> Address {
        self.signer.address()
    }

    /// The deadline for a payload signed now.
    pub fn deadline(&self) -> u64 {
        now_unix_secs().saturating_add(self.deadline_window_secs)
    }

    /// Signs the `ClientFee` typed data for one swap: zero fee, zero vault contribution, this
    /// signer as receiver, and the given swap intent.
    ///
    /// # Errors
    /// Errors when the underlying key fails to sign.
    pub fn sign_client_fee(&self, intent: &SwapIntent) -> Result<[u8; 65], SolveError> {
        let payload = ClientFee {
            clientFeeBps: 0,
            clientFeeReceiver: self.receiver(),
            maxClientContribution: U256::ZERO,
            deadline: U256::from(intent.deadline),
            amountIn: intent.amount_in,
            tokenIn: intent.token_in,
            tokenOut: intent.token_out,
            expectedAmountOut: intent.expected_amount_out,
            minAmountOut: intent.min_amount_out,
            receiver: intent.receiver,
            swaps: intent.swaps.to_vec().into(),
        };
        let domain = eip712_domain! {
            name: "TychoRouter",
            version: "1",
            chain_id: self.chain_id,
            verifying_contract: self.router_address,
        };
        let signing_hash = payload.eip712_signing_hash(&domain);
        let signature = self
            .signer
            .sign_hash_sync(&signing_hash)
            .map_err(|e| SolveError::FailedEncoding(format!("client fee signing failed: {e}")))?;
        Ok(signature.as_bytes())
    }
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
    use alloy::{
        primitives::{keccak256, Signature, B256},
        sol_types::SolValue,
    };

    use super::*;

    const SIGNER_KEY: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";
    const ROUTER: Address = Address::repeat_byte(0x99);
    const CHAIN_ID: u64 = 1;

    fn test_signer() -> DisableSlippageTakingSigner {
        DisableSlippageTakingSigner::new(SIGNER_KEY.parse().unwrap(), CHAIN_ID, ROUTER, 600)
    }

    fn test_intent(swaps: &[u8]) -> SwapIntent<'_> {
        SwapIntent {
            deadline: 1_900_000_000,
            amount_in: U256::from(1_000_000u64),
            token_in: Address::repeat_byte(0x01),
            token_out: Address::repeat_byte(0x02),
            expected_amount_out: U256::from(990_000u64),
            min_amount_out: U256::from(980_000u64),
            receiver: Address::repeat_byte(0x03),
            swaps,
        }
    }

    /// Rebuilds the signing hash the way `TychoRouterV3._verifyClientSignature` does,
    /// independently of the `sol!` struct, so a typehash, field-order, or `bytes`-hashing
    /// mismatch with the contract fails the test.
    fn router_signing_hash(signer: &DisableSlippageTakingSigner, intent: &SwapIntent) -> B256 {
        let type_hash = keccak256(
            b"ClientFee(uint32 clientFeeBps,address clientFeeReceiver,\
uint256 maxClientContribution,uint256 deadline,\
uint256 amountIn,address tokenIn,address tokenOut,\
uint256 expectedAmountOut,uint256 minAmountOut,address receiver,bytes swaps)",
        );
        let struct_hash = keccak256(
            (
                type_hash,
                U256::ZERO,
                signer.receiver(),
                U256::ZERO,
                U256::from(intent.deadline),
                intent.amount_in,
                intent.token_in,
                intent.token_out,
                intent.expected_amount_out,
                intent.min_amount_out,
                intent.receiver,
                keccak256(intent.swaps),
            )
                .abi_encode(),
        );
        let domain_type_hash = keccak256(
            b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let domain_separator = keccak256(
            (
                domain_type_hash,
                keccak256(b"TychoRouter"),
                keccak256(b"1"),
                U256::from(CHAIN_ID),
                ROUTER,
            )
                .abi_encode(),
        );
        let mut data = [0u8; 66];
        data[0] = 0x19;
        data[1] = 0x01;
        data[2..34].copy_from_slice(domain_separator.as_slice());
        data[34..66].copy_from_slice(struct_hash.as_slice());
        keccak256(data)
    }

    #[test]
    fn test_sign_client_fee_against_router_signing_hash() {
        let signer = test_signer();
        let intent = test_intent(&[0xAB; 40]);

        let signature_bytes = signer.sign_client_fee(&intent).unwrap();

        let signature = Signature::try_from(&signature_bytes[..]).unwrap();
        let recovered = signature
            .recover_address_from_prehash(&router_signing_hash(&signer, &intent))
            .unwrap();
        assert_eq!(recovered, signer.receiver());
        // v byte is 27/28, matching the on-chain `ECDSA.recover` expectation.
        assert!(matches!(signature_bytes[64], 27 | 28));
    }
}
