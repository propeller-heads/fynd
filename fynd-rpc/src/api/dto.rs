//! Data Transfer Objects (DTOs) for the HTTP API.
//!
//! Types are defined in `fynd-rpc-types` and re-exported here for use by
//! the server handlers. Conversions between DTO types and `fynd-core` domain
//! types are defined in this module as free functions since Rust's orphan rules
//! prevent implementing `From`/`Into` for types defined in external crates.

// Re-export all DTO types from the shared crate.
pub use fynd_rpc_types::{
    BlockInfo, ErrorResponse, HealthStatus, Order, OrderSide, OrderSolution, Route,
    SingleOrderSolution, Solution, SolutionOptions, SolutionRequest, SolutionStatus, Swap,
};

// ============================================================================
// CONVERSIONS: DTO -> Core
// ============================================================================

impl From<QuoteRequest> for fynd_core::QuoteRequest {
    fn from(dto: QuoteRequest) -> Self {
        Self::new(
            dto.orders
                .into_iter()
                .map(Into::into)
                .collect(),
            dto.options.into(),
        )
    }
}

impl From<QuoteOptions> for fynd_core::QuoteOptions {
    fn from(dto: QuoteOptions) -> Self {
        let mut opts = fynd_core::QuoteOptions::default();
        if let Some(ms) = dto.timeout_ms {
            opts = opts.with_timeout_ms(ms);
        }
        if let Some(n) = dto.min_responses {
            opts = opts.with_min_responses(n);
        }
        if let Some(gas) = dto.max_gas {
            opts = opts.with_max_gas(gas);
        }
        if let Some(enc) = dto.encoding_options {
            opts = opts.with_encoding_options(enc.into());
        }
        opts
    }
}

impl From<UserTransferType> for fynd_core::UserTransferType {
    fn from(dto: UserTransferType) -> Self {
        match dto {
            UserTransferType::TransferFromPermit2 => Self::TransferFromPermit2,
            UserTransferType::TransferFrom => Self::TransferFrom,
            UserTransferType::None => Self::None,
        }
    }
}

impl From<EncodingOptions> for fynd_core::EncodingOptions {
    fn from(dto: EncodingOptions) -> Self {
        let mut opts = fynd_core::EncodingOptions::new(dto.slippage)
            .with_transfer_type(dto.transfer_type.into());
        if let Some(permit) = dto.permit {
            opts = opts.with_permit(permit.into());
        }
        if let Some(sig) = dto.permit2_signature {
            opts = opts.with_signature(sig);
        }
        opts
    }
}

impl From<PermitSingle> for fynd_core::PermitSingle {
    fn from(dto: PermitSingle) -> Self {
        Self::new(dto.details.into(), dto.spender, dto.sig_deadline)
    }
}

impl From<PermitDetails> for fynd_core::PermitDetails {
    fn from(dto: PermitDetails) -> Self {
        Self::new(dto.token, dto.amount, dto.expiration, dto.nonce)
    }
}

impl From<Order> for fynd_core::Order {
    fn from(dto: Order) -> Self {
        let mut order = fynd_core::Order::new(
            dto.token_in,
            dto.token_out,
            dto.amount,
            dto.side.into(),
            dto.sender,
        )
        .with_id(dto.id);
        if let Some(r) = dto.receiver {
            order = order.with_receiver(r);
        }
        order
    }
}

impl From<OrderSide> for fynd_core::OrderSide {
    fn from(dto: OrderSide) -> Self {
        match dto {
            OrderSide::Sell => Self::Sell,
        }
    }
}

// ============================================================================
// CONVERSIONS: Core -> DTO
// ============================================================================

impl From<fynd_core::Quote> for Quote {
    fn from(core: fynd_core::Quote) -> Self {
        let solve_time_ms = core.solve_time_ms();
        let total_gas_estimate = core.total_gas_estimate().clone();
        Self {
            orders: core
                .into_orders()
                .into_iter()
                .map(Into::into)
                .collect(),
            total_gas_estimate,
            solve_time_ms,
        }
    }
}

