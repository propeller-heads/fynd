use num_bigint::BigUint;
use num_traits::Zero;
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::{Solution as ExecutionSolution, Swap, UserTransferType},
    tycho_encoder::TychoEncoder,
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

use crate::{
    feed::market_data::SharedMarketDataRef,
    types::solution::{
        EncodedSwapData, EncodingOptions, Order, OrderSolution, SolutionStatus, TransferType,
    },
    Solution,
};

const BPS: u32 = 10_000u32;

#[derive(Debug, thiserror::Error)]
pub enum EncodingError {
    #[error("component not found in market data: {id}")]
    MissingComponent { id: String },
    #[error("route is empty")]
    EmptyRoute,
    #[error(
        "checked_amount is zero after applying slippage \
         (amount_out too small for slippage_bps={slippage_bps}). \
         Minimum checked amount is one."
    )]
    ZeroCheckedAmount { slippage_bps: u32 },
    #[error("encoding failed: {0}")]
    EncodingFailed(String),
}

/// Encodes solved routes into on-chain calldata using the Tycho router.
///
/// Created once at startup and reused for all requests. Holds a reference
/// to shared market data for looking up `ProtocolComponent` during encoding.
///
/// Two internal encoders are built — one per [`TransferType`] variant — so
/// that per-request transfer semantics are supported without rebuilding.
pub struct SwapEncoder {
    transfer_from_encoder: Box<dyn TychoEncoder>,
    none_encoder: Box<dyn TychoEncoder>,
    market_data: SharedMarketDataRef,
}

impl SwapEncoder {
    /// Creates a swap encoder for the given chain.
    pub fn new(
        chain: Chain,
        market_data: SharedMarketDataRef,
    ) -> Result<Self, EncodingError> {
        let build =
            |transfer_type: UserTransferType| -> Result<Box<dyn TychoEncoder>, EncodingError> {
                let registry = SwapEncoderRegistry::new(chain)
                    .add_default_encoders(None)
                    .map_err(|e| {
                        EncodingError::EncodingFailed(format!(
                            "failed to create SwapEncoderRegistry: {e}"
                        ))
                    })?;

                TychoRouterEncoderBuilder::new()
                    .chain(chain)
                    .user_transfer_type(transfer_type)
                    .swap_encoder_registry(registry)
                    .build()
                    .map_err(|e| {
                        EncodingError::EncodingFailed(format!(
                            "failed to build encoder: {e}"
                        ))
                    })
            };

        Ok(Self {
            transfer_from_encoder: build(UserTransferType::TransferFrom)?,
            none_encoder: build(UserTransferType::None)?,
            market_data,
        })
    }

    /// Encodes all successful orders in a solution.
    pub async fn encode_solution(
        &self,
        orders: &[Order],
        mut solution: Solution,
        options: &EncodingOptions,
    ) -> Result<Solution, EncodingError> {
        assert_eq!(
            solution.orders.len(),
            orders.len(),
            "solution and orders must have the same length"
        );

        for (order_solution, order) in solution.orders.iter_mut().zip(orders) {
            if order_solution.status != SolutionStatus::Success {
                continue;
            }
            order_solution.encoding =
                Some(self.encode_order(order, order_solution, options).await?);
        }
        Ok(solution)
    }

