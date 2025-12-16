use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::Bytes;

use crate::{
    gateway::GatewayError,
    order::{OrderError, ResolvedOrder},
    orderbook::OrderbookError,
};

/// Configuration for UniswapX API integration
#[derive(Clone, Debug)]
pub struct UniswapXConfig {
    pub api_endpoint: String,
    pub api_key: Option<String>,
    pub chain_id: u64,
    pub timeout_secs: u64,
    pub max_orders_per_request: usize,
    pub filler_address: Bytes,
    pub usx_reactor: Bytes,
}

impl Default for UniswapXConfig {
    fn default() -> Self {
        Self {
            api_endpoint: "https://api.uniswap.org/v2/orders".to_string(),
            api_key: None,
            chain_id: 1, // Ethereum mainnet
            timeout_secs: 30,
            max_orders_per_request: 100,
            filler_address: Bytes(hex::decode("6D9da78B6A5BEdcA287AA5d49613bA36b90c15C4").unwrap().into()),
            usx_reactor: Bytes(hex::decode("00000011F84B9aa48e5f8aA8B9897600006289Be").unwrap().into()),
        }
    }
}

/// Raw order response from UniswapX API
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UniswapXOrderResponse {
    #[serde(rename = "orderHash")]
    pub order_hash: String,
    #[serde(rename = "encodedOrder")]
    pub encoded_order: String,
    #[serde(rename = "orderStatus")]
    pub order_status: String,
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    #[serde(rename = "type")]
    pub order_type: String,
    pub signature: String,
    pub deadline: u64,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    // Add other API fields as needed
}

/// Error types for UniswapX operations
#[derive(Debug)]
pub enum UniswapXError {
    Order(OrderError),
    Gateway(GatewayError),
    Orderbook(OrderbookError),
    JsonError(serde_json::Error),
    Config(String),
    ConversionError(String),
    External(String),
}

impl std::fmt::Display for UniswapXError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Order(err) => write!(f, "Order error: {}", err),
            Self::Gateway(err) => write!(f, "Gateway error: {}", err),
            Self::Orderbook(err) => write!(f, "Orderbook error: {}", err),
            Self::JsonError(err) => write!(f, "JSON parsing failed: {}", err),
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::ConversionError(msg) => write!(f, "Conversion error: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for UniswapXError {}

impl From<OrderError> for UniswapXError {
    fn from(err: OrderError) -> Self {
        Self::Order(err)
    }
}

impl From<GatewayError> for UniswapXError {
    fn from(err: GatewayError) -> Self {
        Self::Gateway(err)
    }
}

impl From<OrderbookError> for UniswapXError {
    fn from(err: OrderbookError) -> Self {
        Self::Orderbook(err)
    }
}

impl From<serde_json::Error> for UniswapXError {
    fn from(err: serde_json::Error) -> Self {
        Self::JsonError(err)
    }
}

/// Conversion from UniswapX ResolvedOrder to tycho-router Order
impl ResolvedOrder {
    pub fn to_tycho_order(
        &self,
        order_hash: String,
        tokens: &std::collections::HashMap<
            Bytes,
            tycho_simulation::tycho_common::models::token::Token,
        >,
    ) -> Result<tycho_router::models::Order, UniswapXError> {
        // Convert input token address string to Bytes for lookup
        let input_token_bytes = parse_address_to_bytes(&self.input.token)?;
        let token_in = tokens
            .get(&input_token_bytes)
            .ok_or_else(|| {
                UniswapXError::ConversionError(format!("Unknown input token: {}", self.input.token))
            })?;

        // Use first output as the primary output token (UniswapX orders typically have one main
        // output)
        let main_output = self
            .outputs
            .first()
            .ok_or_else(|| UniswapXError::ConversionError("Order has no outputs".to_string()))?;

        let output_token_bytes = parse_address_to_bytes(&main_output.token)?;
        let token_out = tokens
            .get(&output_token_bytes)
            .ok_or_else(|| {
                UniswapXError::ConversionError(format!(
                    "Unknown output token: {}",
                    main_output.token
                ))
            })?;

        // Convert amounts from Uint<256, 4> to BigUint
        let amount_in = convert_uint_to_biguint(&self.input.amount);
        let amount_out = convert_uint_to_biguint(&main_output.amount);

        // Convert recipient address to Bytes
        let origin_address = parse_address_to_bytes(&main_output.recipient)?;

        // Create tycho-router Order (exact_in mode)
        let order = tycho_router::models::Order::new(
            order_hash,
            token_in.clone(),
            token_out.clone(),
            Some(amount_in), // amount_in for exact_in orders
            None,            // amount_out is None for exact_in
            false,           // exact_out = false (we're doing exact_in)
            amount_out,      // min_amount (minimum output amount)
            origin_address.clone(),
            None, // receiver same as origin
        );

        Ok(order)
    }
}

/// Helper function to convert alloy Uint<256, 4> to BigUint
fn convert_uint_to_biguint(uint: &alloy::primitives::Uint<256, 4>) -> BigUint {
    // Convert to bytes and then to BigUint
    let bytes = uint.to_be_bytes_vec();
    BigUint::from_bytes_be(&bytes)
}

/// Helper function to parse address string to Bytes
fn parse_address_to_bytes(address: &str) -> Result<Bytes, UniswapXError> {
    // Remove 0x prefix if present
    let cleaned = if address.starts_with("0x") { &address[2..] } else { address };

    // Parse hex string to bytes
    hex::decode(cleaned)
        .map(Bytes::from)
        .map_err(|e| UniswapXError::ConversionError(format!("Invalid address format: {}", e)))
}
