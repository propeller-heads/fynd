use std::sync::Arc;

use alloy::{
    primitives::{aliases::U48, Address, Keccak256, U160, U256},
    sol_types::SolValue,
};
use num_bigint::BigUint;
use tycho_execution::encoding::{
    errors::EncodingError,
    evm::{
        approvals::permit2::{PermitDetails as SolPermitDetails, PermitSingle},
        encoder_builders::TychoRouterEncoderBuilder,
        get_router_address,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{biguint_to_u256, bytes_to_address},
    },
    models::{EncodedSolution, Solution, Swap},
    tycho_encoder::TychoEncoder,
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

use crate::{EncodingOptions, OrderQuote, QuoteStatus, SolveError, Transaction};

/// Canonical Permit2 contract address — identical on all EVM chains.
pub const PERMIT2_ADDRESS: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

/// Encodes solution into tycho compatible transactions.
///
/// # Fields
/// * `tycho_encoder` - Encoder created using the configured chain for encoding solutions into tycho
///   compatible transactions
/// * `chain` - Chain to be used.
/// * `router_address` - Address of the Tycho Router contract on this chain.
pub struct Encoder {
    tycho_encoder: Box<dyn TychoEncoder>,
    chain: Chain,
    router_address: Bytes,
    /// Dedicated multi-threaded runtime so that swap encoders using
    /// `block_in_place` (e.g. Bebop RFQ) work even when the caller
    /// runs on a current-thread runtime (actix-web workers).
    encoding_rt: Arc<tokio::runtime::Runtime>,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // If dropped from within a tokio runtime (e.g. test teardown),
        // move the runtime to a background thread so its shutdown doesn't
        // panic with "cannot drop a runtime in a context where blocking
        // is not allowed".
        if tokio::runtime::Handle::try_current().is_ok() {
            let rt = Arc::clone(&self.encoding_rt);
            std::thread::spawn(move || drop(rt));
        }
    }
}

impl TryFrom<&OrderQuote> for Solution {
    type Error = SolveError;