    /// Encodes a single order's route into calldata for the Tycho router.
    async fn encode_order(
        &self,
        order: &Order,
        solution: &OrderSolution,
        options: &EncodingOptions,
    ) -> Result<EncodedSwapData, EncodingError> {
        if solution.status != SolutionStatus::Success {
            return Err(EncodingError::EmptyRoute);
        }

        let route = solution.route.as_ref().ok_or(EncodingError::EmptyRoute)?;

        if route.swaps.is_empty() {
            return Err(EncodingError::EmptyRoute);
        }

        let market_data = self.market_data.read().await;
        let mut execution_swaps = Vec::with_capacity(route.swaps.len());
        for swap in &route.swaps {
            let component = market_data
                .get_component(&swap.component_id)
                .ok_or_else(|| EncodingError::MissingComponent {
                    id: swap.component_id.clone(),
                })?;

            execution_swaps.push(Swap::new(
                component.clone(),
                Bytes::from(swap.token_in.as_ref()),
                Bytes::from(swap.token_out.as_ref()),
            ));
        }

        let checked_amount =
            calculate_min_amount_out(&solution.amount_out, options.slippage_bps);
        if checked_amount.is_zero() && !solution.amount_out.is_zero() {
            return Err(EncodingError::ZeroCheckedAmount {
                slippage_bps: options.slippage_bps,
            });
        }

        let sender = Bytes::from(order.sender.as_ref());
        let receiver = Bytes::from(order.effective_receiver().as_ref());

        let execution_solution = ExecutionSolution {
            sender: sender.clone(),
            receiver,
            given_token: Bytes::from(order.token_in.as_ref()),
            given_amount: order.amount.clone(),
            checked_token: Bytes::from(order.token_out.as_ref()),
            exact_out: false,
            checked_amount: checked_amount.clone(),
            swaps: execution_swaps,
            ..Default::default()
        };

        let encoder = match options.transfer_type {
            TransferType::TransferFrom => self.transfer_from_encoder.as_ref(),
            TransferType::None => self.none_encoder.as_ref(),
        };

        let encoded_solution = encoder
            .encode_solutions(vec![execution_solution])
            .map_err(|e| {
                EncodingError::EncodingFailed(format!(
                    "encode_solutions failed: {e}"
                ))
            })?
            .into_iter()
            .next()
            .ok_or_else(|| {
                EncodingError::EncodingFailed(
                    "encoder returned empty result".to_string(),
                )
            })?;

        Ok(EncodedSwapData::new(&encoded_solution, checked_amount))
    }
}

