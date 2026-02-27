use std::sync::Arc;

use alloy::eips::eip2718::Encodable2718;
use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use num_bigint::BigUint;
use reqwest::Client;

use crate::{
    error::FyndClientError,
    execution::{ExecutionReceipt, TransactionHandle},
    signing::{FyndPayload, SignablePayload, SignedOrder},
    types::{BlockInfo, Order, OrderSide, OrderSolution, Route, SolutionBackend, Swap},
    wire::{WireOrder, WireOrderSide, WireOrderSolution, WireSolutionRequest, WireSolutionStatus},
};

pub struct FyndClient {
    base_url: String,
    http: Client,
}

impl FyndClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), http: Client::new() }
    }

    /// Phase 1: Get a priced route from the solver.
    ///
    /// The returned quote is valid for the block it was computed in.
    /// Re-quote if submission is delayed by more than one or two blocks.
    pub async fn quote(&self, order: Order) -> Result<OrderSolution, FyndClientError> {
        let wire_order = to_wire_order(&order);
        let request = WireSolutionRequest { orders: vec![wire_order] };

        let response = self
            .http
            .post(format!("{}/v1/solve", self.base_url))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_default();
            return Err(FyndClientError::UnexpectedResponse(format!(
                "solver returned {}: {}",
                status, body
            )));
        }

        let wire_solution: crate::wire::WireSolution = response.json().await?;
        let wire_order_solution = wire_solution
            .orders
            .into_iter()
            .next()
            .ok_or_else(|| {
                FyndClientError::UnexpectedResponse("empty orders in response".to_string())
            })?;

        from_wire_order_solution(wire_order_solution)
    }

    /// Phase 2: Build the signable payload for a quote.
    ///
    /// Fetches current nonce and gas fees from the chain via `rpc_url`,
    /// then assembles an unsigned EIP-1559 transaction.
    ///
    /// The returned `SignablePayload` exposes `signing_hash()` — pass this to
    /// your external signer (hardware wallet, KMS, etc.) and then call
    /// `assemble_signed_order()` with the resulting signature.
    ///
    /// Note: the contract address is `Address::ZERO` and calldata is empty
    /// until the server populates these fields in a future update.
    pub async fn signable_payload(
        &self,
        solution: &OrderSolution,
        sender: Address,
        rpc_url: &str,
    ) -> Result<SignablePayload, FyndClientError> {
        let provider = build_provider(rpc_url).await?;

        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| FyndClientError::Rpc(e.to_string()))?;

        let nonce = provider
            .get_transaction_count(sender)
            .await
            .map_err(|e| FyndClientError::Rpc(e.to_string()))?;

        let fee_estimate = provider
            .estimate_eip1559_fees()
            .await
            .map_err(|e| FyndClientError::Rpc(e.to_string()))?;

        let gas_limit = solution
            .gas_estimate
            .to::<u64>()
            .saturating_add(50_000);

        let SolutionBackend::Fynd { ref calldata } = solution.backend;

        // Contract address is Address::ZERO and calldata is empty.
        // The server will populate these in a future update.
        let tx = alloy::consensus::TxEip1559 {
            chain_id,
            nonce,
            max_fee_per_gas: fee_estimate.max_fee_per_gas,
            max_priority_fee_per_gas: fee_estimate.max_priority_fee_per_gas,
            gas_limit,
            to: alloy::primitives::TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: alloy::primitives::Bytes::from(calldata.clone()),
            access_list: Default::default(),
        };

        Ok(SignablePayload::Fynd(FyndPayload { tx }))
    }

    /// Phase 3: Broadcast a signed order to the blockchain.
    ///
    /// Returns an `ExecutionReceipt` that can be `.settle()`d to wait for confirmation.
    pub async fn execute(
        &self,
        signed_order: SignedOrder,
        token_out: Address,
        receiver: Address,
        rpc_url: &str,
    ) -> Result<ExecutionReceipt, FyndClientError> {
        let provider = build_provider(rpc_url).await?;

        let SignedOrder::Fynd { envelope } = signed_order else {
            return Err(FyndClientError::UnexpectedResponse(
                "Turbine not yet implemented".to_string(),
            ));
        };

        let mut encoded = Vec::new();
        (*envelope).encode_2718(&mut encoded);

        let tx_hash = *provider
            .send_raw_transaction(&encoded)
            .await
            .map_err(|e| FyndClientError::Rpc(e.to_string()))?
            .tx_hash();

        Ok(ExecutionReceipt::Transaction(TransactionHandle {
            tx_hash,
            provider,
            token_out,
            receiver,
        }))
    }
}

