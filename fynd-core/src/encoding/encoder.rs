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
        router_fees::{FeeRates, SharedRouterFees},
    },
    EncodingOptions, FeeBreakdown, OrderQuote, QuoteStatus, SolveError, Transaction,
};

/// Canonical Permit2 contract address — identical on all EVM chains.
pub const PERMIT2_ADDRESS: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

/// Mirror of `TychoRouter.MAX_SLIPPAGE_TOLERANCE_BPS`: the router rejects calldata whose
/// `minAmountOut` is more than this many basis points below `expectedAmountOut`.
const MAX_SLIPPAGE_TOLERANCE_BPS: u64 = 2_000;

/// Basis-point denominator used by the router's slippage guardrail.
const BPS_DENOMINATOR: u64 = 10_000;

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
    /// Bytes appended to every encoded transaction's calldata to tag its origin. Trailing
    /// calldata beyond the ABI-encoded arguments is ignored by the EVM, so the tag is free of
    /// on-chain effect. `None` (the default) appends nothing.
    calldata_watermark: Option<Vec<u8>>,
}

/// Maps a successful quote onto an encodable solution, leaving `min_amount_out` equal to the
/// quoted output. That is the widest floor the router accepts; callers that emit calldata must
/// use `solution_from_quote` to supply the fee- and slippage-adjusted floor instead. The user
/// transfer type is not part of the quote either — callers apply it from their `EncodingOptions`
/// via `with_user_transfer_type`.
impl TryFrom<&OrderQuote> for Solution {
    type Error = SolveError;

    fn try_from(quote: &OrderQuote) -> Result<Self, Self::Error> {
        solution_from_quote(quote, quote.amount_out().clone())
    }
}

