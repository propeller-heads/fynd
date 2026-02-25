//! OpenAPI wrapper types for tycho-execution encoding types.
//!
//! These types wrap the encoding types from tycho-execution to provide
//! ToSchema implementations for OpenAPI documentation. They maintain
//! serialization compatibility via From/Into conversions.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tycho_execution::encoding::models::{
    EncodedSolution as TychoEncodedSolution, PermitDetails as TychoPermitDetails,
    PermitSingle as TychoPermitSingle,
};
use tycho_simulation::tycho_common::Bytes;
use utoipa::ToSchema;

/// Wrapper for EncodedSolution with ToSchema support.
///
/// This wrapper provides OpenAPI schema generation for the EncodedSolution
/// type from tycho-execution. All fields maintain serialization compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EncodedSolution {
    /// Encoded swap data as hex string.
    #[schema(value_type = String, example = "0x1234567890abcdef")]
    #[serde(serialize_with = "serialize_bytes_hex", deserialize_with = "deserialize_bytes_hex")]
    pub swaps: Vec<u8>,

    /// Address of the contract to interact with.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub interacting_with: Bytes,

    /// Function signature for the contract call.
    #[schema(example = "executeSwap(bytes,address,uint256)")]
    pub function_signature: String,

    /// Number of tokens involved in the swap.
    #[schema(example = 2)]
    pub n_tokens: usize,

    /// Optional Permit2 permit for token approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permit: Option<PermitSingle>,
}

/// Wrapper for PermitSingle with ToSchema support.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PermitSingle {
    /// Permit details including token, amount, expiration, and nonce.
    pub details: PermitDetails,

    /// Address authorized to spend the tokens.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub spender: Bytes,

    /// Signature deadline as Unix timestamp.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1735689600")]
    pub sig_deadline: BigUint,
}

/// Wrapper for PermitDetails with ToSchema support.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PermitDetails {
    /// Token address for which the permit is granted.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token: Bytes,

    /// Amount of tokens approved for spending.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1000000000000000000")]
    pub amount: BigUint,

    /// Permit expiration as Unix timestamp.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "1735689600")]
    pub expiration: BigUint,

    /// Unique nonce to prevent replay attacks.
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "42")]
    pub nonce: BigUint,
}

// ============================================================================
// CUSTOM SERIALIZATION
// ============================================================================

/// Serializes Vec<u8> to hex string with 0x prefix.
fn serialize_bytes_hex<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&format!("0x{}", hex::encode(bytes)))
}

/// Deserializes hex string (with or without 0x prefix) to Vec<u8>.
fn deserialize_bytes_hex<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let s = s.strip_prefix("0x").unwrap_or(&s);
    hex::decode(s).map_err(serde::de::Error::custom)
}

// ============================================================================
// CONVERSIONS: Wrapper <-> Tycho Types
// ============================================================================

impl From<TychoEncodedSolution> for EncodedSolution {
    fn from(tycho: TychoEncodedSolution) -> Self {
        Self {
            swaps: tycho.swaps,
            interacting_with: tycho.interacting_with,
            function_signature: tycho.function_signature,
            n_tokens: tycho.n_tokens,
            permit: tycho.permit.map(Into::into),
        }
    }
}

impl From<EncodedSolution> for TychoEncodedSolution {
    fn from(wrapper: EncodedSolution) -> Self {
        Self {
            swaps: wrapper.swaps,
            interacting_with: wrapper.interacting_with,
            function_signature: wrapper.function_signature,
            n_tokens: wrapper.n_tokens,
            permit: wrapper.permit.map(Into::into),
        }
    }
}

impl From<TychoPermitSingle> for PermitSingle {
    fn from(tycho: TychoPermitSingle) -> Self {
        Self {
            details: tycho.details.into(),
            spender: tycho.spender,
            sig_deadline: tycho.sig_deadline,
        }
    }
}

impl From<PermitSingle> for TychoPermitSingle {
    fn from(wrapper: PermitSingle) -> Self {
        Self {
            details: wrapper.details.into(),
            spender: wrapper.spender,
            sig_deadline: wrapper.sig_deadline,
        }
    }
}

impl From<TychoPermitDetails> for PermitDetails {
    fn from(tycho: TychoPermitDetails) -> Self {
        Self {
            token: tycho.token,
            amount: tycho.amount,
            expiration: tycho.expiration,
            nonce: tycho.nonce,
        }
    }
}

impl From<PermitDetails> for TychoPermitDetails {
    fn from(wrapper: PermitDetails) -> Self {
        Self {
            token: wrapper.token,
            amount: wrapper.amount,
            expiration: wrapper.expiration,
            nonce: wrapper.nonce,
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoded_solution_hex_serialization() {
        let solution = EncodedSolution {
            swaps: vec![0x12, 0x34, 0x56, 0x78],
            interacting_with: Bytes::from(vec![0xC0; 20]),
            function_signature: "test()".to_string(),
            n_tokens: 2,
            permit: None,
        };

        let json = serde_json::to_string(&solution).unwrap();
        assert!(json.contains("\"swaps\":\"0x12345678\""));

        // Verify round-trip
        let deserialized: EncodedSolution = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.swaps, solution.swaps);
    }

    #[test]
    fn test_conversion_roundtrip() {
        let tycho = TychoEncodedSolution {
            swaps: vec![0xAB, 0xCD],
            interacting_with: Bytes::from(vec![0xC0; 20]),
            function_signature: "test()".to_string(),
            n_tokens: 3,
            permit: None,
        };

        let wrapper: EncodedSolution = tycho.clone().into();
        let back: TychoEncodedSolution = wrapper.into();

        assert_eq!(back.swaps, tycho.swaps);
        assert_eq!(back.n_tokens, tycho.n_tokens);
    }

    #[test]
    fn test_permit_details_biguint_serialization() {
        let details = PermitDetails {
            token: Bytes::from(vec![0xA0; 20]),
            amount: BigUint::from(1_000_000_000_000_000_000u64),
            expiration: BigUint::from(1735689600u64),
            nonce: BigUint::from(42u64),
        };

        let json = serde_json::to_string(&details).unwrap();

        // Verify BigUint serializes as string
        assert!(json.contains("\"amount\":\"1000000000000000000\""));
        assert!(json.contains("\"expiration\":\"1735689600\""));
        assert!(json.contains("\"nonce\":\"42\""));
    }
}
