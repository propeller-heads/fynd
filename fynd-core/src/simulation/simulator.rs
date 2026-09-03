//! Simulation of encoded quotes using `eth_simulateV1` state overrides.

use std::sync::Mutex;

use alloy::{
    network::Ethereum,
    primitives::{address, map::B256HashMap, Address, Bytes, TxKind, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::{AccountOverride, StateOverride},
        BlockOverrides, TransactionRequest,
    },
    sol,
    sol_types::SolError,
};
use metrics::{counter, histogram};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use tokio::time::timeout;
use tycho_simulation::tycho_common::models::Chain;

use crate::{
    encoding::encoder::PERMIT2_ADDRESS,
    simulation::erc20_slots::{find_slot_positions, Erc20SlotPositions},
    solver::defaults::SIMULATION_SLOT_DISCOVERY_TIMEOUT,
    OrderQuote, SimulationResult,
};

sol! { error Error(string); }

/// Balance and allowance every simulated account is given.
///
/// Leaving the high bit clear avoids node-side arithmetic overflow while still funding any
/// practical quote.
const SIMULATION_FUNDING_VALUE: U256 = U256::MAX.wrapping_shr(1);

/// Gas limit for the simulated call, as a multiple of the gas the quote estimated.
///
/// A call that sets no limit inherits the block's, and a pool that reads `gasleft()` can tell that
/// budget apart from a real swap and answer differently. Twice the estimate covers the variance a
/// real transaction meets while still reading like one.
const SIMULATION_GAS_LIMIT_MULTIPLIER: u64 = 2;

/// Floor for the simulated call's gas limit, so a small estimate still leaves room to execute.
const SIMULATION_MIN_GAS_LIMIT: u64 = 500_000;

/// Ceiling for the simulated call's gas limit.
const SIMULATION_MAX_GAS_LIMIT: u64 = 30_000_000;

/// Gas limit reported for the simulated block.
const SIMULATION_BLOCK_GAS_LIMIT: u64 = 45_000_000;

/// Gas price for a quote that carries none, so `tx.gasprice` never reads zero.
const SIMULATION_FALLBACK_GAS_PRICE: u128 = 1_000_000_000;

/// Fee recipient for the simulated block. A live builder, because zero reads as a simulation.
const SIMULATION_COINBASE: Address = address!("0x95222290DD7278Aa3Ddd389Cc1E1d165CC4BAfe5");

/// Nonce given to the sender, because an account that never sent a transaction reads as a
/// simulation.
const SIMULATION_SENDER_NONCE: u64 = 1;

/// Simulates encoded quote transactions with temporary sender funding.
pub struct QuoteSimulator {
    provider: RootProvider<Ethereum>,
    slot_cache: Mutex<FxHashMap<Address, Erc20SlotPositions>>,
    native_token: Address,
    request_timeout: std::time::Duration,
}

/// The transaction envelope a simulated call runs under.
///
/// A pool can read the gas budget and the gas price, so both are taken from what the quote itself
/// priced rather than left at the node's defaults of "the whole block" and zero.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SimulationEnvelope {
    gas_limit: u64,
    gas_price: u128,
}

impl SimulationEnvelope {
    fn for_quote(quote: &OrderQuote) -> Self {
        Self::new(
            quote.gas_estimate().to_u64(),
            quote
                .gas_price()
                .and_then(ToPrimitive::to_u128),
        )
    }

    /// A quote that carries neither figure falls back to the floor and a nominal price, which is
    /// still closer to a real transaction than the node's defaults.
    fn new(gas_estimate: Option<u64>, gas_price: Option<u128>) -> Self {
        let gas_limit = gas_estimate
            .map_or(SIMULATION_MIN_GAS_LIMIT, |estimate| {
                estimate.saturating_mul(SIMULATION_GAS_LIMIT_MULTIPLIER)
            })
            .clamp(SIMULATION_MIN_GAS_LIMIT, SIMULATION_MAX_GAS_LIMIT);
        Self { gas_limit, gas_price: gas_price.unwrap_or(SIMULATION_FALLBACK_GAS_PRICE) }
    }
}

pub(crate) enum SimulationAttempt {
    /// The simulated call completed.
    Success { amount_out: BigUint, gas_used: u64 },
    /// The simulated call reverted.
    Reverted { reason: String },
    /// Simulation could not be completed.
    Failure { reason: String },
}

