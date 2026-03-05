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

            solutions_to_encode.push((i, self.order_solution_to_solution(order_solution)?));
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
            let transaction = encode_tycho_router_call(
                encoded_solution,
                &solution,
                &encoding_options.transfer_type,
                &self.chain.native_token().address,
                encoding_options.clone(),
            )?;
            solutions[idx].transaction = Some(transaction);
        }

        Ok(solutions)
    }

    /// Maps an `OrderSolution` to the `Solution` type expected by TychoEncoder.
    fn order_solution_to_solution(
        &self,
        order_solution: &OrderSolution,
    ) -> Result<Solution, SolveError> {
        // Can use unwrap since status is successful status which means route exists
        let swaps = &order_solution
            .route
            .as_ref()
            .unwrap()
            .swaps;
        let first_swap = swaps
            .first()
            .ok_or_else(|| SolveError::Internal("route has no swaps".to_string()))?;
        let last_swap = swaps
            .last()
            .ok_or_else(|| SolveError::Internal("route has no swaps".to_string()))?;

        let token_in = Bytes::from(first_swap.token_in.as_ref());
        let token_out = Bytes::from(last_swap.token_out.as_ref());

        let amount_in = order_solution.amount_in.clone();
        let amount_out = order_solution.amount_out.clone();

        let native_action = self.native_action(order_solution, &token_in, &token_out)?;

        let (given_token, checked_token, given_amount, checked_amount) = if order_solution.exact_out
        {
            (token_out, token_in, amount_out, amount_in)
        } else {
            (token_in, token_out, amount_in, amount_out)
        };

        let solution_exec = Solution {
            sender: order_solution.sender.clone(),
            receiver: order_solution.receiver.clone(),
            given_token,
            given_amount,
            checked_token,
            exact_out: order_solution.exact_out,
            checked_amount,
            swaps: swaps
                .iter()
                .map(|s| {
                    Swap::new(
                        s.protocol_component.clone(),
                        Bytes::from(s.token_in.as_ref()),
                        Bytes::from(s.token_out.as_ref()),
                    )
                })
                .collect(),
            native_action,
        };
        Ok(solution_exec)
    }

    /// Determines whether a wrap or unwrap of the native token is required.
    ///
    /// Returns `Wrap` when the order sells the native token and the first swap expects the wrapped
    /// token, `Unwrap` when the order buys the native token and the last swap outputs the wrapped
    /// token, and `None` otherwise.
    fn native_action(
        &self,
        order_solution: &OrderSolution,
        first_swap_token_in: &Bytes,
        last_swap_token_out: &Bytes,
    ) -> Result<Option<NativeAction>, EncodingError> {
        if order_solution.token_in == self.chain.native_token().address &&
            *first_swap_token_in ==
                self.chain
                    .wrapped_native_token()
                    .address
        {
            Ok(Some(NativeAction::Wrap))
        } else if order_solution.token_out == self.chain.native_token().address &&
            *last_swap_token_out ==
                self.chain
                    .wrapped_native_token()
                    .address
        {
            Ok(Some(NativeAction::Unwrap))
        } else {
            Ok(None)
        }
    }
}

impl From<EncodingError> for SolveError {
    fn from(err: EncodingError) -> Self {
        SolveError::FailedEncoding(err.to_string())
    }
}

