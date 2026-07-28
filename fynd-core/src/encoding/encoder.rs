use std::sync::Arc;

use alloy::{
    primitives::{aliases::U48, keccak256, Address, Keccak256, U160, U256},
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
        ROUTER_ETH_ADDRESS,
    },
    models::{EncodedSolution, Solution, Swap},
    tycho_encoder::TychoEncoder,
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

use crate::{
    encoding::{
        exclusive_swap::ExclusiveSwapSigner,
        router_fees::{FeeRates, RouterFees, SharedRouterFees},
    },
    EncodingOptions, FeeBreakdown, OrderQuote, QuoteStatus, SolveError, Transaction,
};

/// Canonical Permit2 contract address — identical on all EVM chains.
pub const PERMIT2_ADDRESS: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

/// Encodes solution into tycho compatible transactions.
///
/// # Fields
/// * `tycho_encoder` - Encoder created using the configured chain for encoding solutions into tycho
///   compatible transactions. `None` when the encoder is disabled (router-less / quote-only chain).
/// * `chain` - Chain to be used.
/// * `router_address` - Address of the Tycho Router contract on this chain, or `None` if Tycho has
///   no router deployed there — encoding is then unavailable and `encode()` fails clearly.
/// * `router_fees` - Router fee configuration, refreshed from chain by a background fetcher.
pub struct Encoder {
    tycho_encoder: Option<Box<dyn TychoEncoder>>,
    chain: Chain,
    router_address: Option<Bytes>,
    router_fees: SharedRouterFees,
    /// Signs exclusive legs. `None` disables signing (no controller key configured).
    exclusive_swap_signer: Option<ExclusiveSwapSigner>,
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

        let token_map = route.tokens();
        let lookup_token = |addr: &Bytes| {
            token_map
                .get(addr)
                .cloned()
                .ok_or_else(|| {
                    SolveError::FailedEncoding(format!(
                        "token {addr:?} not found in route's token map; \
                     algorithm must populate Route::with_tokens for every swap token"
                    ))
                })
        };
        let swaps = route
            .swaps()
            .iter()
            .map(|s| {
                let token_in = lookup_token(s.token_in())?;
                let token_out = lookup_token(s.token_out())?;
                Ok(Swap::new(
                    s.protocol_component().clone(),
                    token_in,
                    token_out,
                    s.gas_estimate().clone(),
                )
                .with_split(*s.split())
                .with_protocol_state(Arc::from(s.protocol_state().clone_box()))
                .with_estimated_amount_in(s.amount_in().clone()))
            })
            .collect::<Result<Vec<_>, SolveError>>()?;

        Ok(Solution::new(
            quote.sender().clone(),
            quote.receiver().clone(),
            Bytes::from(token_in.as_ref()),
            Bytes::from(token_out.as_ref()),
            quote.amount_in().clone(),
            quote.amount_out().clone(),
            swaps,
        ))
    }
}

impl Encoder {
    /// Whether Tycho has a router deployment (and thus encoding support) for `chain`.
    pub fn is_supported(chain: Chain) -> bool {
        get_router_address(&chain).is_ok()
    }

    /// Creates a new `Encoder` for the given chain.
    ///
    /// # Arguments
    /// * `chain` - Chain to encode solutions for.
    /// * `swap_encoder_registry` - Registry of swap encoders for supported protocols.
    ///
    /// # Returns
    /// A new `Encoder` configured with `TransferFrom` user transfer type. If `chain` has no Tycho
    /// router deployment, the encoder is returned in a disabled state: it can still be used to
    /// quote, but [`Self::encode`] will fail with [`SolveError::FailedEncoding`].
    pub fn new(
        chain: Chain,
        swap_encoder_registry: SwapEncoderRegistry,
    ) -> Result<Self, SolveError> {
        let router_address = get_router_address(&chain).ok().cloned();
        let tycho_encoder = router_address
            .is_some()
            .then(|| {
                TychoRouterEncoderBuilder::new()
                    .chain(chain)
                    .swap_encoder_registry(swap_encoder_registry)
                    .build()
            })
            .transpose()?;
        let exclusive_swap_signer = ExclusiveSwapSigner::from_env(chain.id())?;
        Ok(Self {
            tycho_encoder,
            chain,
            router_address,
            router_fees: SharedRouterFees::default(),
            exclusive_swap_signer,
        })
    }