impl QuoteSimulator {
    /// Creates a simulator that sends requests to `rpc_url` for `chain`.
    ///
    /// # Errors
    ///
    /// Returns an error when `rpc_url` is not a valid URL or the chain has no native token.
    pub fn new(
        rpc_url: &str,
        chain: Chain,
        request_timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let url = rpc_url
            .parse()
            .map_err(|error| format!("invalid RPC URL {rpc_url:?}: {error}"))?;
        let native_token = chain
            .try_native_token()
            .map_err(|error| format!("native token for {chain:?}: {error}"))?;
        Ok(Self::with_provider(
            ProviderBuilder::default().connect_http(url),
            Address::from_slice(native_token.address.as_ref()),
            request_timeout,
        ))
    }

    /// Simulates an encoded quote and reports its returned amount and gas used or a failure.
    ///
    /// Records the outcome and, on success, how far the simulated amount sits from what the quote
    /// promised. Instrumenting here rather than at the call site keeps every caller measured.
    pub(crate) async fn simulate_attempt(&self, quote: &OrderQuote) -> SimulationAttempt {
        let attempt = self.attempt(quote).await;
        record_outcome(quote, &attempt);
        attempt
    }

    async fn attempt(&self, quote: &OrderQuote) -> SimulationAttempt {
        let transaction = match quote.transaction() {
            Some(value) => value,
            None => return failure("simulation setup failed: quote has no encoded transaction"),
        };
        let route = match quote.route() {
            Some(value) => value,
            None => return failure("simulation setup failed: quote has no route"),
        };
        let first_swap = match route.swaps().first() {
            Some(value) => value,
            None => return failure("simulation setup failed: route has no swaps"),
        };
        let sender = match crate::rpc::to_address(quote.sender(), "quote sender") {
            Ok(value) => value,
            Err(error) => return failure_with(format!("simulation setup failed: {error}")),
        };
        let router = match crate::rpc::to_address(transaction.to(), "transaction destination") {
            Ok(value) => value,
            Err(error) => return failure_with(format!("simulation setup failed: {error}")),
        };
        let token_in = match crate::rpc::to_address(first_swap.token_in(), "route token_in") {
            Ok(value) => value,
            Err(error) => return failure_with(format!("simulation setup failed: {error}")),
        };
        let overrides = match self
            .overrides(sender, token_in, router)
            .await
        {
            Ok(value) => value,
            Err(reason) => return failure_with(reason),
        };
        let value = U256::from_be_slice(
            transaction
                .value()
                .to_bytes_be()
                .as_slice(),
        );
        self.simulate_within_timeout(
            sender,
            router,
            value,
            transaction.data(),
            overrides,
            SimulationEnvelope::for_quote(quote),
        )
        .await
    }

    /// Runs one simulated call, reporting a timeout as a failure rather than waiting forever.
    pub(crate) async fn simulate_within_timeout(
        &self,
        sender: Address,
        router: Address,
        value: U256,
        data: &[u8],
        overrides: StateOverride,
        envelope: SimulationEnvelope,
    ) -> SimulationAttempt {
        match timeout(
            self.request_timeout,
            simulate_with_overrides(
                &self.provider,
                sender,
                router,
                value,
                data,
                overrides,
                envelope,
            ),
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(_) => failure_with(format!(
                "simulation request timed out after {:?}",
                self.request_timeout
            )),
        }
    }