/// Encodes a call using one of its supported swap methods.
///
/// Selects the appropriate router function (`singleSwap`, `singleSwapPermit2`,
/// `sequentialSwap`, or `sequentialSwapPermit2`) based on the function signature in
/// `encoded_solution`, prepends the 4-byte selector, and returns a `Transaction` ready
/// for submission.
///
/// # Arguments
/// * `encoded_solution` - Output of `TychoEncoder::encode_solutions` for one solution.
/// * `solution` - Original solution providing token addresses, amounts, and native action.
/// * `user_transfer_type` - How tokens are transferred from the user to the router.
/// * `native_address` - Address used to identify the native token (e.g. ETH).
/// * `encoding_options` - Slippage, permit2 data, and other encoding options.
///
/// # Returns
/// A `Transaction` containing the ABI-encoded calldata and ETH value to send.
pub fn encode_tycho_router_call(
    encoded_solution: EncodedSolution,
    solution: &Solution,
    user_transfer_type: &UserTransferType,
    native_address: &Bytes,
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
        Err(EncodingError::FatalError("Invalid function signature for Tycho router".to_string()))?
    };

    let contract_interaction = encode_input(&encoded_solution.function_signature, method_calldata);
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_execution::encoding::{
        errors::EncodingError,
        models::{EncodedSolution, NativeAction, Solution},
        tycho_encoder::TychoEncoder,
    };
    use tycho_simulation::tycho_core::{models::Address, Bytes};

    use super::*;
    use crate::{BlockInfo, OrderSolution, SolutionStatus};

    fn eth() -> Bytes {
        Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap()
    }

    fn weth() -> Bytes {
        Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
    }

    fn make_route_swap(token_in_byte: u8, token_out_byte: u8) -> crate::types::Swap {
        make_route_swap_addrs(make_address(token_in_byte), make_address(token_out_byte))
    }

    fn make_route_swap_addrs(token_in: Address, token_out: Address) -> crate::types::Swap {
        use tycho_simulation::tycho_core::models::{token::Token, Chain as SimChain};

        use crate::algorithm::test_utils::{component, MockProtocolSim};

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

    fn addr_from_bytes(b: &Bytes) -> Address {
        let arr: [u8; 20] = <[u8; 20]>::try_from(b.as_ref()).expect("address must be 20 bytes");
        Address::from(arr)
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
            exact_out: false,
        }
    }

    struct MockTychoEncoder;

    impl TychoEncoder for MockTychoEncoder {
        fn encode_solutions(
            &self,
            _solutions: Vec<Solution>,
        ) -> Result<Vec<EncodedSolution>, EncodingError> {
            Ok(vec![])
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
        Encoder { tycho_encoder: Box::new(MockTychoEncoder), chain }
    }

    #[rstest]
    #[case::wrap(eth(), make_address(0x02), weth(), make_address(0x02), Some(NativeAction::Wrap))]
    #[case::unwrap(
        make_address(0x02),
        eth(),
        make_address(0x02),
        weth(),
        Some(NativeAction::Unwrap)
    )]
    #[case::none(
        make_address(0x01),
        make_address(0x02),
        make_address(0x01),
        make_address(0x02),
        None
    )]

    fn test_native_action(
        #[case] token_in: Bytes,
        #[case] token_out: Bytes,
        #[case] first_swap_token_in: Bytes,
        #[case] last_swap_token_out: Bytes,
        #[case] expected_result: Option<NativeAction>,
    ) {
        let chain = Chain::Ethereum;
        let encoder = test_encoder(chain);

        let order_solution = make_order_solution(token_in, token_out);

        let result = encoder
            .native_action(&order_solution, &first_swap_token_in, &last_swap_token_out)
            .unwrap();

        assert_eq!(result, expected_result);
    }

    #[test]
    fn test_order_solution_to_solution_errors_when_route_has_no_swaps() {
        let encoder = test_encoder(Chain::Ethereum);
        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x02));
        order_solution.route = Some(crate::types::Route::new(vec![]));

        let result = encoder.order_solution_to_solution(&order_solution);

        let Err(SolveError::Internal(msg)) = result else {
            panic!("expected Err(SolveError::Internal)");
        };
        assert_eq!(msg, "route has no swaps");
    }

    #[test]
    fn test_order_solution_to_solution_exact_in_maps_tokens_and_amounts() {
        let encoder = test_encoder(Chain::Ethereum);
        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x02));
        order_solution.exact_out = false;
        order_solution.route = Some(crate::types::Route::new(vec![make_route_swap(0x01, 0x02)]));

        let solution = encoder
            .order_solution_to_solution(&order_solution)
            .unwrap();

        assert_eq!(solution.given_token, Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(solution.checked_token, Bytes::from(make_address(0x02).as_ref()));
        assert_eq!(solution.given_amount, order_solution.amount_in);
        assert_eq!(solution.checked_amount, order_solution.amount_out);
        assert!(!solution.exact_out);
        assert_eq!(solution.swaps.len(), 1);
    }

    #[test]
    fn test_order_solution_to_solution_multi_hop_uses_boundary_swap_tokens() {
        let encoder = test_encoder(Chain::Ethereum);
        let mut order_solution = make_order_solution(make_address(0x01), make_address(0x03));
        order_solution.route = Some(crate::types::Route::new(vec![
            make_route_swap(0x01, 0x02),
            make_route_swap(0x02, 0x03),
        ]));

        let solution = encoder
            .order_solution_to_solution(&order_solution)
            .unwrap();

        // given_token = first swap's token_in, checked_token = last swap's token_out
        assert_eq!(solution.given_token, Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(solution.checked_token, Bytes::from(make_address(0x03).as_ref()));
        assert_eq!(solution.swaps.len(), 2);
    }

    #[test]
    fn test_order_solution_to_solution_with_wrap() {
        let chain = Chain::Ethereum;
        let encoder = test_encoder(chain);
        let native_addr = addr_from_bytes(&eth());
        let weth_addr = addr_from_bytes(&weth());

        let mut order_solution = make_order_solution(native_addr, make_address(0x03));
        // First swap token_in = WETH triggers Wrap
        order_solution.route = Some(crate::types::Route::new(vec![make_route_swap_addrs(
            weth_addr,
            make_address(0x03),
        )]));

        let solution = encoder
            .order_solution_to_solution(&order_solution)
            .unwrap();

        assert_eq!(solution.native_action, Some(NativeAction::Wrap));
    }

    #[test]
    fn test_order_solution_to_solution_unwrap_with_unwrap() {
        let chain = Chain::Ethereum;
        let encoder = test_encoder(chain);
        let native_addr = addr_from_bytes(&eth());
        let weth_addr = addr_from_bytes(&weth());

        let mut order_solution = make_order_solution(make_address(0x01), native_addr);
        // Last swap token_out = WETH triggers Unwrap
        order_solution.route = Some(crate::types::Route::new(vec![make_route_swap_addrs(
            make_address(0x01),
            weth_addr,
        )]));

        let solution = encoder
            .order_solution_to_solution(&order_solution)
            .unwrap();

        assert_eq!(solution.native_action, Some(NativeAction::Unwrap));
    }
}