    /// Overrides the exclusive-swap signer, replacing whatever was read from the environment.
    #[must_use]
    pub fn with_exclusive_swap_signer(mut self, signer: ExclusiveSwapSigner) -> Self {
        self.exclusive_swap_signer = Some(signer);
        self
    }

    /// Returns the Tycho Router contract address for this chain, or `None` if encoding is
    /// unavailable because no router is deployed there.
    pub fn router_address(&self) -> Option<&Bytes> {
        self.router_address.as_ref()
    }

    /// Returns the shared router fee handle this encoder reads on every encode.
    ///
    /// Pass it to a [`RouterFeeFetcher`](crate::encoding::fee_fetcher::RouterFeeFetcher)
    /// to keep the fees in sync with the on-chain FeeCalculator.
    pub fn router_fees(&self) -> SharedRouterFees {
        self.router_fees.clone()
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
        let Some(tycho_encoder) = self.tycho_encoder.as_ref() else {
            return Err(SolveError::EncodingUnavailable(format!(
                "encoding is unavailable on chain '{}': no Tycho router is deployed. Fynd is \
                 running quote-only; contact ops to deploy the router/executor contracts.",
                self.chain
            )));
        };

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

            let solution = Solution::try_from(quote)?
                .with_user_transfer_type(encoding_options.transfer_type().clone());
            let solution = match &self.exclusive_swap_signer {
                Some(signer) => Self::stamp_exclusive_swaps(solution, quote, signer)?,
                None => {
                    // Fail fast rather than emit on-chain-invalid unsigned calldata for an
                    // exclusive leg: an exclusive route requires a signature.
                    if has_exclusive_leg(quote) {
                        return Err(SolveError::FailedEncoding(
                            "quote routes through an exclusive pool but no signing key is \
                             configured (set EXCLUSIVE_SWAP_CONTROLLER_KEY)"
                                .to_string(),
                        ));
                    }
                    solution
                }
            };
            to_encode.push((i, solution));
        }

        let solutions: Vec<Solution> = to_encode
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        let encoded_solutions = tycho_encoder.encode_solutions(solutions)?;

        let router_fees = self.router_fees.snapshot();
        for (encoded_solution, (idx, solution)) in encoded_solutions
            .into_iter()
            .zip(to_encode)
        {
            quotes[idx].set_gas_estimate(encoded_solution.estimated_gas().clone());
            let (transaction, fee_breakdown) = self.encode_tycho_router_call(
                encoded_solution,
                &solution,
                &encoding_options,
                &router_fees,
            )?;
            quotes[idx].set_transaction(transaction);
            quotes[idx].set_fee_breakdown(fee_breakdown);
        }