/// Builds a type-erased provider from a URL string.
async fn build_provider(rpc_url: &str) -> Result<Arc<dyn Provider<Ethereum>>, FyndClientError> {
    let provider = ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .map_err(|e| FyndClientError::Rpc(e.to_string()))?;
    Ok(Arc::new(provider) as Arc<dyn Provider<Ethereum>>)
}

// ── Conversion helpers ────────────────────────────────────────────────────────

fn biguint_from_u256(v: U256) -> BigUint {
    BigUint::from_bytes_be(&v.to_be_bytes::<32>())
}

fn u256_from_biguint(v: &BigUint) -> U256 {
    let bytes = v.to_bytes_be();
    // BigUint values from the solver are token amounts and gas estimates;
    // values exceeding U256 are impossible in practice.
    assert!(bytes.len() <= 32, "BigUint from server exceeds U256 range");
    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    U256::from_be_bytes(padded)
}

fn parse_address(s: &str) -> Result<Address, FyndClientError> {
    s.parse::<Address>()
        .map_err(|e| FyndClientError::UnexpectedResponse(e.to_string()))
}

fn to_wire_order(order: &Order) -> WireOrder {
    WireOrder {
        token_in: format!("{:#x}", order.token_in),
        token_out: format!("{:#x}", order.token_out),
        amount: biguint_from_u256(order.amount),
        side: match order.side {
            OrderSide::Sell => WireOrderSide::Sell,
        },
        sender: format!("{:#x}", order.sender),
        receiver: order
            .receiver
            .map(|a| format!("{:#x}", a)),
    }
}