/// Maps a successful quote onto an encodable solution with an explicit `min_amount_out`.
///
/// `min_amount_out` is the router's revert guardrail: it rejects a value above
/// `expected_amount_out` (the quoted output) or more than `MAX_SLIPPAGE_TOLERANCE_BPS` below it.
fn solution_from_quote(
    quote: &OrderQuote,
    min_amount_out: BigUint,
) -> Result<Solution, SolveError> {
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
        min_amount_out,
        swaps,
    ))
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
        // Without a router address there is no locker to authorize, and `encode` already fails on
        // this chain, so an exclusive leg could not be encoded either way.
        let exclusive_swap_signer = match &router_address {
            Some(router) => ExclusiveSwapSigner::from_env(chain.id(), router)?,
            None => None,
        };
        Ok(Self {
            tycho_encoder,
            chain,
            router_address,
            router_fees: SharedRouterFees::default(),
            exclusive_swap_signer,
            calldata_watermark: None,
        })
    }

    /// Sets a watermark appended to every encoded transaction's calldata (e.g. `"fynd"`), so
    /// on-chain observers can attribute router calls to this deployment. The EVM ignores
    /// calldata past the ABI-encoded arguments, so the watermark does not change execution.
    #[must_use]
    pub fn with_calldata_watermark(mut self, watermark: impl Into<Vec<u8>>) -> Self {
        self.calldata_watermark = Some(watermark.into());
        self
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

    /// Returns the chain this encoder targets.
    pub fn chain(&self) -> Chain {
        self.chain
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

        let router_fees = self.router_fees.snapshot();
        let mut to_encode: Vec<(usize, Solution, FeeBreakdown, FeeRates)> = Vec::new();

        for (i, quote) in quotes.iter_mut().enumerate() {
            if quote.status() != QuoteStatus::Success {
                continue;
            }

            // Mirror FeeCalculator._resolveClient: custom router fees are looked up by the client
            // fee receiver; without client fee params the contract falls back to tx.origin, for
            // which the order sender is our best available proxy.
            let fee_client = encoding_options
                .client_fee_params()
                .map_or_else(|| quote.sender(), |f| f.receiver());
            let fee_rates = router_fees.fees_for(fee_client);
            let fee_breakdown = Self::calculate_fee_breakdown(
                quote.amount_out(),
                encoding_options
                    .client_fee_params()
                    .map_or(0, |f| f.bps()),
                slippage,
                fee_rates,
            )?;
            Self::check_slippage_guardrail(
                biguint_to_u256(quote.amount_out()),
                biguint_to_u256(fee_breakdown.min_amount_received()),
            )?;

            let solution = solution_from_quote(
                quote,
                fee_breakdown
                    .min_amount_received()
                    .clone(),
            )?
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
            to_encode.push((i, solution, fee_breakdown, fee_rates));
        }

        let solutions: Vec<Solution> = to_encode
            .iter()
            .map(|(_, s, _, _)| s.clone())
            .collect();
        let encoded_solutions = tycho_encoder.encode_solutions(solutions)?;

        for (encoded_solution, (idx, solution, fee_breakdown, fee_rates)) in encoded_solutions
            .into_iter()
            .zip(to_encode)
        {
            quotes[idx].set_gas_estimate(encoded_solution.estimated_gas().clone());
            let (transaction, fee_breakdown) = self.encode_tycho_router_call(
                encoded_solution,
                &solution,
                &encoding_options,
                fee_breakdown,
                fee_rates,
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

        // `route_swaps` carry `committed_amount_out` and the component attributes;
        // `solution.swaps()`
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
    /// Both amounts the router compares come off `solution`: `expected_amount_out`, its reference
    /// for positive and negative slippage, and `min_amount_out`, the post-fee floor below which it
    /// reverts.
    fn encode_tycho_router_call(
        &self,
        encoded_solution: EncodedSolution,
        solution: &Solution,
        encoding_options: &EncodingOptions,
        fee_breakdown: FeeBreakdown,
        fee_rates: FeeRates,
    ) -> Result<(Transaction, FeeBreakdown), EncodingError> {
        let amount_in = biguint_to_u256(solution.amount_in());
        let expected_amount_out = biguint_to_u256(solution.expected_amount_out());
        let min_amount_out = biguint_to_u256(solution.min_amount_out());
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
            // The router takes the client fee in the FeeCalculator's fee units, while Fynd's
            // API expresses it in legacy basis points.
            let fee_units = fee_rates.client_fee_units(fee.bps());
            let fee_units = u32::try_from(fee_units).map_err(|_| {
                EncodingError::FatalError(format!(
                    "client fee ({} bps) scales to {fee_units} fee units, which overflows the \
                     router's uint32 clientFeeBps",
                    fee.bps()
                ))
            })?;
            (
                fee_units,
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
            (0u32, Address::ZERO, U256::ZERO, U256::MAX, vec![])
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
                    expected_amount_out,
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
                    expected_amount_out,
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
                expected_amount_out,
                min_amount_out,
                U256::from(encoded_solution.n_tokens()),
                receiver,
                client_fee_params,
                swaps,
            )
                .abi_encode()
        } else if fn_sig.contains("singleSwap") || fn_sig.contains("sequentialSwap") {
            (
                amount_in,
                token_in,
                token_out,
                expected_amount_out,
                min_amount_out,
                receiver,
                client_fee_params,
                swaps,
            )
                .abi_encode()
        } else {
            return Err(EncodingError::FatalError(format!(
                "unsupported function signature for Tycho router: {fn_sig}"
            )));
        };

        let mut contract_interaction =
            Self::encode_input(encoded_solution.function_signature(), method_calldata);
        if let Some(watermark) = &self.calldata_watermark {
            contract_interaction.extend_from_slice(watermark);
        }

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

    /// Rejects calldata the router would revert on.
    ///
    /// `TychoRouter` reverts with `TychoRouter__InvalidMinAmountOut` when `minAmountOut` is above
    /// `expectedAmountOut` or more than `MAX_SLIPPAGE_TOLERANCE_BPS` below it, so fees plus
    /// slippage may not eat more than 20% of the quoted output.
    fn check_slippage_guardrail(
        expected_amount_out: U256,
        min_amount_out: U256,
    ) -> Result<(), EncodingError> {
        let floor = expected_amount_out * U256::from(BPS_DENOMINATOR - MAX_SLIPPAGE_TOLERANCE_BPS) /
            U256::from(BPS_DENOMINATOR);
        if min_amount_out > expected_amount_out || min_amount_out < floor {
            return Err(EncodingError::FatalError(format!(
                "minimum amount out {min_amount_out} is outside the router's accepted range \
                 [{floor}, {expected_amount_out}] for the quoted output; reduce slippage or the \
                 client fee"
            )));
        }
        Ok(())
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

    /// Whether the quote's pAMM legs fall back to a Uniswap V3 fill that still pays the user's
    /// `min_amount_out`.
    ///
    /// A pAMM leg fills on the venue when the maker's quote reaches the chain, and on a Uniswap V3
    /// pool when it does not. `min_amount_out` keeps describing the venue quote and the slippage
    /// the user accepted, so a fallback below that floor reverts. Such a quote must be dropped
    /// before the router picks it, so the next-best candidate is quoted instead.
    ///
    /// Returns `true` for a quote whose route carries no fallback amount — there is no floor to
    /// miss. The fee math mirrors `encode`, so both read the same floor for the same quote.
    pub(crate) fn fallback_clears_min_amount_out(
        &self,
        quote: &OrderQuote,
        encoding_options: &EncodingOptions,
    ) -> Result<bool, SolveError> {
        let Some(fallback_amount_out) = quote
            .route()
            .and_then(|route| route.fallback_amount_out())
        else {
            return Ok(true);
        };

        let client_fee_bps = encoding_options
            .client_fee_params()
            .map_or(0, |f| f.bps());
        let fee_client = encoding_options
            .client_fee_params()
            .map_or_else(|| quote.sender(), |f| f.receiver());
        let fee_rates = self
            .router_fees
            .snapshot()
            .fees_for(fee_client);
        let floor = Self::calculate_fee_breakdown(
            quote.amount_out(),
            client_fee_bps,
            encoding_options.slippage(),
            fee_rates,
        )?;

        Ok(Self::fallback_clears_floor(
            floor.min_amount_received(),
            fallback_amount_out,
            client_fee_bps,
            fee_rates,
        )?)
    }

    /// Whether a pAMM route's Uniswap V3 fallback fill clears the floor the router checks.
    ///
    /// `floor` is `min_amount_out`: the venue quote, less fees, less the slippage the user
    /// accepted. The fallback pays less than the venue quote, and when it pays less than the floor
    /// the router reverts. Lowering the floor to fit would hand the user less than the slippage
    /// they accepted, so the quote is dropped instead.
    ///
    /// A fallback fill credits `fallback_amount_out` less fees. Running the fee math at zero
    /// slippage returns exactly that amount, which is what the on-chain post-fee check compares
    /// against the floor.
    fn fallback_clears_floor(
        floor: &BigUint,
        fallback_amount_out: &BigUint,
        client_fee_bps: u16,
        fee_rates: FeeRates,
    ) -> Result<bool, EncodingError> {
        let fallback_fill =
            Self::calculate_fee_breakdown(fallback_amount_out, client_fee_bps, 0.0, fee_rates)?;
        Ok(fallback_fill.min_amount_received() >= floor)
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
        // Scale the client fee from legacy bps (10_000 = 100%) to the fee units the router
        // takes in calldata, so both fee types share the same denominator.
        let scaled_client_fee = fee_rates.client_fee_units(client_fee_bps);
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

/// Returns whether the quote routes through an exclusive component, i.e. any swap in its route
/// carries a committed amount.
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
    use alloy::primitives::{Address as EvmAddress, Bytes as EvmBytes};
    use num_bigint::BigUint;
    use rstest::rstest;
    use rustc_hash::FxHashMap;
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
        encoding::router_fees::RouterFees,
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
        let component_addr = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
        crate::types::Swap::new(
            component_addr.to_string(),
            "uniswap_v2".to_string(),
            token_in,
            token_out,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(50_000u64),
            component(component_addr, &[tin, tout]),
            Box::new(MockProtocolSim::default()),
        )
    }

    /// Builds a `Route` with both swaps and the token map populated, mirroring
    /// what the algorithms do in production.
    fn make_route_with_tokens(pairs: &[(Address, Address)]) -> crate::types::Route {
        let mut tokens = rustc_hash::FxHashMap::default();
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
        router_fees.set(RouterFees::new(
            FEE_SCALE,
            100_000,
            20_000_000,
            rustc_hash::FxHashMap::default(),
        ));
        Encoder {
            tycho_encoder: Some(Box::new(MockTychoEncoder)),
            chain,
            router_address: Some(Bytes::from([0u8; 20].as_ref())),
            router_fees,
            exclusive_swap_signer: None,
            calldata_watermark: None,
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
        assert_eq!(*solution.expected_amount_out(), *quote.amount_out());
        // `TryFrom` leaves the floor at the quoted output; only `encode` narrows it.
        assert_eq!(*solution.min_amount_out(), *quote.amount_out());
        assert_eq!(solution.swaps().len(), 1);
    }

    const CONTROLLER_KEY: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";
    const LOCKER: alloy::primitives::Address = alloy::primitives::Address::repeat_byte(0x77);

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
        let tokens = FxHashMap::from_iter([
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
        let signer = ExclusiveSwapSigner::new(CONTROLLER_KEY.parse().unwrap(), 1, 0, 120, LOCKER);

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
            .set(RouterFees::new(FEE_SCALE, 100_000, 20_000_000, rustc_hash::FxHashMap::default()));
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

    /// Argument layout of `singleSwap(uint256,address,address,uint256,uint256,address,
    /// (uint32,address,uint256,uint256,bytes),bytes)`.
    type SingleSwapCalldata = (
        U256,
        EvmAddress,
        EvmAddress,
        U256,
        U256,
        EvmAddress,
        (u32, EvmAddress, U256, U256, EvmBytes),
        EvmBytes,
    );

    #[tokio::test]
    async fn test_encode_calldata_amounts_and_client_fee_units() {
        let encoder = real_encoder();
        let quote = make_order_quote(1_000_000_000)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let amount_in = quote.amount_in().clone();
        let amount_out = quote.amount_out().clone();
        let opts = EncodingOptions::new(0.01).with_client_fee_params(make_client_fee(100));

        let result = encoder
            .encode(vec![quote], opts)
            .await
            .unwrap();

        let tx = result[0].transaction().unwrap();
        let breakdown = result[0].fee_breakdown().unwrap();
        let (encoded_amount_in, _, _, expected_amount_out, min_amount_out, _, client_fee, _) =
            <SingleSwapCalldata as SolValue>::abi_decode_params(&tx.data()[4..]).unwrap();

        assert_eq!(encoded_amount_in, biguint_to_u256(&amount_in));
        // The quoted output is the router's positive-slippage baseline.
        assert_eq!(expected_amount_out, biguint_to_u256(&amount_out));
        assert_eq!(min_amount_out, biguint_to_u256(breakdown.min_amount_received()));
        assert!(min_amount_out < expected_amount_out);
        // 100 bps, scaled into the FeeCalculator's 1e8 fee units.
        assert_eq!(client_fee.0, 1_000_000);
    }

    #[tokio::test]
    async fn test_encode_rejects_slippage_beyond_router_guardrail() {
        let encoder = real_encoder();
        let quote = make_order_quote(1_000_000_000)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        // The router accepts a minAmountOut at most 20% below the quoted output.
        let err = encoder
            .encode(vec![quote], EncodingOptions::new(0.25))
            .await
            .expect_err("25% slippage must be rejected before it reaches the router");

        assert!(
            err.to_string()
                .contains("outside the router's accepted range"),
            "got {err:?}"
        );
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

    /// Encodes one quote whose route carries `fallback_amount_out`, at 1% slippage on a quoted
    /// 990 out.
    async fn encode_with_fallback(fallback_amount_out: u64) -> OrderQuote {
        let encoder = real_encoder();
        let mut route = make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]);
        route.set_fallback_amount_out(BigUint::from(fallback_amount_out));
        let quote = make_order_quote(990).with_route(route);

        encoder
            .encode(vec![quote], EncodingOptions::new(0.01))
            .await
            .expect("encode")
            .remove(0)
    }

    /// A fallback that pays less than the user's accepted slippage misses the floor: the floor
    /// stays where the user put it, so the route would only revert. The router drops such a
    /// candidate before ranking.
    #[rstest]
    #[case::below_the_floor(500, false)]
    #[case::above_the_floor(985, true)]
    fn test_fallback_clears_min_amount_out(#[case] fallback: u64, #[case] clears: bool) {
        let encoder = real_encoder();
        let mut route = make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]);
        route.set_fallback_amount_out(BigUint::from(fallback));
        let quote = make_order_quote(990).with_route(route);

        assert_eq!(
            encoder
                .fallback_clears_min_amount_out(&quote, &EncodingOptions::new(0.01))
                .expect("floor check"),
            clears
        );
    }

    /// A route without a pAMM leg carries no fallback amount, so there is no floor to miss.
    #[test]
    fn test_fallback_clears_min_amount_out_without_a_fallback_amount() {
        let encoder = real_encoder();
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        assert!(encoder
            .fallback_clears_min_amount_out(&quote, &EncodingOptions::new(0.01))
            .expect("floor check"));
    }

    /// A fallback that clears the floor changes nothing: `min_amount_out` still describes the
    /// venue quote less fees and the user's slippage.
    #[tokio::test]
    async fn test_encode_keeps_floor_when_fallback_clears_it() {
        let encoder = real_encoder();
        let venue_quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let venue = encoder
            .encode(vec![venue_quote], EncodingOptions::new(0.01))
            .await
            .expect("encode")
            .remove(0);

        let with_fallback = encode_with_fallback(985).await;

        let venue_fees = venue
            .fee_breakdown()
            .expect("venue fee breakdown");
        let fallback_fees = with_fallback
            .fee_breakdown()
            .expect("fallback fee breakdown");

        assert_eq!(with_fallback.status(), QuoteStatus::Success);
        assert!(with_fallback.transaction().is_some());
        assert_eq!(fallback_fees.min_amount_received(), venue_fees.min_amount_received());
        assert_eq!(fallback_fees.router_fee(), venue_fees.router_fee());
        assert_eq!(fallback_fees.client_fee(), venue_fees.client_fee());
        assert_eq!(fallback_fees.max_slippage(), venue_fees.max_slippage());
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

    // ==================== Calldata Watermark Tests ====================

    #[tokio::test]
    async fn test_encode_appends_calldata_watermark() {
        let encoder = real_encoder().with_calldata_watermark("fynd");
        let quote = make_order_quote(990)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));

        let result = encoder
            .encode(vec![quote], EncodingOptions::new(0.01))
            .await
            .unwrap();

        let tx = result[0].transaction().unwrap();
        assert!(
            tx.data().ends_with(b"fynd"),
            "calldata must end with the watermark bytes, got suffix {:?}",
            &tx.data()[tx.data().len().saturating_sub(4)..]
        );
    }

    #[tokio::test]
    async fn test_encode_without_watermark_leaves_calldata_unchanged() {
        let make_quote = || {
            make_order_quote(990)
                .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]))
        };

        let plain = real_encoder()
            .encode(vec![make_quote()], EncodingOptions::new(0.01))
            .await
            .unwrap();
        let watermarked = real_encoder()
            .with_calldata_watermark("fynd")
            .encode(vec![make_quote()], EncodingOptions::new(0.01))
            .await
            .unwrap();

        let plain_data = plain[0].transaction().unwrap().data();
        let watermarked_data = watermarked[0]
            .transaction()
            .unwrap()
            .data();
        // The watermark is a pure suffix: stripping it yields the unwatermarked calldata.
        assert_eq!(*plain_data, watermarked_data[..watermarked_data.len() - 4]);
    }

    #[tokio::test]
    async fn test_watermarked_calldata_still_decodes() {
        let encoder = real_encoder().with_calldata_watermark("fynd");
        let quote = make_order_quote(1_000_000_000)
            .with_route(make_route_with_tokens(&[(make_address(0x01), make_address(0x02))]));
        let amount_in = quote.amount_in().clone();
        let opts = EncodingOptions::new(0.01).with_client_fee_params(make_client_fee(100));

        let result = encoder
            .encode(vec![quote], opts)
            .await
            .unwrap();

        let tx = result[0].transaction().unwrap();
        // Solidity's ABI decoder ignores trailing calldata, so decoding the args without the
        // 4-byte watermark suffix must still work.
        let (encoded_amount_in, _, _, _, _, _, _, _) =
            <SingleSwapCalldata as SolValue>::abi_decode_params(&tx.data()[4..tx.data().len() - 4])
                .unwrap();
        assert_eq!(encoded_amount_in, biguint_to_u256(&amount_in));
    }

    #[tokio::test]
    async fn test_signature_offset_unaffected_by_watermark() {
        let encoder = real_encoder().with_calldata_watermark("fynd");
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
        assert!(calldata.ends_with(b"fynd"));
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
        let custom =
            FxHashMap::from_iter([(Bytes::from(make_address(0xBB).as_ref()), (0u32, 0u32))]);
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
        let custom = FxHashMap::from_iter([(
            Bytes::from(make_address(0xAA).as_ref()),
            (0u32, 20_000_000u32),
        )]);
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
            .set(RouterFees::new(
                FEE_SCALE,
                1_000_000,
                20_000_000,
                rustc_hash::FxHashMap::default(),
            ));
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
