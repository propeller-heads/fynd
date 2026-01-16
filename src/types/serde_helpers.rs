//! Serde helper modules for custom serialization.

/// Serialize BigUint as a decimal string.
///
/// This ensures JSON compatibility with JavaScript/TypeScript clients (and other languages),
/// since JS numbers cannot safely represent values above 2^53-1.
pub mod biguint_as_string {
    use num_bigint::BigUint;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &BigUint, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BigUint, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<BigUint>()
            .map_err(serde::de::Error::custom)
    }
}

/// Serialize BigInt as a decimal string.
///
/// Same as biguint_as_string but for signed integers. Supports negative values.
pub mod bigint_as_string {
    use num_bigint::BigInt;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &BigInt, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BigInt, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<BigInt>()
            .map_err(serde::de::Error::custom)
    }
}