impl From<fynd_core::OrderQuote> for OrderQuote {
    fn from(core: fynd_core::OrderQuote) -> Self {
        let order_id = core.order_id().to_string();
        let status = core.status().into();
        let amount_in = core.amount_in().clone();
        let amount_out = core.amount_out().clone();
        let gas_estimate = core.gas_estimate().clone();
        let price_impact_bps = core.price_impact_bps();
        let amount_out_net_gas = core.amount_out_net_gas().clone();
        let block = core.block().clone().into();
        let gas_price = core.gas_price().cloned();
        let transaction = core
            .transaction()
            .cloned()
            .map(Into::into);
        let route = core.into_route().map(Into::into);
        Self {
            order_id,
            status,
            route,
            amount_in,
            amount_out,
            gas_estimate,
            price_impact_bps,
            amount_out_net_gas,
            block,
            gas_price,
            transaction,
        }
    }
}

impl From<fynd_core::QuoteStatus> for QuoteStatus {
    fn from(core: fynd_core::QuoteStatus) -> Self {
        match core {
            fynd_core::QuoteStatus::Success => Self::Success,
            fynd_core::QuoteStatus::NoRouteFound => Self::NoRouteFound,
            fynd_core::QuoteStatus::InsufficientLiquidity => Self::InsufficientLiquidity,
            fynd_core::QuoteStatus::Timeout => Self::Timeout,
            fynd_core::QuoteStatus::NotReady => Self::NotReady,
        }
    }
}

impl From<fynd_core::BlockInfo> for BlockInfo {
    fn from(core: fynd_core::BlockInfo) -> Self {
        Self { number: core.number(), hash: core.hash().to_string(), timestamp: core.timestamp() }
    }
}

impl From<fynd_core::Route> for Route {
    fn from(core: fynd_core::Route) -> Self {
        Self {
            swaps: core
                .into_swaps()
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<fynd_core::Swap> for Swap {
    fn from(core: fynd_core::Swap) -> Self {
        Self {
            component_id: core.component_id().to_string(),
            protocol: core.protocol().to_string(),
            token_in: core.token_in().clone(),
            token_out: core.token_out().clone(),
            amount_in: core.amount_in().clone(),
            amount_out: core.amount_out().clone(),
            gas_estimate: core.gas_estimate().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::tycho_common::models::Address;

    use super::*;

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn test_solution_request_conversion_roundtrip() {
        let dto = QuoteRequest {
            orders: vec![Order {
                id: "test-id".to_string(),
                token_in: make_address(0x01),
                token_out: make_address(0x02),
                amount: BigUint::from(1000u64),
                side: OrderSide::Sell,
                sender: make_address(0xAA),
                receiver: None,
            }],
            options: QuoteOptions { timeout_ms: Some(5000), min_responses: None, max_gas: None },
        };

        let core: fynd_core::QuoteRequest = dto.clone().into();
        assert_eq!(core.orders().len(), 1);
        assert_eq!(core.orders()[0].id(), "test-id");
        assert_eq!(core.options().timeout_ms(), Some(5000));
    }

    #[test]
    fn test_solution_conversion() {
        let core: fynd_core::Quote = serde_json::from_str(
            r#"{"orders":[],"total_gas_estimate":"100000","solve_time_ms":50}"#,
        )
        .unwrap();

        let dto: Quote = core.into();
        assert_eq!(dto.total_gas_estimate, BigUint::from(100_000u64));
        assert_eq!(dto.solve_time_ms, 50);
    }

    #[test]
    fn test_order_side_conversion() {
        let dto = OrderSide::Sell;
        let core = order_side_to_core(dto);
        assert_eq!(core, fynd_core::OrderSide::Sell);
    }

    #[test]
    fn test_status_conversion() {
        let statuses = vec![
            (fynd_core::QuoteStatus::Success, QuoteStatus::Success),
            (fynd_core::QuoteStatus::NoRouteFound, QuoteStatus::NoRouteFound),
            (fynd_core::QuoteStatus::InsufficientLiquidity, QuoteStatus::InsufficientLiquidity),
            (fynd_core::QuoteStatus::Timeout, QuoteStatus::Timeout),
            (fynd_core::QuoteStatus::NotReady, QuoteStatus::NotReady),
        ];

        for (core, expected_dto) in statuses {
            let dto: QuoteStatus = core.into();
            assert_eq!(dto, expected_dto);
        }
    }
}