    pub(crate) fn with_provider(
        provider: RootProvider<Ethereum>,
        native_token: Address,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            provider,
            slot_cache: Mutex::new(FxHashMap::default()),
            native_token,
            request_timeout,
        }
    }

    async fn overrides(
        &self,
        sender: Address,
        token: Address,
        router: Address,
    ) -> Result<StateOverride, String> {
        if token == self.native_token {
            return Ok(native_balance_override(sender));
        }
        let permit2: Address = PERMIT2_ADDRESS
            .parse()
            .map_err(|error| format!("invalid Permit2 address: {error}"))?;
        let positions = self
            .cached_positions(token, sender, router)
            .await?;
        Ok(token_overrides(sender, token, router, permit2, positions))
    }

    async fn cached_positions(
        &self,
        token: Address,
        holder: Address,
        spender: Address,
    ) -> Result<Erc20SlotPositions, String> {
        if let Some(positions) = self
            .slot_cache
            .lock()
            .map_err(|_| "simulation slot cache lock poisoned".to_string())?
            .get(&token)
            .copied()
        {
            return Ok(positions);
        }
        let positions = timeout(SIMULATION_SLOT_DISCOVERY_TIMEOUT, find_slot_positions(&self.provider, token, holder, spender)).await
            .map_err(|_| format!("simulation ERC-20 slot discovery timed out after {SIMULATION_SLOT_DISCOVERY_TIMEOUT:?}"))?
            .map_err(|error| format!("simulation ERC-20 slot detection failed: {error}"))?;
        self.slot_cache
            .lock()
            .map_err(|_| "simulation slot cache lock poisoned".to_string())?
            .insert(token, positions);
        Ok(positions)
    }
}

impl SimulationAttempt {
    pub(crate) fn into_result(self) -> SimulationResult {
        match self {
            Self::Success { amount_out, gas_used } => {
                SimulationResult::Success { amount_out, gas_used }
            }
            Self::Reverted { reason } | Self::Failure { reason } => {
                SimulationResult::Failure { reason }
            }
        }
    }
}

/// Records the outcome of one simulated quote, and on success how far the simulated amount landed
/// from what the quote promised.
///
/// A revert and a failure are counted apart because they call for different work: a revert means
/// the route the solver priced does not execute, while a failure means the simulation itself did
/// not run, so it says nothing about the route. Both carry the winning pool and algorithm, so a
/// rise in either can be traced to the solver that produced the route.
fn record_outcome(quote: &OrderQuote, attempt: &SimulationAttempt) {
    let pool = quote.worker_pool().to_string();
    let algorithm = quote.algorithm().to_string();
    let outcome = match attempt {
        SimulationAttempt::Success { amount_out, .. } => {
            if let Some(deviation) = deviation_bps(quote, amount_out) {
                histogram!(
                    "quote_simulation_deviation_bps",
                    "pool" => pool.clone(),
                    "algorithm" => algorithm.clone()
                )
                .record(deviation);
            }
            "success"
        }
        SimulationAttempt::Reverted { .. } => "reverted",
        SimulationAttempt::Failure { .. } => "failed",
    };
    counter!(
        "quote_simulations_total",
        "outcome" => outcome,
        "pool" => pool,
        "algorithm" => algorithm
    )
    .increment(1);
}

/// How far the simulated amount sits from what the quote promised, in basis points.
///
/// The router returns the output after it takes the router and client fees, so the quote's own
/// `amount_out`, which is the raw swap output, is not the same quantity and would read as a
/// standing fee-sized gap. The comparison is against the quoted amount less those same fees.
///
/// A negative value means the simulation returned less than the quote promised. Returns `None`
/// when the quote states no fees, or when the quoted amount is zero and there is no ratio to take.
fn deviation_bps(quote: &OrderQuote, simulated_amount_out: &BigUint) -> Option<f64> {
    let fees = quote.fee_breakdown()?;
    // The quoted amount after fees, reached by addition because `min_amount_received` is that
    // amount less the slippage the user accepted.
    let quoted = fees.min_amount_received() + fees.max_slippage();
    if quoted == BigUint::ZERO {
        return None;
    }
    let quoted = quoted.to_f64()?;
    let simulated = simulated_amount_out.to_f64()?;
    let deviation = (simulated - quoted) / quoted * 10_000.0;
    deviation
        .is_finite()
        .then_some(deviation)
}

fn failure(reason: &str) -> SimulationAttempt {
    failure_with(reason.to_string())
}
fn failure_with(reason: String) -> SimulationAttempt {
    SimulationAttempt::Failure { reason }
}

/// Block environment for a simulated call.
///
/// `eth_simulateV1` builds on the real head, so the block number, timestamp, base fee, chain id and
/// the ancestor hashes `blockhash` reads are already the ones the next block will carry. What it
/// leaves at zero is what a pool can read to recognise a simulation, so those are set here.
fn block_overrides() -> BlockOverrides {
    BlockOverrides {
        coinbase: Some(SIMULATION_COINBASE),
        random: Some(B256::from(rand::random::<[u8; 32]>())),
        gas_limit: Some(SIMULATION_BLOCK_GAS_LIMIT),
        ..Default::default()
    }
}