        Ok(quotes)
    }

    /// Stamps controller-signed `user_data` onto each exclusive leg of `solution`.
    ///
    /// A leg is exclusive when its route swap carries a committed amount. The solution's swaps are
    /// built 1:1 from the route's swaps, so they are matched by index. Returns `solution` unchanged
    /// when no leg is exclusive.
    fn stamp_exclusive_swaps(
        solution: Solution,
        quote: &OrderQuote,
        signer: &ExclusiveSwapSigner,
    ) -> Result<Solution, SolveError> {
        let route = quote.route().ok_or_else(|| {
            SolveError::FailedEncoding("successful quote must have a route".to_string())
        })?;
        let route_swaps = route.swaps();

        // Nothing to sign unless a leg carries a committed amount; leave the solution untouched.
        if !route_swaps
            .iter()
            .any(|s| s.committed_amount_out().is_some())
        {
            return Ok(solution);
        }

        // `route_swaps` carry `committed_amount_out` and the pool attributes; `solution.swaps()`
        // are built 1:1 from them by `Solution::try_from` and are what the router executes.
        // We read the committed amount from the route swap but stamp `user_data` onto the
        // matching solution swap, matched by index via the zip below.
        let swaps = solution
            .swaps()
            .iter()
            .cloned()
            .zip(route_swaps.iter())
            // Only the exclusive leg (the route swap carrying a committed amount) gets signed
            // `user_data`; every other solution swap passes through unchanged.
            .map(|(solution_swap, route_swap)| {
                if route_swap
                    .committed_amount_out()
                    .is_some()
                {
                    let user_data = signer.build_user_data(route_swap)?;
                    Ok(solution_swap.with_user_data(user_data))
                } else {
                    Ok(solution_swap)
                }
            })
            .collect::<Result<Vec<_>, SolveError>>()?;

        Ok(solution.with_swaps(swaps))
    }

    /// Encodes a call using one of the router's swap methods.
    ///
    /// Selects the appropriate router function based on the function signature in
    /// `encoded_solution` (single/sequential/split, with optional Permit2 or Vault variants),
    /// prepends the 4-byte selector, and returns a `Transaction` ready for submission.
    ///
    /// Fee calculation mirrors the on-chain `FeeCalculator.calculateFee` using identical
    /// integer arithmetic so `min_amount_out` passes the router's post-fee check.
    fn encode_tycho_router_call(
        &self,
        encoded_solution: EncodedSolution,
        solution: &Solution,
        encoding_options: &EncodingOptions,
        router_fees: &RouterFees,
    ) -> Result<(Transaction, FeeBreakdown), EncodingError> {
        let amount_in = biguint_to_u256(solution.amount_in());
        let swap_output = solution.min_amount_out();
        // Mirror FeeCalculator._resolveClient: custom router fees are looked up by the client
        // fee receiver; without client fee params the contract falls back to tx.origin, for
        // which the order sender is our best available proxy.
        let fee_client = encoding_options
            .client_fee_params()
            .map_or_else(|| solution.sender(), |f| f.receiver());
        let fee_breakdown = Self::calculate_fee_breakdown(
            swap_output,
            encoding_options
                .client_fee_params()
                .map_or(0, |f| f.bps()),
            encoding_options.slippage(),
            router_fees.fees_for(fee_client),
        )?;
        let min_amount_out = biguint_to_u256(fee_breakdown.min_amount_received());
        let native_address = &self.chain.native_token().address;
        let router_eth = Address::from_slice(ROUTER_ETH_ADDRESS.as_ref());
        let to_router_address = |raw: Address| {
            if raw.as_slice() == native_address.as_ref() {
                router_eth
            } else {
                raw
            }
        };

        let token_in = to_router_address(bytes_to_address(solution.token_in())?);
        let token_out = to_router_address(bytes_to_address(solution.token_out())?);
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
                // Pad to 65 bytes so the ABI encoding always reserves room for
                // the client to patch the real EIP-712 signature after signing.
                {
                    let mut sig = fee.signature().to_vec();
                    sig.resize(65, 0);
                    sig
                },
            )
        } else {
            (0u16, Address::ZERO, U256::ZERO, U256::MAX, vec![])
        };

        let fn_sig = encoded_solution.function_signature();
        let swaps = encoded_solution.swaps();
        let fee_breakdown = if encoding_options
            .client_fee_params()
            .is_some()
        {
            fee_breakdown.with_swaps_hash(keccak256(swaps).0)
        } else {
            fee_breakdown
        };

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

        let contract_interaction =
            Self::encode_input(encoded_solution.function_signature(), method_calldata);

        let value =
            if token_in == router_eth { solution.amount_in().clone() } else { BigUint::ZERO };
        let mut transaction = Transaction::new(
            encoded_solution
                .interacting_with()
                .clone(),
            value,
            contract_interaction,
        );
        if encoding_options
            .client_fee_params()
            .is_some()
        {
            let offset = encoded_solution.client_fee_signature_offset();
            transaction = transaction.with_client_fee_signature_offset(offset);
        }
        Ok((transaction, fee_breakdown))
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

    /// Mirrors the on-chain `FeeCalculator.calculateFee` using identical integer arithmetic.
    ///
    /// Given the raw swap output, client fee in bps, slippage tolerance, and the effective
    /// router fee rates for the client, computes the exact fee amounts and the minimum
    /// amount the user will receive.
    ///
    /// # Errors
    ///
    /// Returns an error when the combined fees exceed 100%, which would make the on-chain
    /// call revert with `FeeCalculator__FeeTooHigh`.
    fn calculate_fee_breakdown(
        swap_output: &BigUint,
        client_fee_bps: u16,
        slippage: f64,
        fee_rates: FeeRates,
    ) -> Result<FeeBreakdown, EncodingError> {
        let max_fee_units = fee_rates.max_fee_units();
        // Scale the client fee from legacy bps (10_000 = 100%) to fee units so both fee
        // types share the same denominator, exactly as the contract does.
        let scaled_client_fee = client_fee_bps as u64 * fee_rates.fee_units_per_bps();
        let fee_on_output = fee_rates.on_output() as u64;
        let fee_on_client_fee = fee_rates.on_client_fee() as u64;

        if scaled_client_fee + fee_on_output > max_fee_units {
            return Err(EncodingError::FatalError(format!(
                "client fee ({client_fee_bps} bps) plus router fee on output \
                 ({fee_on_output} fee units) exceed the {max_fee_units} fee-unit cap (100%); \
                 the router would revert"
            )));
        }
        if fee_on_client_fee > max_fee_units {
            return Err(EncodingError::FatalError(format!(
                "router fee on client fee ({fee_on_client_fee} fee units) exceeds the \
                 {max_fee_units} fee-unit cap (100%); the router would revert"
            )));
        }

        let mut router_fee_on_client = BigUint::ZERO;
        let mut client_portion = BigUint::ZERO;

        if scaled_client_fee > 0 {
            let client_fee_numerator = swap_output * scaled_client_fee;
            let total_client_fee = &client_fee_numerator / max_fee_units;

            router_fee_on_client = client_fee_numerator * fee_on_client_fee /
                BigUint::from(fee_rates.max_fee_units_squared());

            client_portion = total_client_fee - &router_fee_on_client;
        }

        let router_fee_on_output = swap_output * fee_on_output / max_fee_units;
        let total_router_fee = router_fee_on_client + router_fee_on_output;

        let amount_after_fees = swap_output - &client_portion - &total_router_fee;

        let precision = BigUint::from(1_000_000u64);
        let slippage_amount =
            &amount_after_fees * BigUint::from((slippage * 1_000_000.0) as u64) / &precision;

        let min_amount_received = &amount_after_fees - &slippage_amount;

        Ok(FeeBreakdown::new(
            total_router_fee,
            client_portion,
            slippage_amount,
            min_amount_received,
        ))
    }
}