fn calculate_min_amount_out(
    expected_amount: &BigUint,
    slippage_bps: u32,
) -> BigUint {
    let bps = BigUint::from(BPS);
    let multiplier = &bps - BigUint::from(slippage_bps);
    (expected_amount * &multiplier) / &bps
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use chrono::NaiveDateTime;
    use tokio::sync::RwLock;
    use tycho_simulation::tycho_common::models::{protocol::ProtocolComponent, Chain};

    use super::*;
    use crate::{
        feed::market_data::SharedMarketData,
        types::solution::{
            BlockInfo, Order, OrderSide, OrderSolution, Route, Swap as FyndSwap,
        },
    };

    fn usdc_bytes() -> [u8; 20] {
        let addr = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&hex::decode(&addr[2..]).expect("valid hex"));
        bytes
    }

    fn dai_bytes() -> [u8; 20] {
        let addr = "0x6B175474E89094C44Da98b954EedeAC495271d0F";
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&hex::decode(&addr[2..]).expect("valid hex"));
        bytes
    }

    const POOL_DAI_USDC: &str = "0xae461ca67b15dc8dc81ce7615e0320da1a9ab8d5";

    fn make_address(
        bytes: [u8; 20],
    ) -> tycho_simulation::tycho_common::models::Address {
        tycho_simulation::tycho_common::models::Address::from(bytes)
    }

    fn create_univ2_component(
        pool_id: &str,
        token_a: [u8; 20],
        token_b: [u8; 20],
    ) -> ProtocolComponent {
        ProtocolComponent::new(
            pool_id,
            "uniswap_v2",
            "uniswap_v2_pool",
            Chain::Ethereum,
            vec![make_address(token_a), make_address(token_b)],
            vec![],
            HashMap::new(),
            Default::default(),
            Default::default(),
            NaiveDateTime::default(),
        )
    }

    fn create_encoder_with_component(
        component: ProtocolComponent,
    ) -> (SwapEncoder, SharedMarketDataRef) {
        let mut market_data = SharedMarketData::new();
        market_data.upsert_components(vec![component]);
        let shared = Arc::new(RwLock::new(market_data));
        let encoder = SwapEncoder::new(Chain::Ethereum, Arc::clone(&shared))
            .expect("failed to create encoder");
        (encoder, shared)
    }

    fn make_order(
        token_in: [u8; 20],
        token_out: [u8; 20],
        amount: u64,
    ) -> Order {
        Order {
            id: "test-order".to_string(),
            token_in: make_address(token_in),
            token_out: make_address(token_out),
            amount: BigUint::from(amount),
            side: OrderSide::Sell,
            sender: make_address([0xAA; 20]),
            receiver: None,
        }
    }

    fn make_successful_solution(
        component_id: &str,
        token_in: [u8; 20],
        token_out: [u8; 20],
        amount_in: u64,
        amount_out: u64,
    ) -> OrderSolution {
        OrderSolution {
            order_id: "test-order".to_string(),
            status: SolutionStatus::Success,
            route: Some(Route::new(vec![FyndSwap::new(
                component_id.to_string(),
                "uniswap_v2".to_string(),
                make_address(token_in),
                make_address(token_out),
                BigUint::from(amount_in),
                BigUint::from(amount_out),
                BigUint::from(120_000u64),
            )])),
            amount_in: BigUint::from(amount_in),
            amount_out: BigUint::from(amount_out),
            gas_estimate: BigUint::from(120_000u64),
            price_impact_bps: None,
            amount_out_net_gas: BigUint::from(amount_out),
            block: BlockInfo {
                number: 1,
                hash: "0x123".to_string(),
                timestamp: 1000,
            },
            encoding: None,
            algorithm: "test".to_string(),
        }
    }

    #[test]
    fn test_calculate_min_amount_out() {
        let test_cases = vec![
            (1_000_000u64, 50, 995_000u64, "50 bps slippage"),
            (1_000_000u64, BPS, 0u64, "100 percent slippage yields zero"),
            (1_000_000u64, 0, 1_000_000u64, "zero bps slippage"),
            (99u64, 50, 98u64, "small amount with slippage"),
            (0u64, 50, 0u64, "zero amount"),
        ];

        for (amount, slippage_bps, expected, description) in test_cases {
            let amount = BigUint::from(amount);
            let result = calculate_min_amount_out(&amount, slippage_bps);
            assert_eq!(
                result,
                BigUint::from(expected),
                "failed for: {}",
                description
            );
        }
    }

    #[test]
    fn encoder_creates_successfully() {
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let result = SwapEncoder::new(Chain::Ethereum, market_data);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn encode_order_univ2_produces_calldata() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _market_data) =
            create_encoder_with_component(component);

        let order = make_order(usdc_bytes(), dai_bytes(), 100_000_000);
        let solution = make_successful_solution(
            POOL_DAI_USDC,
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            99_795_590,
        );
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let result = encoder.encode_order(&order, &solution, &opts).await;
        assert!(result.is_ok(), "encoding failed: {:?}", result.err());

        let encoded = result.expect("already checked");
        assert!(
            !encoded.encoded_calldata.is_empty(),
            "calldata should contain actual data"
        );
        // checked_amount = 99_795_590 * 9950 / 10000 = 99_296_612
        assert_eq!(
            encoded.checked_amount,
            BigUint::from(99_296_612u64)
        );
    }

    #[tokio::test]
    async fn encode_order_returns_error_for_missing_component() {
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let encoder =
            SwapEncoder::new(Chain::Ethereum, Arc::clone(&market_data))
                .expect("failed to create encoder");

        let order = make_order(usdc_bytes(), dai_bytes(), 100_000_000);
        let solution = make_successful_solution(
            "0xdeadbeef",
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            99_000_000,
        );
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let result = encoder.encode_order(&order, &solution, &opts).await;
        assert!(matches!(
            result,
            Err(EncodingError::MissingComponent { .. })
        ));
    }

    #[tokio::test]
    async fn encode_order_returns_error_for_empty_route() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _md) = create_encoder_with_component(component);

        let order = make_order(usdc_bytes(), dai_bytes(), 100_000_000);
        let mut solution = make_successful_solution(
            POOL_DAI_USDC,
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            99_000_000,
        );
        solution.route = Some(Route::new(vec![]));
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let result = encoder.encode_order(&order, &solution, &opts).await;
        assert!(matches!(result, Err(EncodingError::EmptyRoute)));
    }

    #[tokio::test]
    async fn encode_order_returns_error_for_non_success_status() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _md) = create_encoder_with_component(component);

        let order = make_order(usdc_bytes(), dai_bytes(), 100_000_000);
        let mut solution = make_successful_solution(
            POOL_DAI_USDC,
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            99_000_000,
        );
        solution.status = SolutionStatus::NoRouteFound;
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let result = encoder.encode_order(&order, &solution, &opts).await;
        assert!(matches!(result, Err(EncodingError::EmptyRoute)));
    }

    #[tokio::test]
    async fn encode_order_zero_checked_amount_returns_error() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _md) = create_encoder_with_component(component);

        let order = make_order(usdc_bytes(), dai_bytes(), 100_000_000);
        let mut solution = make_successful_solution(
            POOL_DAI_USDC,
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            1,
        );
        solution.amount_out = BigUint::from(1u64);
        let opts = EncodingOptions {
            slippage_bps: 10_000,
            transfer_type: TransferType::default(),
        };

        let result = encoder.encode_order(&order, &solution, &opts).await;
        assert!(matches!(
            result,
            Err(EncodingError::ZeroCheckedAmount { .. })
        ));
    }

    #[tokio::test]
    async fn encode_order_with_explicit_receiver() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _md) = create_encoder_with_component(component);

        let mut order = make_order(usdc_bytes(), dai_bytes(), 100_000_000);
        order.receiver = Some(make_address([0xBB; 20]));

        let solution = make_successful_solution(
            POOL_DAI_USDC,
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            99_000_000,
        );
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let result = encoder.encode_order(&order, &solution, &opts).await;
        assert!(
            result.is_ok(),
            "encoding with explicit receiver should work"
        );
    }

    #[tokio::test]
    async fn encode_solution_encodes_successful_orders() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _md) = create_encoder_with_component(component);

        let orders = vec![make_order(usdc_bytes(), dai_bytes(), 100_000_000)];
        let solution = Solution {
            orders: vec![make_successful_solution(
                POOL_DAI_USDC,
                usdc_bytes(),
                dai_bytes(),
                100_000_000,
                99_000_000,
            )],
            total_gas_estimate: BigUint::from(120_000u64),
            solve_time_ms: 10,
        };
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let solution = encoder
            .encode_solution(&orders, solution, &opts)
            .await
            .expect("encoding should succeed");

        assert!(
            solution.orders[0].encoding.is_some(),
            "successful order should be encoded"
        );
    }

    #[tokio::test]
    async fn encode_solution_skips_failed_orders() {
        let component =
            create_univ2_component(POOL_DAI_USDC, dai_bytes(), usdc_bytes());
        let (encoder, _md) = create_encoder_with_component(component);

        let orders = vec![make_order(usdc_bytes(), dai_bytes(), 100_000_000)];
        let mut failed_solution = make_successful_solution(
            POOL_DAI_USDC,
            usdc_bytes(),
            dai_bytes(),
            100_000_000,
            99_000_000,
        );
        failed_solution.status = SolutionStatus::NoRouteFound;
        failed_solution.route = None;

        let solution = Solution {
            orders: vec![failed_solution],
            total_gas_estimate: BigUint::ZERO,
            solve_time_ms: 10,
        };
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let solution = encoder
            .encode_solution(&orders, solution, &opts)
            .await
            .expect("encoding should succeed (no successful orders)");

        assert!(
            solution.orders[0].encoding.is_none(),
            "failed order should not be encoded"
        );
    }

    #[tokio::test]
    async fn encode_solution_returns_error_for_missing_component() {
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let encoder =
            SwapEncoder::new(Chain::Ethereum, Arc::clone(&market_data))
                .expect("failed to create encoder");

        let orders = vec![make_order(usdc_bytes(), dai_bytes(), 100_000_000)];
        let solution = Solution {
            orders: vec![make_successful_solution(
                "0xnonexistent",
                usdc_bytes(),
                dai_bytes(),
                100_000_000,
                99_000_000,
            )],
            total_gas_estimate: BigUint::from(120_000u64),
            solve_time_ms: 10,
        };
        let opts = EncodingOptions {
            slippage_bps: 50,
            transfer_type: TransferType::default(),
        };

        let result =
            encoder.encode_solution(&orders, solution, &opts).await;

        assert!(matches!(
            result,
            Err(EncodingError::MissingComponent { .. })
        ));
    }
}
