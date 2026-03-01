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

pub(crate) fn solution_request_to_core(dto: SolutionRequest) -> fynd_core::SolutionRequest {
    fynd_core::SolutionRequest {
        orders: dto
            .orders
            .into_iter()
            .map(order_to_core)
            .collect(),
        options: solution_options_to_core(dto.options),
    }
}

fn solution_options_to_core(dto: SolutionOptions) -> fynd_core::SolutionOptions {
    fynd_core::SolutionOptions {
        timeout_ms: dto.timeout_ms,
        min_responses: dto.min_responses,
        max_gas: dto.max_gas,
    }
}

fn order_to_core(dto: Order) -> fynd_core::Order {
    fynd_core::Order {
        id: dto.id,
        token_in: dto.token_in,
        token_out: dto.token_out,
        amount: dto.amount,
        side: order_side_to_core(dto.side),
        sender: dto.sender,
        receiver: dto.receiver,
    }
}

fn order_side_to_core(dto: OrderSide) -> fynd_core::OrderSide {
    match dto {
        OrderSide::Sell => fynd_core::OrderSide::Sell,
    }
}

// ============================================================================
// CONVERSIONS: Core -> DTO
// ============================================================================

pub(crate) fn solution_from_core(core: fynd_core::Solution) -> Solution {
    Solution {
        orders: core
            .orders
            .into_iter()
            .map(order_solution_from_core)
            .collect(),
        total_gas_estimate: core.total_gas_estimate,
        solve_time_ms: core.solve_time_ms,
    }
}

fn order_solution_from_core(core: fynd_core::OrderSolution) -> OrderSolution {
    OrderSolution {
        order_id: core.order_id,
        status: solution_status_from_core(core.status),
        route: core.route.map(route_from_core),
        amount_in: core.amount_in,
        amount_out: core.amount_out,
        gas_estimate: core.gas_estimate,
        price_impact_bps: core.price_impact_bps,
        amount_out_net_gas: core.amount_out_net_gas,
        block: block_info_from_core(core.block),
    }
}

fn solution_status_from_core(core: fynd_core::SolutionStatus) -> SolutionStatus {
    match core {
        fynd_core::SolutionStatus::Success => SolutionStatus::Success,
        fynd_core::SolutionStatus::NoRouteFound => SolutionStatus::NoRouteFound,
        fynd_core::SolutionStatus::InsufficientLiquidity => SolutionStatus::InsufficientLiquidity,
        fynd_core::SolutionStatus::Timeout => SolutionStatus::Timeout,
        fynd_core::SolutionStatus::NotReady => SolutionStatus::NotReady,
    }
}

fn block_info_from_core(core: fynd_core::BlockInfo) -> BlockInfo {
    BlockInfo { number: core.number, hash: core.hash, timestamp: core.timestamp }
}

fn route_from_core(core: fynd_core::Route) -> Route {
    Route {
        swaps: core
            .swaps
            .into_iter()
            .map(swap_from_core)
            .collect(),
    }
}

fn swap_from_core(core: fynd_core::Swap) -> Swap {
    Swap {
        component_id: core.component_id,
        protocol: core.protocol,
        token_in: core.token_in,
        token_out: core.token_out,
        amount_in: core.amount_in,
        amount_out: core.amount_out,
        gas_estimate: core.gas_estimate,
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
        let dto = SolutionRequest {
            orders: vec![Order {
                id: "test-id".to_string(),
                token_in: make_address(0x01),
                token_out: make_address(0x02),
                amount: BigUint::from(1000u64),
                side: OrderSide::Sell,
                sender: make_address(0xAA),
                receiver: None,
            }],
            options: SolutionOptions {
                timeout_ms: Some(5000),
                min_responses: None,
                max_gas: None,
            },
        };

        let core = solution_request_to_core(dto.clone());
        assert_eq!(core.orders.len(), 1);
        assert_eq!(core.orders[0].id, "test-id");
        assert_eq!(core.options.timeout_ms, Some(5000));
    }

    #[test]
    fn test_solution_conversion() {
        let core = fynd_core::Solution {
            orders: vec![],
            total_gas_estimate: BigUint::from(100_000u64),
            solve_time_ms: 50,
        };

        let dto = solution_from_core(core);
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
            (fynd_core::SolutionStatus::Success, SolutionStatus::Success),
            (fynd_core::SolutionStatus::NoRouteFound, SolutionStatus::NoRouteFound),
            (
                fynd_core::SolutionStatus::InsufficientLiquidity,
                SolutionStatus::InsufficientLiquidity,
            ),
            (fynd_core::SolutionStatus::Timeout, SolutionStatus::Timeout),
            (fynd_core::SolutionStatus::NotReady, SolutionStatus::NotReady),
        ];

        for (core, expected_dto) in statuses {
            let dto = solution_status_from_core(core);
            assert_eq!(dto, expected_dto);
        }
    }
}