impl From<EncodingError> for SolveError {
    fn from(err: EncodingError) -> Self {
        SolveError::FailedEncoding(err.to_string())
    }
}

/// Returns whether the quote routes through an exclusive pool, i.e. any swap in its route carries
/// a committed amount.
fn has_exclusive_leg(quote: &OrderQuote) -> bool {
    quote.route().is_some_and(|route| {
        route
            .swaps()
            .iter()
            .any(|s| s.committed_amount_out().is_some())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use num_bigint::BigUint;
    use rstest::rstest;
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

    fn make_token(addr: Address) -> Token {
        Token {
            address: addr,
            symbol: "T".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: SimChain::Ethereum,
            quality: 100,
        }
    }

    fn make_route_swap_addrs(token_in: Address, token_out: Address) -> crate::types::Swap {
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

    /// Builds a `Route` with both swaps and the token map populated, mirroring
    /// what the algorithms do in production.
    fn make_route_with_tokens(pairs: &[(Address, Address)]) -> crate::types::Route {
        let mut tokens = HashMap::new();
        let swaps = pairs
            .iter()
            .map(|(tin, tout)| {
                tokens
                    .entry(tin.clone())
                    .or_insert_with(|| make_token(tin.clone()));
                tokens
                    .entry(tout.clone())
                    .or_insert_with(|| make_token(tout.clone()));
                make_route_swap_addrs(tin.clone(), tout.clone())
            })
            .collect();
        crate::types::Route::new(swaps, tokens).expect("non-empty route")
    }

    fn make_address(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_order_quote(amount_out: u64) -> OrderQuote {
        OrderQuote::new(
            "test-order".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(100_000u64),
            BigUint::from(amount_out),
            BlockInfo::new(1, "0x123".to_string(), 1000),
            "test".to_string(),
            Bytes::from(make_address(0xAA).as_ref()),
            Bytes::from(make_address(0xAA).as_ref()),
            "1".to_string(),
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
        let router_fees = SharedRouterFees::default();
        router_fees.set(RouterFees::new(FEE_SCALE, 100_000, 20_000_000, HashMap::new()));
        Encoder {
            tycho_encoder: Some(Box::new(MockTychoEncoder)),
            chain,
            router_address: Some(Bytes::from([0u8; 20].as_ref())),
            router_fees,
            exclusive_swap_signer: None,
        }
    }

    #[test]
    fn test_encoder_new_disabled_on_unsupported_chain() {
        // Starknet has no entry in ROUTER_ADDRESSES_JSON.
        // Build a registry for Ethereum (which is valid) but pass Starknet to Encoder::new —
        // this must succeed with a disabled encoder rather than fail.
        let registry =
            tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry::new(Chain::Ethereum)
                .add_default_encoders(None)
                .expect("registry should build for Ethereum");
        let encoder = Encoder::new(Chain::Starknet, registry)
            .expect("new must not fail for a router-less chain");
        assert!(
            encoder.router_address().is_none(),
            "expected disabled encoder, got a router address"
        );
    }

    #[tokio::test]
    async fn disabled_encoder_quotes_but_refuses_to_encode() {
        // A chain with no Tycho router deployment yields a disabled encoder.
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(None)
            .unwrap();
        let encoder =
            Encoder::new(Chain::Starknet, registry).expect("new must not fail when disabled");
        assert!(encoder.router_address().is_none());

        let err = encoder
            .encode(vec![], EncodingOptions::new(0.01))
            .await
            .expect_err("encoding must fail on a router-less chain");
        assert!(matches!(err, SolveError::EncodingUnavailable(_)));
    }

    #[test]
    fn test_try_from_without_route_errors() {
        let quote = make_order_quote(990);

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
            "1".to_string(),
        );

        let result = Solution::try_from(&quote);

        assert!(result.is_err());
    }

    #[test]
    fn test_try_from_maps_tokens_and_amounts() {
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        let solution = Solution::try_from(&quote).unwrap();

        assert_eq!(*solution.token_in(), Bytes::from(make_address(0x01).as_ref()));
        assert_eq!(*solution.token_out(), Bytes::from(make_address(0x02).as_ref()));
        assert_eq!(*solution.amount_in(), *quote.amount_in());
        assert_eq!(*solution.min_amount_out(), *quote.amount_out());
        assert_eq!(solution.swaps().len(), 1);
    }

    const CONTROLLER_KEY: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";

    fn ekubo_signed_swap(committed: Option<u64>) -> crate::types::Swap {
        let token_in = make_address(0x11);
        let token_out = make_address(0x22);
        let mut comp = component("ekubo-signed-pool", &[]);
        comp.static_attributes
            .insert("extension".to_string(), Bytes::from([0x55u8; 20].as_ref()));
        comp.static_attributes
            .insert("fee".to_string(), Bytes::from(0u64));
        comp.static_attributes
            .insert("pool_type_config".to_string(), Bytes::from(0u32));

        let mut swap = crate::types::Swap::new(
            "ekubo-signed-pool".to_string(),
            "ekubo_v3".to_string(),
            token_in,
            token_out,
            BigUint::from(1_000_000u64),
            BigUint::from(1_000_000u64),
            BigUint::from(50_000u64),
            comp,
            Box::new(MockProtocolSim::default()),
        );
        if let Some(committed) = committed {
            swap.set_committed_amount_out(BigUint::from(committed));
        }
        swap
    }

    fn single_swap_route(swap: crate::types::Swap) -> crate::types::Route {
        let tokens = HashMap::from([
            (swap.token_in().clone(), make_token(swap.token_in().clone())),
            (swap.token_out().clone(), make_token(swap.token_out().clone())),
        ]);
        crate::types::Route::new(vec![swap], tokens).expect("non-empty route")
    }

    #[rstest]
    #[case::exclusive_leg_signed(Some(990_000), true)]
    #[case::public_leg_untouched(None, false)]
    fn test_stamp_exclusive_swaps(#[case] committed: Option<u64>, #[case] signed: bool) {
        let quote =
            make_order_quote(990_000).with_route(single_swap_route(ekubo_signed_swap(committed)));
        let signer = ExclusiveSwapSigner::new(CONTROLLER_KEY.parse().unwrap(), 1, 0, 120);

        let solution =
            Encoder::stamp_exclusive_swaps(Solution::try_from(&quote).unwrap(), &quote, &signer)
                .unwrap();

        assert_eq!(
            solution.swaps()[0]
                .user_data()
                .is_some(),
            signed
        );
    }

    #[tokio::test]
    async fn test_encode_rejects_exclusive_leg_without_signer() {
        // mock_encoder has no exclusive_swap_signer, so an exclusive leg must fail fast rather than
        // produce unsigned (on-chain-invalid) calldata.
        let encoder = mock_encoder(Chain::Ethereum);
        let quote = make_order_quote(990_000)
            .with_route(single_swap_route(ekubo_signed_swap(Some(990_000))));

        let result = encoder
            .encode(vec![quote], EncodingOptions::new(0.01))
            .await;

        assert!(result.is_err(), "expected fail-fast error for unsigned exclusive leg");
    }

    #[test]
    fn test_try_from_multi_hop_uses_boundary_swap_tokens() {
        let quote = make_order_quote(990).with_route(make_route_with_tokens(&[
            (make_address(0x01), make_address(0x02)),
            (make_address(0x02), make_address(0x03)),
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
            "1".to_string(),
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
        let encoder = Encoder::new(Chain::Ethereum, registry).unwrap();
        // Load fees so encode() can run; in production the fetcher supplies on-chain values.
        encoder
            .router_fees()
            .set(RouterFees::new(FEE_SCALE, 100_000, 20_000_000, HashMap::new()));
        encoder
    }

    #[tokio::test]
    async fn test_encode_sets_transaction_on_successful_solution() {
        let encoder = real_encoder();
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

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
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

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
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        let encoding_options = EncodingOptions::new(0.01);

        let result = encoder
            .encode(vec![quote], encoding_options)
            .await
            .unwrap();

        assert!(result[0].transaction().is_some());
    }

    // ==================== Signature Offset Tests ====================

    fn make_client_fee(bps: u16) -> crate::ClientFeeParams {
        crate::ClientFeeParams::new(
            bps,
            Bytes::from(make_address(0xBB).as_ref()),
            BigUint::from(0u64),
            1_893_456_000u64,
            Bytes::from(vec![]),
        )
    }

    #[tokio::test]
    async fn test_encode_with_client_fee_returns_signature_offset() {
        let encoder = real_encoder();
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let opts = EncodingOptions::new(0.01).with_client_fee_params(make_client_fee(100));

        let result = encoder
            .encode(vec![quote], opts)
            .await
            .unwrap();

        let tx = result[0].transaction().unwrap();
        tx.client_fee_signature_offset()
            .expect("client_fee_signature_offset must be present with client fee");
    }

    #[tokio::test]
    async fn test_encode_without_client_fee_has_no_signature_offset() {
        let encoder = real_encoder();
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let opts = EncodingOptions::new(0.01);

        let result = encoder
            .encode(vec![quote], opts)
            .await
            .unwrap();

        let tx = result[0].transaction().unwrap();
        assert!(tx
            .client_fee_signature_offset()
            .is_none());
    }

    #[tokio::test]
    async fn test_signature_offset_allows_patching() {
        let encoder = real_encoder();
        let real_sig = vec![0xFF; 65];
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let opts = EncodingOptions::new(0.01).with_client_fee_params(make_client_fee(100));

        let result = encoder
            .encode(vec![quote], opts)
            .await
            .unwrap();

        let tx = result[0].transaction().unwrap();
        let offset = tx
            .client_fee_signature_offset()
            .unwrap();

        let mut calldata = tx.data().to_vec();
        calldata[offset..offset + 65].copy_from_slice(&real_sig);
        assert_eq!(&calldata[offset..offset + 65], &real_sig[..]);
    }

    // ==================== Fee Breakdown Tests ====================

    /// FeeCalculator precision used in these tests: 100% = 100,000,000 fee units.
    const FEE_SCALE: u64 = 100_000_000;

    #[test]
    fn test_calculate_fee_breakdown() {
        // 10 bps router fee on output, 20% router share of the client fee, 1% client fee.
        let rates = FeeRates::new(100_000, 20_000_000, FEE_SCALE);

        let breakdown =
            Encoder::calculate_fee_breakdown(&BigUint::from(1_000_000u64), 100, 0.0, rates)
                .unwrap();

        // total client fee = 1% of 1_000_000 = 10_000; router takes 20% of it = 2_000.
        // router fee on output = 0.1% of 1_000_000 = 1_000.
        assert_eq!(*breakdown.client_fee(), BigUint::from(8_000u64));
        assert_eq!(*breakdown.router_fee(), BigUint::from(3_000u64));
        assert_eq!(*breakdown.min_amount_received(), BigUint::from(989_000u64));
    }

    #[test]
    fn test_calculate_fee_breakdown_zero_fees() {
        let rates = FeeRates::new(0, 0, FEE_SCALE);

        let breakdown =
            Encoder::calculate_fee_breakdown(&BigUint::from(1_000_000u64), 0, 0.0, rates).unwrap();

        assert_eq!(*breakdown.client_fee(), BigUint::ZERO);
        assert_eq!(*breakdown.router_fee(), BigUint::ZERO);
        assert_eq!(*breakdown.min_amount_received(), BigUint::from(1_000_000u64));
    }

    #[test]
    fn test_calculate_fee_breakdown_fee_too_high() {
        // 100% client fee plus any router fee on output exceeds the maximum.
        let rates = FeeRates::new(1, 0, FEE_SCALE);

        let result =
            Encoder::calculate_fee_breakdown(&BigUint::from(1_000_000u64), 10_000, 0.0, rates);

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_encode_uses_custom_fees_for_client_fee_receiver() {
        let encoder = real_encoder();
        // Default 1% router fee on output; receiver 0xBB pays no router fees at all.
        let custom = HashMap::from([(Bytes::from(make_address(0xBB).as_ref()), (0u32, 0u32))]);
        encoder
            .router_fees()
            .set(RouterFees::new(FEE_SCALE, 1_000_000, 20_000_000, custom));
        let quote = make_order_quote(1_000_000_000)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let opts = EncodingOptions::new(0.0).with_client_fee_params(make_client_fee(100));

        let result = encoder
            .encode(vec![quote], opts)
            .await
            .unwrap();

        let breakdown = result[0].fee_breakdown().unwrap();
        assert_eq!(*breakdown.router_fee(), BigUint::ZERO);
        // Client keeps the full 1% fee since the router's share is overridden to zero.
        assert_eq!(*breakdown.client_fee(), BigUint::from(10_000_000u64));
    }

    #[tokio::test]
    async fn test_encode_falls_back_to_sender() {
        let encoder = real_encoder();
        // The order sender (0xAA) has a custom zero router fee on output; client-fee share
        // inherits the 20% default.
        let custom =
            HashMap::from([(Bytes::from(make_address(0xAA).as_ref()), (0u32, 20_000_000u32))]);
        encoder
            .router_fees()
            .set(RouterFees::new(FEE_SCALE, 1_000_000, 20_000_000, custom));
        let quote = make_order_quote(1_000_000_000)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        let result = encoder
            .encode(vec![quote], EncodingOptions::new(0.0))
            .await
            .unwrap();

        let breakdown = result[0].fee_breakdown().unwrap();
        assert_eq!(*breakdown.router_fee(), BigUint::ZERO);
    }

    #[tokio::test]
    async fn test_encode_unknown_client() {
        let encoder = real_encoder();
        encoder
            .router_fees()
            .set(RouterFees::new(FEE_SCALE, 1_000_000, 20_000_000, HashMap::new()));
        let quote = make_order_quote(1_000_000_000)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        let result = encoder
            .encode(vec![quote], EncodingOptions::new(0.0))
            .await
            .unwrap();

        let breakdown = result[0].fee_breakdown().unwrap();
        // 1% of 1_000_000_000.
        assert_eq!(*breakdown.router_fee(), BigUint::from(10_000_000u64));
    }
}