async fn simulate_with_overrides(
    provider: &RootProvider<Ethereum>,
    sender: Address,
    router: Address,
    value: U256,
    data: &[u8],
    overrides: StateOverride,
    envelope: SimulationEnvelope,
) -> SimulationAttempt {
    let call = TransactionRequest {
        from: Some(sender),
        to: Some(TxKind::Call(router)),
        value: Some(value),
        input: Bytes::copy_from_slice(data).into(),
        gas: Some(envelope.gas_limit),
        gas_price: Some(envelope.gas_price),
        ..Default::default()
    };
    let payload = SimulatePayload::default().extend(
        SimBlock::default()
            .with_state_overrides(overrides)
            .with_block_overrides(block_overrides())
            .call(call),
    );
    let response = match provider.simulate(&payload).await {
        Ok(value) => value,
        Err(error) => {
            return failure_with(format!(
                "simulation eth_simulateV1 failed: {}",
                rpc_error_reason(&error)
            ))
        }
    };
    let Some(block) = response.first() else {
        return failure("simulation eth_simulateV1 returned no blocks");
    };
    let Some(result) = block.calls.first() else {
        return failure("simulation eth_simulateV1 returned no call results");
    };
    if !result.status {
        let message = result
            .error
            .as_ref()
            .map_or("execution reverted", |error| error.message.as_str());
        return SimulationAttempt::Reverted {
            reason: format!(
                "simulation reverted: {}",
                decode_revert_reason(Some(result.return_data.as_ref()), message)
            ),
        };
    }
    if result.return_data.len() != 32 {
        return failure_with(format!(
            "simulation eth_simulateV1 returned {} bytes; expected exactly 32 bytes for uint256",
            result.return_data.len()
        ));
    }
    let amount = U256::from_be_slice(&result.return_data);
    SimulationAttempt::Success {
        amount_out: BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
        gas_used: result.gas_used,
    }
}

fn rpc_error_reason(
    error: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>,
) -> String {
    let response = error.as_error_resp();
    let data = response.and_then(|response| response.as_revert_data());
    let message =
        response.map_or_else(|| error.to_string(), |response| response.message.to_string());
    decode_revert_reason(data.as_deref().map(AsRef::as_ref), &message)
}

/// Funds the sender and gives it a used-account nonce.
fn sender_override() -> AccountOverride {
    AccountOverride {
        balance: Some(SIMULATION_FUNDING_VALUE),
        nonce: Some(SIMULATION_SENDER_NONCE),
        ..Default::default()
    }
}

fn native_balance_override(sender: Address) -> StateOverride {
    StateOverride::from_iter([(sender, sender_override())])
}

/// Funds and approves every account a route can pull the input token from.
///
/// The sender holds the funds for a `transfer_from` route and the router holds them for a
/// `use_vaults_funds` one; the spender is the router directly, or Permit2 when the route carries a
/// permit. Which of those a quote uses is settled by calldata the simulation does not read, so all
/// of them are funded and approved rather than asking the caller which to prepare.
fn token_overrides(
    sender: Address,
    token: Address,
    router: Address,
    permit2: Address,
    positions: Erc20SlotPositions,
) -> StateOverride {
    let funding = B256::from(SIMULATION_FUNDING_VALUE);
    let mut state_diff = B256HashMap::default();
    for holder in [sender, router] {
        state_diff.insert(positions.balance_slot(holder), funding);
    }
    for spender in [router, permit2] {
        state_diff.insert(positions.allowance_slot(sender, spender), funding);
    }
    StateOverride::from_iter([
        (sender, sender_override()),
        (token, AccountOverride { state_diff: Some(state_diff), ..Default::default() }),
    ])
}

fn decode_revert_reason(revert_data: Option<&[u8]>, message: &str) -> String {
    let Some(data) = revert_data else {
        return message.to_string();
    };
    if data.starts_with(&Error::SELECTOR) {
        if let Ok(error) = Error::abi_decode_raw(&data[4..]) {
            return format!("reverted: {}", error.0);
        }
    }
    format!("reverted with data 0x{}", alloy::hex::encode(data))
}

#[cfg(test)]
#[path = "../tests/simulation/simulator.rs"]
mod tests;
