// UniswapX Gateway
//
// Handles all UniswapX API interactions and order lifecycle management
// Inspired by patterns from defibot/solver/uniswap_x/orderbook/uniswap_x_api_client.py

use std::collections::HashMap;

/// UniswapX gateway operation error types
#[derive(Debug)]
pub enum GatewayError {
    Api(String),
    OrderProcessing(String),
    Authentication(String),
    RateLimit(String),
    External(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(msg) => write!(f, "API request failed: {}", msg),
            Self::OrderProcessing(msg) => write!(f, "Order processing failed: {}", msg),
            Self::Authentication(msg) => write!(f, "Authentication failed: {}", msg),
            Self::RateLimit(msg) => write!(f, "Rate limiting: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for GatewayError {}

use crate::{
    models::{UniswapXConfig, UniswapXError, UniswapXOrderResponse},
    order::{Order, ResolvedOrder},
    orderbook::OrderBook,
};

/// Gateway for UniswapX API integration and order management
/// Based on UniswapXAPIClient pattern from the Python implementation
pub struct UniswapXGateway {
    config: UniswapXConfig,
    order_book: OrderBook,
}

impl UniswapXGateway {
    pub fn new(config: UniswapXConfig) -> Result<Self, UniswapXError> {
        Ok(Self { config, order_book: OrderBook::new() })
    }

    /// Get currently active orders from UniswapX API
    pub async fn get_active_orders(&mut self) -> Result<HashMap<String, Order>, UniswapXError> {
        // 1. Make API request to UniswapX orders endpoint
        let mut params = HashMap::new();
        params.insert("orderStatus".to_string(), "open".to_string());
        params.insert("chainId".to_string(), self.config.chain_id.to_string());

        // 2. Get raw orders from API (with pagination)
        let raw_orders = self
            .make_paginated_request(Some(params))
            .await?;

        // 3. Decode each encoded order using Order::decode()
        let decoded_orders = self
            .decode_raw_orders(raw_orders)
            .await?;

        // 4. Update order_book with new orders
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let orders_vec: Vec<(String, Order)> = decoded_orders
            .iter()
            .map(|(hash, order)| (hash.clone(), order.clone()))
            .collect();
        self.order_book
            .update_orders(orders_vec, current_timestamp);

        // 5. Return decoded orders indexed by order_hash
        Ok(decoded_orders)
    }

    /// Get filled orders within time range
    /// Based on get_filled_orders() pattern from Python implementation  
    pub async fn get_filled_orders(
        &self,
        from_ts: u64,
        to_ts: Option<u64>,
    ) -> Result<Vec<Order>, UniswapXError> {
        // TODO: Make API request for filled orders
        // - Set orderStatus=filled query parameter
        // - Set sortKey=createdAt and sort=between(from,to) or gt(from)
        // - Handle pagination
        // - Decode orders and return
        todo!("Implement get_filled_orders API call")
    }

    /// Get specific order by hash
    pub async fn get_order_by_hash(
        &self,
        order_hash: &str,
    ) -> Result<Option<Order>, UniswapXError> {
        // TODO: Make API request for specific order
        // - Set orderHash query parameter
        // - Decode if found
        todo!("Implement get_order_by_hash API call")
    }

    /// Resolve orders to current amounts based on timestamp/block
    /// Uses uniswapx-rs resolution logic for decay curves
    pub fn resolve_orders(
        &self,
        orders: &[(String, Order)],
        timestamp: u64,
        block_number: u64,
        gas_price: Option<&tycho_router::models::GasPrice>,
    ) -> Vec<(String, ResolvedOrder)> {
        let mut resolved_orders = Vec::new();

        for (order_hash, order) in orders {
            // Call resolve() method from uniswapx-rs based on order type
            let resolution = match order {
                crate::order::Order::V2DutchOrder(dutch_order) => dutch_order.resolve(timestamp),
                crate::order::Order::PriorityOrder(priority_order) => {
                    // Use actual priority fee from gas price or fallback
                    let priority_fee = gas_price
                        .map(|gp| gp.priority_fee_uint())
                        .unwrap_or_else(|| alloy::primitives::Uint::from(1000000000u64)); // 1 gwei fallback
                    priority_order.resolve(block_number, timestamp, priority_fee)
                }
            };

            // Handle resolution results
            match resolution {
                crate::order::OrderResolution::Resolved(resolved_order) => {
                    resolved_orders.push((order_hash.clone(), resolved_order));
                }
                crate::order::OrderResolution::Expired => {
                    println!("Order {} expired", order_hash);
                    // TODO: Mark order as expired in OrderBook
                }
                crate::order::OrderResolution::Invalid => {
                    println!("Order {} is invalid", order_hash);
                    // TODO: Mark order as failed in OrderBook
                }
                crate::order::OrderResolution::NotFillableYet => {
                    println!("Order {} not fillable yet (priority auction)", order_hash);
                    // Skip for now, will be fillable later
                }
            }
        }

        println!("Resolved {}/{} orders successfully", resolved_orders.len(), orders.len());
        resolved_orders
    }

    /// Convert resolved orders to tycho-router format
    pub fn to_tycho_orders(
        &self,
        resolved_orders: &[(String, ResolvedOrder)],
        tokens: &std::collections::HashMap<
            tycho_simulation::tycho_common::Bytes,
            tycho_simulation::tycho_common::models::token::Token,
        >,
    ) -> Vec<tycho_router::models::Order> {
        resolved_orders
            .iter()
            .filter_map(|(order_hash, resolved_order)| {
                match resolved_order.to_tycho_order(order_hash.clone(), tokens) {
                    Ok(order) => Some(order),
                    Err(e) => {
                        eprintln!("Failed to convert order {}: {}", order_hash, e);
                        None
                    }
                }
            })
            .collect()
    }

    /// Complete pipeline: fetch → resolve → convert
    /// Main entry point inspired by driver.py processing flow
    pub async fn get_processable_orders(
        &mut self,
        timestamp: u64,
        block_number: u64,
        gas_price: Option<&tycho_router::models::GasPrice>,
        tokens: &std::collections::HashMap<
            tycho_simulation::tycho_common::Bytes,
            tycho_simulation::tycho_common::models::token::Token,
        >,
    ) -> Result<Vec<tycho_router::models::Order>, UniswapXError> {
        // 1. Fetch active orders from API (includes decoding and OrderBook update)
        let active_orders = self.get_active_orders().await?;

        // 2. Get processable orders from OrderBook (filtered by status)
        let processable_orders = self
            .order_book
            .get_processable_orders(timestamp);
        let orders_to_resolve: Vec<(String, crate::order::Order)> = processable_orders
            .iter()
            .filter_map(|order_ref| {
                // Find the order hash for this order
                active_orders
                    .iter()
                    .find(|(_, order)| std::ptr::eq(*order, *order_ref))
                    .map(|(hash, order)| (hash.clone(), order.clone()))
            })
            .collect();

        // 3. Resolve orders using uniswapx-rs logic
        let resolved_orders = self.resolve_orders(&orders_to_resolve, timestamp, block_number, gas_price);

        // 4. Mark orders as being processed in OrderBook
        for (order_hash, _) in &resolved_orders {
            self.order_book
                .mark_processing(order_hash, timestamp);
        }

        // 5. Convert to tycho-router format
        let tycho_orders = self.to_tycho_orders(&resolved_orders, tokens);

        Ok(tycho_orders)
    }

    /// Make paginated API request with cursor handling
    /// Based on _make_request() pattern from Python implementation
    async fn make_paginated_request(
        &self,
        params: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, UniswapXOrderResponse>, UniswapXError> {
        let mut all_orders = HashMap::new();
        let mut cursor: Option<String> = None;
        let mut page_count = 0;
        let max_pages = 10; // Safety limit to prevent infinite loops

        loop {
            page_count += 1;
            if page_count > max_pages {
                eprintln!("Reached maximum page limit ({}) for API request", max_pages);
                break;
            }

            // Build request parameters
            let mut request_params = params.clone().unwrap_or_default();
            if let Some(cursor_value) = &cursor {
                request_params.insert("cursor".to_string(), cursor_value.clone());
            }
            request_params.insert(
                "limit".to_string(),
                self.config
                    .max_orders_per_request
                    .to_string(),
            );

            // TODO: Make actual HTTP request here
            // For now, return empty result to prevent blocking
            println!(
                "TODO: Making API request to {} with params: {:?}",
                self.config.api_endpoint, request_params
            );

            // TODO: Implement actual HTTP request:
            // let response = self.make_api_request(&request_params).await?;
            // let page_data: ApiResponse = serde_json::from_str(&response)?;
            //
            // for order in page_data.orders {
            //     all_orders.insert(order.order_hash.clone(), order);
            // }
            //
            // cursor = page_data.cursor;
            // if cursor.is_none() { break; }

            // For now, break to avoid infinite loop
            break;
        }

        println!("Fetched {} orders across {} API pages", all_orders.len(), page_count);
        Ok(all_orders)
    }

    /// Decode raw API responses into Order structs
    async fn decode_raw_orders(
        &self,
        raw_orders: HashMap<String, UniswapXOrderResponse>,
    ) -> Result<HashMap<String, Order>, UniswapXError> {
        let mut decoded_orders = HashMap::new();

        for (order_hash, raw_order) in raw_orders {
            let encoded_bytes = match hex::decode(
                &raw_order
                    .encoded_order
                    .trim_start_matches("0x"),
            ) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("Failed to decode hex for order {}: {}", order_hash, e);
                    continue;
                }
            };

            // Decode using Order::decode from uniswapx-rs
            match crate::order::Order::decode(&encoded_bytes, &raw_order.order_type) {
                Ok(order) => {
                    // TODO: Add validation logic here:
                    // - Check if tokens are known
                    // - Validate order is not expired
                    // - Check order signature if needed

                    decoded_orders.insert(order_hash, order);
                }
                Err(e) => {
                    eprintln!("Failed to decode order {}: {}", order_hash, e);
                    // Continue processing other orders
                }
            }
        }
        Ok(decoded_orders)
    }
}