fn from_wire_order_solution(w: WireOrderSolution) -> Result<OrderSolution, FyndClientError> {
    match w.status {
        WireSolutionStatus::Success => {}
        WireSolutionStatus::NoRouteFound => {
            return Err(FyndClientError::NoRouteFound { order_id: w.order_id })
        }
        WireSolutionStatus::InsufficientLiquidity => {
            return Err(FyndClientError::InsufficientLiquidity { order_id: w.order_id })
        }
        WireSolutionStatus::Timeout => {
            return Err(FyndClientError::Timeout { order_id: w.order_id })
        }
        WireSolutionStatus::NotReady => return Err(FyndClientError::NotReady),
    }

    let route = w
        .route
        .map(|r| {
            r.swaps
                .into_iter()
                .map(|s| {
                    Ok(Swap {
                        component_id: s.component_id,
                        protocol: s.protocol,
                        token_in: parse_address(&s.token_in)?,
                        token_out: parse_address(&s.token_out)?,
                        amount_in: u256_from_biguint(&s.amount_in),
                        amount_out: u256_from_biguint(&s.amount_out),
                        gas_estimate: u256_from_biguint(&s.gas_estimate),
                    })
                })
                .collect::<Result<Vec<_>, FyndClientError>>()
                .map(|swaps| Route { swaps })
        })
        .transpose()?;

    Ok(OrderSolution {
        order_id: w.order_id,
        amount_in: u256_from_biguint(&w.amount_in),
        amount_out: u256_from_biguint(&w.amount_out),
        gas_estimate: u256_from_biguint(&w.gas_estimate),
        price_impact_bps: w.price_impact_bps,
        block: BlockInfo {
            number: w.block.number,
            hash: w.block.hash,
            timestamp: w.block.timestamp,
        },
        route,
        backend: SolutionBackend::Fynd { calldata: vec![] },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{WireBlockInfo, WireOrderSolution, WireSolutionStatus};
    use num_bigint::BigUint;

    fn make_wire_solution_success() -> WireOrderSolution {
        WireOrderSolution {
            order_id: "test-order-1".to_string(),
            status: WireSolutionStatus::Success,
            route: None,
            amount_in: BigUint::from(1_000_000_000_000_000_000u64),
            amount_out: BigUint::from(3_500_000_000u64),
            gas_estimate: BigUint::from(150_000u64),
            price_impact_bps: Some(10),
            amount_out_net_gas: BigUint::from(3_498_000_000u64),
            block: WireBlockInfo {
                number: 21_000_000,
                hash: "0xabcd".to_string(),
                timestamp: 1_730_000_000,
            },
        }
    }

    #[test]
    fn test_to_wire_order_sell() {
        let order = Order {
            token_in: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                .parse()
                .expect("valid address"),
            token_out: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                .parse()
                .expect("valid address"),
            amount: U256::from(1_000_000_000_000_000_000u64),
            side: OrderSide::Sell,
            sender: "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
                .parse()
                .expect("valid address"),
            receiver: None,
        };

        let wire = to_wire_order(&order);

        assert_eq!(wire.token_in.to_lowercase(), "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        assert_eq!(wire.token_out.to_lowercase(), "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        assert_eq!(wire.amount, BigUint::from(1_000_000_000_000_000_000u64));
        assert!(wire.receiver.is_none());
    }

    #[test]
    fn test_to_wire_order_with_receiver() {
        let receiver: Address = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
            .parse()
            .expect("valid address");
        let order = Order {
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount: U256::from(100u64),
            side: OrderSide::Sell,
            sender: Address::ZERO,
            receiver: Some(receiver),
        };

        let wire = to_wire_order(&order);
        assert!(wire.receiver.is_some());
    }

    #[test]
    fn test_from_wire_order_solution_success() {
        let wire = make_wire_solution_success();
        let solution = from_wire_order_solution(wire).expect("should succeed");

        assert_eq!(solution.order_id, "test-order-1");
        assert_eq!(solution.amount_in, U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(solution.amount_out, U256::from(3_500_000_000u64));
        assert_eq!(solution.gas_estimate, U256::from(150_000u64));
        assert_eq!(solution.price_impact_bps, Some(10));
        assert_eq!(solution.block.number, 21_000_000);
        assert!(solution.route.is_none());
    }

    #[test]
    fn test_from_wire_order_solution_no_route() {
        let wire = WireOrderSolution {
            status: WireSolutionStatus::NoRouteFound,
            ..make_wire_solution_success()
        };
        let err = from_wire_order_solution(wire).expect_err("should be error");
        assert!(matches!(err, FyndClientError::NoRouteFound { .. }));
    }

    #[test]
    fn test_from_wire_order_solution_insufficient_liquidity() {
        let wire = WireOrderSolution {
            status: WireSolutionStatus::InsufficientLiquidity,
            ..make_wire_solution_success()
        };
        let err = from_wire_order_solution(wire).expect_err("should be error");
        assert!(matches!(err, FyndClientError::InsufficientLiquidity { .. }));
    }

    #[test]
    fn test_from_wire_order_solution_timeout() {
        let wire = WireOrderSolution {
            status: WireSolutionStatus::Timeout,
            ..make_wire_solution_success()
        };
        let err = from_wire_order_solution(wire).expect_err("should be error");
        assert!(matches!(err, FyndClientError::Timeout { .. }));
    }

    #[test]
    fn test_from_wire_order_solution_not_ready() {
        let wire = WireOrderSolution {
            status: WireSolutionStatus::NotReady,
            ..make_wire_solution_success()
        };
        let err = from_wire_order_solution(wire).expect_err("should be error");
        assert!(matches!(err, FyndClientError::NotReady));
    }

    #[test]
    fn test_biguint_u256_roundtrip() {
        let original = U256::from(1_000_000_000_000_000_000u64);
        let biguint = biguint_from_u256(original);
        let back = u256_from_biguint(&biguint);
        assert_eq!(original, back);
    }

    #[test]
    fn test_biguint_u256_zero() {
        let original = U256::ZERO;
        let biguint = biguint_from_u256(original);
        let back = u256_from_biguint(&biguint);
        assert_eq!(original, back);
    }
}
