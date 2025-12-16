// UniswapX Order Book
//
// Order lifecycle tracking and management
// Inspired by patterns from defibot/solver/uniswap_x/orderbook.py

use std::collections::HashMap;

/// UniswapX orderbook operation error types
#[derive(Debug)]
pub enum OrderbookError {
    OrderTracking(String),
    OrderProcessing(String),
    DataInconsistency(String),
    Storage(String),
    External(String),
}

impl std::fmt::Display for OrderbookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OrderTracking(msg) => write!(f, "Order tracking failed: {}", msg),
            Self::OrderProcessing(msg) => write!(f, "Order processing failed: {}", msg),
            Self::DataInconsistency(msg) => write!(f, "Data inconsistency: {}", msg),
            Self::Storage(msg) => write!(f, "Storage operation failed: {}", msg),
            Self::External(msg) => write!(f, "External error: {}", msg),
        }
    }
}

impl std::error::Error for OrderbookError {}

use crate::order::Order;

/// Order lifecycle tracking - inspired by driver.py patterns
#[derive(Clone, Debug)]
pub struct OrderBook {
    /// Active orders being tracked
    pub active_orders: HashMap<String, Order>,
    /// Order status tracking
    pub order_statuses: HashMap<String, OrderStatus>,
    /// Recently filled orders for analytics
    pub filled_orders: HashMap<String, Order>,
}

#[derive(Clone, Debug)]
pub struct OrderStatus {
    pub order_hash: String,
    pub status: OrderLifecycleStatus,
    pub first_seen_at: u64,
    pub last_updated_at: u64,
    pub processing_attempts: u32,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderLifecycleStatus {
    /// Just discovered from API
    Discovered,
    /// Validated and ready for processing  
    Validated,
    /// Currently being processed by solver
    Processing,
    /// Successfully processed/filled
    Filled,
    /// Processing failed
    Failed,
    /// Order expired or cancelled
    Expired,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            active_orders: HashMap::new(),
            order_statuses: HashMap::new(),
            filled_orders: HashMap::new(),
        }
    }

    pub fn update_orders(&mut self, orders: Vec<(String, Order)>, current_timestamp: u64) {
        for (order_hash, order) in orders {
            // Check if this is a new order or an existing one
            match self
                .active_orders
                .entry(order_hash.clone())
            {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    // New order - add to active orders and create status tracking
                    entry.insert(order);
                    self.order_statuses.insert(
                        order_hash.clone(),
                        OrderStatus {
                            order_hash: order_hash.clone(),
                            status: OrderLifecycleStatus::Discovered,
                            first_seen_at: current_timestamp,
                            last_updated_at: current_timestamp,
                            processing_attempts: 0,
                            last_error: None,
                        },
                    );
                    println!("Discovered new order: {}", order_hash);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    // Existing order - update the order data and last_seen timestamp
                    entry.insert(order);
                    if let Some(status) = self.order_statuses.get_mut(&order_hash) {
                        status.last_updated_at = current_timestamp;
                        // TODO: Could compare order data here to detect changes
                    }
                }
            }

            // Auto-validate new orders (basic validation)
            if let Some(status) = self.order_statuses.get_mut(&order_hash) {
                if status.status == OrderLifecycleStatus::Discovered {
                    status.status = OrderLifecycleStatus::Validated;
                    println!("Validated order: {}", order_hash);
                }
            }
        }

        // Clean up expired orders
        self.cleanup_expired_orders(current_timestamp);
    }

    pub fn get_processable_orders(&self, _current_timestamp: u64) -> Vec<&Order> {
        self.active_orders
            .iter()
            .filter_map(|(order_hash, order)| {
                // Check order status - only process Validated orders
                if let Some(status) = self.order_statuses.get(order_hash) {
                    match status.status {
                        OrderLifecycleStatus::Validated => {
                            // TODO: Add additional checks here:
                            // - Check if order is not expired based on deadline
                            // - Check if we have recent pricing data for tokens
                            // - Check if order hasn't failed too many times
                            Some(order)
                        }
                        _ => None, // Skip orders that are not validated or already being processed
                    }
                } else {
                    None // No status tracking - shouldn't happen
                }
            })
            .collect()
    }

    pub fn mark_processing(&mut self, order_hash: &str, current_timestamp: u64) {
        if let Some(status) = self.order_statuses.get_mut(order_hash) {
            status.status = OrderLifecycleStatus::Processing;
            status.last_updated_at = current_timestamp;
            status.processing_attempts += 1;
            println!(
                "Marking order {} as processing (attempt #{})",
                order_hash, status.processing_attempts
            );
        }
    }

    pub fn mark_completed(&mut self, order_hash: &str, current_timestamp: u64) {
        if let Some(order) = self.active_orders.remove(order_hash) {
            // Move to filled orders
            self.filled_orders
                .insert(order_hash.to_string(), order);

            if let Some(mut status) = self.order_statuses.remove(order_hash) {
                status.status = OrderLifecycleStatus::Filled;
                status.last_updated_at = current_timestamp;
                println!("Order {} completed successfully", order_hash);
                // Could store completed status in a separate map for analytics
            }
        }
    }

    pub fn mark_failed(&mut self, order_hash: &str, error: String, current_timestamp: u64) {
        if let Some(status) = self.order_statuses.get_mut(order_hash) {
            status.status = OrderLifecycleStatus::Failed;
            status.last_updated_at = current_timestamp;
            status.last_error = Some(error.clone());
            println!("Order {} failed: {}", order_hash, error);

            // TODO: Could implement retry logic here:
            // - If processing_attempts < MAX_RETRIES, reset to Validated after delay
            // - Otherwise, move to permanent failure
        }
    }

    /// Clean up expired orders from active tracking
    pub fn cleanup_expired_orders(&mut self, current_timestamp: u64) {
        let expired_threshold = current_timestamp.saturating_sub(3600); // 1 hour ago

        let mut expired_orders = Vec::new();
        for (order_hash, status) in &self.order_statuses {
            if status.last_updated_at < expired_threshold {
                expired_orders.push(order_hash.clone());
            }
        }

        for order_hash in expired_orders {
            self.active_orders.remove(&order_hash);
            if let Some(mut status) = self.order_statuses.remove(&order_hash) {
                status.status = OrderLifecycleStatus::Expired;
                println!("Cleaned up expired order: {}", order_hash);
            }
        }
    }
}
