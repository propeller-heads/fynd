use alloy::{primitives::Keccak256, sol_types::SolValue};
use num_bigint::BigUint;
use tycho_execution::encoding::{
    errors::EncodingError,
    evm::{
        approvals::permit2::PermitSingle,
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{biguint_to_u256, bytes_to_address},
    },
    models::{
        EncodedSolution, NativeAction, PermitDetails as ModelsPermitDetails,
        PermitSingle as ModelsPermitSingle, Solution, Swap, UserTransferType,
    },
    tycho_encoder::TychoEncoder,
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

use crate::{
    types::{EncodingOptions, OrderSolution},
    SolutionStatus, SolveError, Transaction,
};

/// Encodes solution into tycho compatible transactions.
///
/// # Fields
/// * `tycho_encoder` - Encoder created using the configured chain for encoding solutions into tycho
///   compatible transactions
/// * `chain` - Chain to be used.
pub struct Encoder {
    tycho_encoder: Box<dyn TychoEncoder>,
    chain: Chain,
}

impl From<&OrderSolution> for Solution {
    fn from(order_solution: &OrderSolution) -> Self {
        let swaps = order_solution
            .route
            .as_ref()
            .map(|r| {
                r.swaps
                    .iter()
                    .map(|s| {
                        Swap::new(
                            s.protocol_component.clone(),
                            Bytes::from(s.token_in.as_ref()),
                            Bytes::from(s.token_out.as_ref()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        Solution {
            sender: order_solution.sender.clone(),
            receiver: order_solution.receiver.clone(),
            given_token: Bytes::from(order_solution.token_in.as_ref()),
            given_amount: order_solution.amount_in.clone(),
            checked_token: Bytes::from(order_solution.token_out.as_ref()),
            exact_out: false,
            checked_amount: order_solution.amount_out.clone(),
            swaps,
            // TODO: remove once router v3 is released
            native_action: None,
        }
    }
}

impl Encoder {
    /// Creates a new `Encoder` for the given chain.
    ///
    /// # Arguments
    /// * `chain` - Chain to encode solutions for.
    /// * `swap_encoder_registry` - Registry of swap encoders for supported protocols.
    ///
    /// # Returns
    /// A new `Encoder` configured with `TransferFrom` user transfer type.
    pub fn new(
        chain: Chain,
        swap_encoder_registry: SwapEncoderRegistry,
    ) -> Result<Self, SolveError> {
        Ok(Self {
            tycho_encoder: TychoRouterEncoderBuilder::new()
                .chain(chain)
                .user_transfer_type(UserTransferType::TransferFrom)
                .swap_encoder_registry(swap_encoder_registry)
                .build()?,
            chain,
        })
    }

    /// Encodes order solutions for execution.
    ///
    /// # Arguments
    /// * `solutions` - Array containing order solutions.
    /// * `encoding_options` - Additional context needed for encoding.
    ///
    /// # Returns
    /// Input order solutions with the encoded transaction added to each successful solution.
    pub async fn encode(
        &self,
        mut solutions: Vec<OrderSolution>,
        encoding_options: EncodingOptions,
    ) -> Result<Vec<OrderSolution>, SolveError> {
        let mut solutions_to_encode: Vec<(usize, Solution)> = Vec::new();

        for (i, order_solution) in solutions.iter().enumerate() {
            if order_solution.status != SolutionStatus::Success {
                continue;
            }

            solutions_to_encode.push((i, Solution::from(order_solution)));
        }

        let encoded_solutions = self.tycho_encoder.encode_solutions(
            solutions_to_encode
                .iter()
                .map(|(_, s)| s.clone())
                .collect(),
        )?;

        for (encoded_solution, (idx, solution)) in encoded_solutions
            .into_iter()
            .zip(solutions_to_encode)
        {
            let transaction = self.encode_tycho_router_call(
                encoded_solution,
                &solution,
                &encoding_options.transfer_type,
                encoding_options.clone(),
            )?;
            solutions[idx].transaction = Some(transaction);
        }

        Ok(solutions)
    }

    /// Encodes a call using one of its supported swap methods.
    ///
    /// Selects the appropriate router function (`singleSwap`, `singleSwapPermit2`,
    /// `sequentialSwap`, or `sequentialSwapPermit2`) based on the function signature in
    /// `encoded_solution`, prepends the 4-byte selector, and returns a `Transaction` ready
    /// for submission.
    fn encode_tycho_router_call(
        &self,
        encoded_solution: EncodedSolution,
        solution: &Solution,
        user_transfer_type: &UserTransferType,
        encoding_options: EncodingOptions,
    ) -> Result<Transaction, EncodingError> {
        let (mut unwrap, mut wrap) = (false, false);
        if let Some(action) = solution.native_action.clone() {
            match action {
                NativeAction::Wrap => wrap = true,
                NativeAction::Unwrap => unwrap = true,
            }
        }

        let given_amount = biguint_to_u256(&solution.given_amount);
        let precision = BigUint::from(1_000_000u64);
        let slippage_amount = solution.checked_amount.clone() *
            BigUint::from((encoding_options.slippage * 1_000_000.0) as u64) /
            &precision;
        let min_amount_out = biguint_to_u256(&(solution.checked_amount.clone() - slippage_amount));
        let given_token = bytes_to_address(&solution.given_token)?;
        let checked_token = bytes_to_address(&solution.checked_token)?;
        let receiver = bytes_to_address(&solution.receiver)?;
        let (permit, signature) = if let Some(p) = encoding_options.permit {
            let models_permit = ModelsPermitSingle {
                details: ModelsPermitDetails {
                    token: p.details.token,
                    amount: p.details.amount,
                    expiration: p.details.expiration,
                    nonce: p.details.nonce,
                },
                spender: p.spender,
                sig_deadline: p.sig_deadline,
            };
            let permit = Some(
                PermitSingle::try_from(&models_permit)
                    .map_err(|_| EncodingError::InvalidInput("Invalid permit".to_string()))?,
            );
            let signature = if let Some(sig) = encoding_options.permit2_signature {
                sig
            } else {
                return Err(EncodingError::FatalError(
                    "Signature must be provided for permit2".to_string(),
                ));
            };
            (permit, signature.to_vec())
        } else {
            (None, vec![])
        };

        let method_calldata = if encoded_solution
            .function_signature
            .contains("singleSwapPermit2")
        {
            (
                given_amount,
                given_token,
                checked_token,
                min_amount_out,
                wrap,
                unwrap,
                receiver,
                permit.ok_or(EncodingError::FatalError(
                    "permit2 object must be set to use permit2".to_string(),
                ))?,
                signature,
                encoded_solution.swaps,
            )
                .abi_encode()
        } else if encoded_solution
            .function_signature
            .contains("singleSwap")
        {
            (
                given_amount,
                given_token,
                checked_token,
                min_amount_out,
                wrap,
                unwrap,
                receiver,
                user_transfer_type == &UserTransferType::TransferFrom,
                encoded_solution.swaps,
            )
                .abi_encode()
        } else if encoded_solution
            .function_signature
            .contains("sequentialSwapPermit2")
        {
            (
                given_amount,
                given_token,
                checked_token,
                min_amount_out,
                wrap,
                unwrap,
                receiver,
                permit.ok_or(EncodingError::FatalError(
                    "permit2 object must be set to use permit2".to_string(),
                ))?,
                signature,
                encoded_solution.swaps,
            )
                .abi_encode()
        } else if encoded_solution
            .function_signature
            .contains("sequentialSwap")
        {
            (
                given_amount,
                given_token,
                checked_token,
                min_amount_out,
                wrap,
                unwrap,
                receiver,
                user_transfer_type == &UserTransferType::TransferFrom,
                encoded_solution.swaps,
            )
                .abi_encode()
        } else {
            Err(EncodingError::FatalError(
                "Invalid function signature for Tycho router".to_string(),
            ))?
        };

        let native_address = &self.chain.native_token().address;
        let contract_interaction =
            Self::encode_input(&encoded_solution.function_signature, method_calldata);
        let value = if solution.given_token == *native_address {
            solution.given_amount.clone()
        } else {
            BigUint::ZERO
        };
        Ok(Transaction { to: encoded_solution.interacting_with, value, data: contract_interaction })
    }

    /// Encodes the input data for a function call to the given function selector.
    fn encode_input(selector: &str, mut encoded_args: Vec<u8>) -> Vec<u8> {
        let mut hasher = Keccak256::new();
        hasher.update(selector.as_bytes());
        let selector_bytes = &hasher.finalize()[..4];
        let mut call_data = selector_bytes.to_vec();
        // Remove extra prefix if present (32 bytes for dynamic data)
        // Alloy encoding is including a prefix for dynamic data indicating the offset or length
        // but at this point we don't want that
        if encoded_args.len() > 32 &&
            encoded_args[..32] ==
                [0u8; 31]
                    .into_iter()
                    .chain([32].to_vec())
                    .collect::<Vec<u8>>()
        {
            encoded_args = encoded_args[32..].to_vec();
        }
        call_data.extend(encoded_args);
        call_data
    }
}

impl From<EncodingError> for SolveError {
    fn from(err: EncodingError) -> Self {
        SolveError::FailedEncoding(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_execution::encoding::{
        errors::EncodingError,
        models::{EncodedSolution, Solution},
        tycho_encoder::TychoEncoder,
    };
    use tycho_simulation::tycho_core::{
        models::{token::Token, Address, Chain as SimChain},
        Bytes,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component, MockProtocolSim},
        BlockInfo, OrderSolution, SolutionStatus,
    };

    fn make_route_swap_addrs(token_in: Address, token_out: Address) -> crate::types::Swap {
        let make_token = |addr: Address| Token {
            address: addr,
            symbol: "T".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: SimChain::Ethereum,
            quality: 100,
        };
        let tin = make_token(token_in.clone());
        let tout = make_token(token_out.clone());
        crate::types::Swap {
            component_id: "pool-1".to_string(),
            protocol: "uniswap_v2".to_string(),
            token_in,
            token_out,
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(990u64),
            gas_estimate: BigUint::from(50_000u64),
            protocol_component: component("test-pool", &[tin, tout]),
            protocol_state: Box::new(MockProtocolSim::default()),
        }
    }

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order_solution(token_in: Address, token_out: Address) -> OrderSolution {
        OrderSolution {
            order_id: "test-order".to_string(),
            token_in,
            token_out,
            status: SolutionStatus::Success,
            route: None,
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(990u64),
            gas_estimate: BigUint::from(100_000u64),
            price_impact_bps: None,
            amount_out_net_gas: BigUint::from(990u64),
            block: BlockInfo { number: 1, hash: "0x123".to_string(), timestamp: 1000 },
            algorithm: "test".to_string(),
            gas_price: None,
            transaction: None,
            sender: Bytes::from(make_address(0xAA).as_ref()),
            receiver: Bytes::from(make_address(0xAA).as_ref()),
        }
    }

    struct MockTychoEncoder {
        encoded_solutions: Vec<EncodedSolution>,
    }

    impl TychoEncoder for MockTychoEncoder {
        fn encode_solutions(
            &self,
            _solutions: Vec<Solution>,
        ) -> Result<Vec<EncodedSolution>, EncodingError> {
            Ok(self.encoded_solutions.clone())
        }

        fn encode_full_calldata(
            &self,
            _solutions: Vec<Solution>,
        ) -> Result<Vec<tycho_execution::encoding::models::Transaction>, EncodingError> {
            Ok(vec![])
        }

        fn validate_solution(&self, _solution: &Solution) -> Result<(), EncodingError> {
            Ok(())
        }
    }

    fn test_encoder(chain: Chain) -> Encoder {
        test_encoder_with_encoded_solutions(chain, vec![])
    }

    fn test_encoder_with_encoded_solutions(
        chain: Chain,
        encoded_solutions: Vec<EncodedSolution>,
    ) -> Encoder {
        Encoder { tycho_encoder: Box::new(MockTychoEncoder { encoded_solutions }), chain }
    }

    #[test]
    fn test_from_without_route_has_empty_swaps() {
        let order_solution = make_order_solution(make_address(0x01), make_address(0x02));

        let solution = Solution::from(&order_solution);

        assert_eq!(solution.given_token, Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(solution.checked_token, Bytes::from(make_address(0x02).as_ref()));
        assert!(solution.swaps.is_empty());
    }

    #[test]
    fn test_from_maps_tokens_and_amounts() {
        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x02));
        order_solution.route = Some(crate::types::Route::new(vec![make_route_swap_addrs(
            make_address(0x01),
            make_address(0x02),
        )]));

        let solution = Solution::from(&order_solution);

        assert_eq!(solution.given_token, Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(solution.checked_token, Bytes::from(make_address(0x02).as_ref()));
        assert_eq!(solution.given_amount, order_solution.amount_in);
        assert_eq!(solution.checked_amount, order_solution.amount_out);
        assert!(!solution.exact_out);
        assert_eq!(solution.native_action, None);
        assert_eq!(solution.swaps.len(), 1);
    }

    #[test]
    fn test_from_multi_hop_uses_boundary_swap_tokens() {
        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x03));
        order_solution.route = Some(crate::types::Route::new(vec![
            make_route_swap_addrs(make_address(0x01), make_address(0x02)),
            make_route_swap_addrs(make_address(0x02), make_address(0x03)),
        ]));

        let solution = Solution::from(&order_solution);

        assert_eq!(solution.given_token, Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(solution.checked_token, Bytes::from(make_address(0x03).as_ref()));
        assert_eq!(solution.swaps.len(), 2);
    }

    #[tokio::test]
    async fn test_encode_skips_non_successful_solutions() {
        let encoder = test_encoder(Chain::Ethereum);
        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x02));
        order_solution.status = SolutionStatus::NoRouteFound;

        let encoding_options = EncodingOptions {
            slippage: 0.01,
            transfer_type: UserTransferType::TransferFrom,
            permit: None,
            permit2_signature: None,
        };

        let result = encoder
            .encode(vec![order_solution], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction.is_none());
    }

    #[tokio::test]
    async fn test_encode_sets_transaction_on_successful_solution() {
        let encoded = EncodedSolution {
            function_signature:
                "singleSwap(uint256,address,address,uint256,bool,bool,address,bool,bytes)"
                    .to_string(),
            swaps: vec![1, 2, 3],
            interacting_with: Bytes::from(make_address(0xFF).as_ref()),
            n_tokens: 2,
            permit: None,
        };
        let encoder = test_encoder_with_encoded_solutions(Chain::Ethereum, vec![encoded]);

        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x02));
        order_solution.route = Some(crate::types::Route::new(vec![make_route_swap_addrs(
            make_address(0x01),
            make_address(0x02),
        )]));

        let encoding_options = EncodingOptions {
            slippage: 0.01,
            transfer_type: UserTransferType::TransferFrom,
            permit: None,
            permit2_signature: None,
        };

        let result = encoder
            .encode(vec![order_solution], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction.is_some());
        let tx = result[0].transaction.as_ref().unwrap();
        assert_eq!(tx.to, Bytes::from(make_address(0xFF).as_ref()));
        assert!(!tx.data.is_empty());
    }
}