    fn try_from(quote: &OrderQuote) -> Result<Self, Self::Error> {
        if quote.status() != QuoteStatus::Success {
            return Err(SolveError::FailedEncoding(format!(
                "cannot convert quote with status {:?} to Solution",
                quote.status()
            )));
        }

        let route = quote.route().ok_or_else(|| {
            SolveError::FailedEncoding("successful quote must have a route".to_string())
        })?;

        let token_in = route
            .input_token()
            .ok_or_else(|| SolveError::FailedEncoding("route has no input token".to_string()))?;
        let token_out = route
            .output_token()
            .ok_or_else(|| SolveError::FailedEncoding("route has no output token".to_string()))?;

        let swaps = route
            .swaps()
            .iter()
            .map(|s| {
                Swap::new(
                    s.protocol_component().clone(),
                    s.token_in().clone(),
                    s.token_out().clone(),
                )
                .with_split(*s.split())
                .with_protocol_state(Arc::from(s.protocol_state().clone_box()))
                .with_estimated_amount_in(s.amount_in().clone())
            })
            .collect();

        Ok(Solution::new(
            quote.sender.clone(),
            quote.receiver.clone(),
            Bytes::from(token_in.as_ref()),
            Bytes::from(token_out.as_ref()),
            quote.amount_in().clone(),
            quote.amount_out().clone(),
            swaps,
        ))
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
        let router_address = get_router_address(&chain)
            .map_err(|e| SolveError::FailedEncoding(e.to_string()))?
            .clone();
        let encoding_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| {
                SolveError::FailedEncoding(format!("failed to create encoding runtime: {e}"))
            })?;
        Ok(Self {
            tycho_encoder: TychoRouterEncoderBuilder::new()
                .chain(chain)
                .swap_encoder_registry(swap_encoder_registry)
                .build()?,
            chain,
            router_address,
            encoding_rt: Arc::new(encoding_rt),
        })
    }

    /// Returns the Tycho Router contract address for this chain.
    pub fn router_address(&self) -> &Bytes {
        &self.router_address
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
        mut quotes: Vec<OrderQuote>,
        encoding_options: EncodingOptions,
    ) -> Result<Vec<OrderQuote>, SolveError> {
        let slippage = encoding_options.slippage();
        if slippage == 0.0 {
            tracing::warn!("slippage is 0, transaction will likely revert");
        } else if slippage > 0.5 {
            tracing::warn!(slippage, "slippage exceeds 50%, possible misconfiguration");
        }

        let mut to_encode: Vec<(usize, Solution)> = Vec::new();

        for (i, quote) in quotes.iter().enumerate() {
            if quote.status() != QuoteStatus::Success {
                continue;
            }

            to_encode.push((
                i,
                Solution::try_from(quote)?
                    .with_user_transfer_type(encoding_options.transfer_type().clone()),
            ));
        }

        let solutions: Vec<Solution> = to_encode
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        let encoded_solutions = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    self.encoding_rt.block_on(async {
                        self.tycho_encoder
                            .encode_solutions(solutions)
                    })
                })
                .join()
                .expect("encoding thread panicked")
        })?;

        for (encoded_solution, (idx, solution)) in encoded_solutions
            .into_iter()
            .zip(to_encode)
        {
            let transaction =
                self.encode_tycho_router_call(encoded_solution, &solution, &encoding_options)?;
            quotes[idx].transaction = Some(transaction);
        }

        Ok(quotes)
    }

    /// Encodes a call using one of the router's swap methods.
    ///
    /// Selects the appropriate router function based on the function signature in
    /// `encoded_solution` (single/sequential/split, with optional Permit2 or Vault variants),
    /// prepends the 4-byte selector, and returns a `Transaction` ready for submission.
    fn encode_tycho_router_call(
        &self,
        encoded_solution: EncodedSolution,
        solution: &Solution,
        encoding_options: &EncodingOptions,
    ) -> Result<Transaction, EncodingError> {
        let amount_in = biguint_to_u256(solution.amount_in());
        let precision = BigUint::from(1_000_000u64);
        let slippage_amount = solution.min_amount_out().clone() *
            BigUint::from((encoding_options.slippage() * 1_000_000.0) as u64) /
            &precision;
        let min_amount_out =
            biguint_to_u256(&(solution.min_amount_out().clone() - slippage_amount));
        let token_in = bytes_to_address(solution.token_in())?;
        let token_out = bytes_to_address(solution.token_out())?;
        let receiver = bytes_to_address(solution.receiver())?;

        let (permit, permit2_sig) = if let Some(p) = encoding_options.permit() {
            let d = p.details();
            let permit = Some(PermitSingle {
                details: SolPermitDetails {
                    token: bytes_to_address(d.token())?,
                    amount: U160::from(biguint_to_u256(d.amount())),
                    expiration: U48::from(biguint_to_u256(d.expiration())),
                    nonce: U48::from(biguint_to_u256(d.nonce())),
                },
                spender: bytes_to_address(p.spender())?,
                sigDeadline: biguint_to_u256(p.sig_deadline()),
            });
            let sig = encoding_options
                .permit2_signature()
                .ok_or_else(|| {
                    EncodingError::FatalError("Signature must be provided for permit2".to_string())
                })?
                .to_vec();
            (permit, sig)
        } else {
            (None, vec![])
        };

        let client_fee_params = if let Some(fee) = encoding_options.client_fee_params() {
            (
                fee.bps(),
                bytes_to_address(fee.receiver())?,
                biguint_to_u256(fee.max_contribution()),
                U256::from(fee.deadline()),
                fee.signature().to_vec(),
            )
        } else {
            (0u16, Address::ZERO, U256::ZERO, U256::MAX, vec![])
        };

        let fn_sig = encoded_solution.function_signature();
        let swaps = encoded_solution.swaps();

        let method_calldata = if fn_sig.contains("Permit2") {
            let permit = permit.ok_or(EncodingError::FatalError(
                "permit2 object must be set to use permit2".to_string(),
            ))?;
            if fn_sig.contains("splitSwap") {
                (
                    amount_in,
                    token_in,
                    token_out,
                    min_amount_out,
                    U256::from(encoded_solution.n_tokens()),
                    receiver,
                    client_fee_params,
                    permit,
                    permit2_sig,
                    swaps,
                )
                    .abi_encode()
            } else {
                (
                    amount_in,
                    token_in,
                    token_out,
                    min_amount_out,
                    receiver,
                    client_fee_params,
                    permit,
                    permit2_sig,
                    swaps,
                )
                    .abi_encode()
            }
        } else if fn_sig.contains("splitSwap") {
            (
                amount_in,
                token_in,
                token_out,
                min_amount_out,
                U256::from(encoded_solution.n_tokens()),
                receiver,
                client_fee_params,
                swaps,
            )
                .abi_encode()
        } else if fn_sig.contains("singleSwap") || fn_sig.contains("sequentialSwap") {
            (amount_in, token_in, token_out, min_amount_out, receiver, client_fee_params, swaps)
                .abi_encode()
        } else {
            return Err(EncodingError::FatalError(format!(
                "unsupported function signature for Tycho router: {fn_sig}"
            )));
        };

        let native_address = &self.chain.native_token().address;
        let contract_interaction =
            Self::encode_input(encoded_solution.function_signature(), method_calldata);
        let value = if *solution.token_in() == *native_address {
            solution.amount_in().clone()
        } else {
            BigUint::ZERO
        };
        Ok(Transaction::new(
            encoded_solution
                .interacting_with()
                .clone(),
            value,
            contract_interaction,
        ))
    }

    /// Prepends the 4-byte Keccak selector for `selector` to the ABI-encoded args.
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
        BlockInfo, OrderQuote, QuoteStatus,
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
        // Component ID must be a valid address for the USV2 swap encoder
        let pool_addr = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
        crate::types::Swap::new(
            pool_addr.to_string(),
            "uniswap_v2".to_string(),
            token_in,
            token_out,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(50_000u64),
            component(pool_addr, &[tin, tout]),
            Box::new(MockProtocolSim::default()),
        )
    }

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order_quote() -> OrderQuote {
        OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(100_000u64),
            BigUint::from(990u64),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
        )
    }

    struct MockTychoEncoder;

    impl TychoEncoder for MockTychoEncoder {
        fn encode_solutions(
            &self,
            _solutions: Vec<Solution>,
        ) -> Result<Vec<EncodedSolution>, EncodingError> {
            Ok(vec![])
        }

        fn validate_solution(&self, _solution: &Solution) -> Result<(), EncodingError> {
            Ok(())
        }
    }

    fn mock_encoder(chain: Chain) -> Encoder {
        Encoder {
            tycho_encoder: Box::new(MockTychoEncoder),
            chain,
            router_address: Bytes::from([0u8; 20].as_ref()),
            encoding_rt: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .unwrap(),
            ),
        }
    }

    #[test]
    fn test_encoder_new_fails_on_unsupported_chain() {
        // Arbitrum has no entry in ROUTER_ADDRESSES_JSON.
        // Build a registry for Ethereum (which is valid) but pass Arbitrum to Encoder::new —
        // the router address lookup must fail before the encoder builder is invoked.
        let registry =
            tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry::new(Chain::Ethereum)
                .add_default_encoders(None)
                .expect("registry should build for Ethereum");
        let result = Encoder::new(Chain::Arbitrum, registry);
        assert!(result.is_err(), "expected Err for chain without router address, got Ok");
    }

    #[test]
    fn test_try_from_without_route_errors() {
        let quote = make_order_quote();

        let result = Solution::try_from(&quote);

        assert!(result.is_err());
    }

    #[test]
    fn test_try_from_non_success_errors() {
        let quote = OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::NoRouteFound,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(100_000u64),
            BigUint::from(990u64),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
        );

        let result = Solution::try_from(&quote);

        assert!(result.is_err());
    }

    #[test]
    fn test_try_from_maps_tokens_and_amounts() {
        let quote =
            make_order_quote().with_route(crate::types::Route::new(vec![make_route_swap_addrs(
                make_address(0x01),
                make_address(0x02),
            )]));

        let solution = Solution::try_from(&quote).unwrap();

        assert_eq!(*solution.token_in(), Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(*solution.token_out(), Bytes::from(make_address(0x02).as_ref()));
        assert_eq!(*solution.amount_in(), *quote.amount_in());
        assert_eq!(*solution.min_amount_out(), *quote.amount_out());
        assert_eq!(solution.swaps().len(), 1);
    }

    #[test]
    fn test_try_from_multi_hop_uses_boundary_swap_tokens() {
        let quote = make_order_quote().with_route(crate::types::Route::new(vec![
            make_route_swap_addrs(make_address(0x01), make_address(0x02)),
            make_route_swap_addrs(make_address(0x02), make_address(0x03)),
        ]));

        let solution = Solution::try_from(&quote).unwrap();

        assert_eq!(*solution.token_in(), Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(*solution.token_out(), Bytes::from(make_address(0x03).as_ref()));
        assert_eq!(solution.swaps().len(), 2);
    }

    #[tokio::test]
    async fn test_encode_skips_non_successful_solutions() {
        let encoder = mock_encoder(Chain::Ethereum);
        let quote = OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::NoRouteFound,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(100_000u64),
            BigUint::from(990u64),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
        );

        let encoding_options = EncodingOptions::new(0.01);

        let result = encoder
            .encode(vec![quote], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction().is_none());
    }

    fn real_encoder() -> Encoder {
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(None)
            .unwrap();
        Encoder::new(Chain::Ethereum, registry).unwrap()
    }

    #[tokio::test]
    async fn test_encode_sets_transaction_on_successful_solution() {
        let encoder = real_encoder();
        let quote =
            make_order_quote().with_route(crate::types::Route::new(vec![make_route_swap_addrs(
                make_address(0x01),
                make_address(0x02),
            )]));

        let encoding_options = EncodingOptions::new(0.01);

        let result = encoder
            .encode(vec![quote], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction().is_some());
        let tx = result[0].transaction().unwrap();
        assert!(!tx.data().is_empty());
        // Data starts with a 4-byte function selector
        assert!(tx.data().len() > 4);
    }

    #[tokio::test]
    async fn test_encode_with_client_fee_params() {
        let encoder = real_encoder();
        let quote =
            make_order_quote().with_route(crate::types::Route::new(vec![make_route_swap_addrs(
                make_address(0x01),
                make_address(0x02),
            )]));

        let fee = crate::ClientFeeParams::new(
            100,
            Bytes::from(make_address(0xBB).as_ref()),
            BigUint::from(0u64),
            1_893_456_000u64,
            Bytes::from(vec![0xAB; 65]),
        );
        let encoding_options = EncodingOptions::new(0.01).with_client_fee_params(fee);

        let result = encoder
            .encode(vec![quote], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction().is_some());
        let tx = result[0].transaction().unwrap();
        assert!(!tx.data().is_empty());
        // Calldata with fee params should be longer than without
        assert!(tx.data().len() > 4);
    }

    #[tokio::test]
    async fn test_encode_without_client_fee_produces_transaction() {
        let encoder = real_encoder();
        let quote =
            make_order_quote().with_route(crate::types::Route::new(vec![make_route_swap_addrs(
                make_address(0x01),
                make_address(0x02),
            )]));

        let encoding_options = EncodingOptions::new(0.01);

        let result = encoder
            .encode(vec![quote], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction().is_some());
    }
}
