//! Signs the Tycho Router's `ClientFee` payload for disable-slippage-taking encoding.
//!
//! The encoder attaches server-signed, zero-fee `ClientFeeParams` to the transaction, so the
//! on-chain FeeCalculator resolves this signer's address as the fee client and applies its
//! positive-slippage exemption. The exemption itself is FeeCalculator state; this module only
//! produces the attribution signature. The router recovers the signature against
//! `clientFeeReceiver`, so the signer's address doubles as the receiver; with a zero fee and
//! zero vault contribution, no funds ever move to it.

use alloy::{
    primitives::{Address, U256},
    signers::{local::PrivateKeySigner, SignerSync},
    sol,
    sol_types::{eip712_domain, SolStruct},
};
use tycho_simulation::tycho_common::Bytes;

use crate::{
    encoding::{now_unix_secs, DEFAULT_DEADLINE_WINDOW_SECS},
    SolveError,
};

/// Environment variable holding the disable-slippage-taking signing key (hex, with or without
/// `0x`).
pub(crate) const ENV_DISABLE_SLIPPAGE_TAKING_KEY: &str = "DISABLE_SLIPPAGE_TAKING_SIGNER_KEY";

sol! {
    /// Mirror of `TychoRouterV3.CLIENT_FEE_TYPEHASH`: the field names, types, and order must
    /// match the contract exactly, or the recovered signer changes and the router rejects the
    /// params.
    ///
    /// Also mirrored manually in `clients/rust/src/types.rs` (`ClientFeeParams::eip712_signing_hash`);
    /// keep both in sync if the contract type changes.
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
pub(crate) struct SwapIntent<'a> {
    /// Exact input amount.
    pub(crate) amount_in: U256,
    /// Input token as encoded in calldata.
    pub(crate) token_in: Address,
    /// Output token as encoded in calldata.
    pub(crate) token_out: Address,
    /// Quoted output amount — the router's positive-slippage baseline.
    pub(crate) expected_amount_out: U256,
    /// Post-fee floor below which the router reverts.
    pub(crate) min_amount_out: U256,
    /// Address receiving the swap output.
    pub(crate) receiver: Address,
    /// ABI-encoded swap bytes.
    pub(crate) swaps: &'a [u8],
}

/// A signed zero-fee `ClientFee` payload: the 65-byte signature and the deadline it covers.
///
/// Both go into the same calldata field, and the signature is only valid for this deadline, so
/// they travel together.
pub(crate) struct SignedClientFee {
    /// Unix timestamp after which the router rejects the params.
    pub(crate) deadline: u64,
    /// EIP-712 signature over the `ClientFee` payload, `r || s || v`.
    pub(crate) signature: [u8; 65],
}

/// Signs zero-fee `ClientFee` payloads that identify this deployment as the router fee client.
pub(crate) struct DisableSlippageTakingSigner {
    signer: PrivateKeySigner,
    chain_id: u64,
    router_address: Address,
    deadline_window_secs: u32,
}

impl DisableSlippageTakingSigner {
    /// Builds a signer from the `DISABLE_SLIPPAGE_TAKING_SIGNER_KEY` env var: `Ok(None)` when
    /// unset (signing disabled), an error when set but invalid.
    pub(crate) fn from_env(
        chain_id: u64,
        router_address: &Bytes,
    ) -> Result<Option<Self>, SolveError> {
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
        Ok(Some(Self::new(signer, chain_id, router, DEFAULT_DEADLINE_WINDOW_SECS)))
    }

    /// Creates a signer from explicit parts. `deadline_window_secs` is added to the
    /// signing-time timestamp to form each payload's deadline.
    pub(crate) fn new(
        signer: PrivateKeySigner,
        chain_id: u64,
        router_address: Address,
        deadline_window_secs: u32,
    ) -> Self {
        Self { signer, chain_id, router_address, deadline_window_secs }
    }

    /// The `clientFeeReceiver` every payload from this signer carries: the signing key's own
    /// address.
    pub(crate) fn receiver(&self) -> Address {
        self.signer.address()
    }

    /// Signs the `ClientFee` typed data for one swap: zero fee, zero vault contribution, this
    /// signer as receiver, and the given swap intent.
    ///
    /// The deadline is derived here, from the signing-time clock and this signer's window, and
    /// returned alongside the signature it is covered by — the caller encodes both as they come.
    ///
    /// # Errors
    /// Errors when the system clock reads before the Unix epoch, or the underlying key fails to
    /// sign.
    pub(crate) fn sign_client_fee(
        &self,
        intent: &SwapIntent,
    ) -> Result<SignedClientFee, SolveError> {
        let deadline = now_unix_secs()?.saturating_add(u64::from(self.deadline_window_secs));
        Ok(SignedClientFee { deadline, signature: self.sign_at_deadline(deadline, intent)? })
    }

    /// Signs the `ClientFee` typed data for an already-chosen `deadline`. Reachable from the
    /// crate's tests, which re-sign decoded calldata to check what a signature binds.
    ///
    /// # Errors
    /// Errors when the underlying key fails to sign.
    pub(crate) fn sign_at_deadline(
        &self,
        deadline: u64,
        intent: &SwapIntent,
    ) -> Result<[u8; 65], SolveError> {
        let payload = ClientFee {
            clientFeeBps: 0,
            clientFeeReceiver: self.receiver(),
            maxClientContribution: U256::ZERO,
            deadline: U256::from(deadline),
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
        DisableSlippageTakingSigner::new(
            SIGNER_KEY.parse().unwrap(),
            CHAIN_ID,
            ROUTER,
            DEFAULT_DEADLINE_WINDOW_SECS,
        )
    }

    fn test_intent(swaps: &[u8]) -> SwapIntent<'_> {
        SwapIntent {
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
    fn router_signing_hash(
        signer: &DisableSlippageTakingSigner,
        intent: &SwapIntent,
        deadline: u64,
    ) -> B256 {
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
                U256::from(deadline),
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

        let signed = signer.sign_client_fee(&intent).unwrap();

        let signature = Signature::try_from(&signed.signature[..]).unwrap();
        let recovered = signature
            .recover_address_from_prehash(&router_signing_hash(&signer, &intent, signed.deadline))
            .unwrap();
        assert_eq!(recovered, signer.receiver());
        // v byte is 27/28, matching the on-chain `ECDSA.recover` expectation.
        assert!(matches!(signed.signature[64], 27 | 28));
    }

    #[test]
    fn test_sign_client_fee_deadline_window() {
        let before = now_unix_secs().unwrap();

        let signed = test_signer()
            .sign_client_fee(&test_intent(&[0xAB; 40]))
            .unwrap();

        let window = u64::from(DEFAULT_DEADLINE_WINDOW_SECS);
        assert!(
            signed.deadline >= before + window,
            "deadline {} predates the window opened at {before}",
            signed.deadline
        );
        assert!(
            signed.deadline <= now_unix_secs().unwrap() + window,
            "deadline {} outlives the window",
            signed.deadline
        );
    }
}
