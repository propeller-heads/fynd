//! Simulation of encoded quotes using `eth_simulateV1` state overrides.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy::{
    eips::BlockNumberOrTag,
    network::Ethereum,
    primitives::{address, map::B256HashMap, uint, Address, Bytes, TxKind, B256, U256},
    providers::{ext::DebugApi, Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::{AccountOverride, StateOverride},
        trace::geth::{
            CallConfig, GethDebugBuiltInTracerType, GethDebugTracerType,
            GethDebugTracingCallOptions, GethDebugTracingOptions,
        },
        BlockOverrides, TransactionRequest,
    },
};
use metrics::{counter, histogram};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use tokio::{sync::OnceCell, time::timeout};
use tracing::debug;
use tycho_simulation::tycho_common::models::Chain;

use crate::{
    encoding::encoder::PERMIT2_ADDRESS,
    simulation::{
        deviation::deviation_bps,
        revert,
        token_layout::{discover_layout, DiscoveryError, TokenLayout},
    },
    solver::defaults::SIMULATION_LAYOUT_DISCOVERY_TIMEOUT,
    OrderQuote, SimulationResult,
};

/// Balance and allowance every simulated account is given.
///
/// `1e36`: a billion billion tokens at 18 decimals, comfortably above any practical input.
///
/// The ceiling matters as much as the floor. A rebasing token derives its balance arithmetically
/// -- stETH multiplies a holder's shares by the pooled-ETH rate -- and `U256::MAX >> 1` overflows
/// that multiplication before the simulated transfer starts. Leaving the high bits clear also
/// keeps a token that packs flags into its balance word reading the value back unchanged.
const SIMULATION_FUNDING_VALUE: U256 =
    uint!(1_000_000_000_000_000_000_000_000_000_000_000_000_U256);

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

/// Budget for the trace that recovers a revert's error.
///
/// Fixed, and far below the request budget: `debug_traceCall` costs a node markedly more than
/// `eth_simulateV1`, and the trace only names a revert the caller already knows about. Reusing the
/// request budget would let the diagnostic cost the caller more than the simulation did.
const SIMULATION_TRACE_TIMEOUT: Duration = Duration::from_millis(500);

/// Seconds `eth_simulateV1` advances the clock by when it builds its block on the head.
const SIMULATION_BLOCK_INTERVAL_SECS: u64 = 12;

/// One token's layout, or the reason this build cannot resolve one, resolved once per token.
type LayoutCell = Arc<OnceCell<Result<TokenLayout, String>>>;

/// The call a simulation runs.
///
/// The four travel together and always come from the same quote, so they are passed as one rather
/// than as four parameters a caller could pair up wrongly.
#[derive(Clone, Copy)]
pub(crate) struct SimulatedCall<'a> {
    pub(crate) sender: Address,
    pub(crate) router: Address,
    pub(crate) value: U256,
    pub(crate) data: &'a [u8],
}

/// Simulates encoded quote transactions with temporary sender funding.
pub struct QuoteSimulator {
    provider: RootProvider<Ethereum>,
    /// Layout per token, or the reason this build cannot resolve one.
    ///
    /// One cell per token, so quotes that arrive for a new token together await one discovery
    /// rather than each running their own trace and probes.
    layout_cache: Mutex<FxHashMap<Address, LayoutCell>>,
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
    /// The block the two calls must both report, when the caller knows which one that is.
    ///
    /// `eth_simulateV1` builds on the head and reports N+1, while `debug_traceCall` at `latest`
    /// reports N. A pool that keys off the height or the clock would then meet two different
    /// environments, and the reason the trace recovers could belong to a different failure than
    /// the one the quote was rejected for. Pinning both leaves them the same.
    block: Option<SimulationBlock>,
}

/// The height and clock a simulated call reports.
#[derive(Clone, Copy, Debug)]
struct SimulationBlock {
    number: u64,
    timestamp: u64,
}

impl SimulationEnvelope {
    fn for_quote(quote: &OrderQuote) -> Self {
        let block = quote.block();
        Self {
            block: Some(SimulationBlock {
                number: block.number() + 1,
                timestamp: block.timestamp() + SIMULATION_BLOCK_INTERVAL_SECS,
            }),
            ..Self::new(
                quote.gas_estimate().to_u64(),
                quote
                    .gas_price()
                    .and_then(ToPrimitive::to_u128),
            )
        }
    }

    /// A quote that carries neither figure falls back to the floor and a nominal price, which is
    /// still closer to a real transaction than the node's defaults.
    fn new(gas_estimate: Option<u64>, gas_price: Option<u128>) -> Self {
        let gas_limit = gas_estimate
            .map_or(SIMULATION_MIN_GAS_LIMIT, |estimate| {
                estimate.saturating_mul(SIMULATION_GAS_LIMIT_MULTIPLIER)
            })
            .clamp(SIMULATION_MIN_GAS_LIMIT, SIMULATION_MAX_GAS_LIMIT);
        Self {
            gas_limit,
            gas_price: gas_price.unwrap_or(SIMULATION_FALLBACK_GAS_PRICE),
            block: None,
        }
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
            SimulatedCall { sender, router, value, data: transaction.data() },
            overrides,
            SimulationEnvelope::for_quote(quote),
        )
        .await
    }

    /// Runs one simulated call, reporting a timeout as a failure rather than waiting forever.
    pub(crate) async fn simulate_within_timeout(
        &self,
        call: SimulatedCall<'_>,
        overrides: StateOverride,
        envelope: SimulationEnvelope,
    ) -> SimulationAttempt {
        match simulate_with_overrides(
            &self.provider,
            call,
            overrides,
            envelope,
            self.request_timeout,
        )
        .await
        {
            CallOutcome::Success { amount_out, gas_used } => {
                SimulationAttempt::Success { amount_out, gas_used }
            }
            CallOutcome::Reverted { reason } => {
                SimulationAttempt::Reverted { reason: format!("simulation reverted: {reason}") }
            }
            CallOutcome::Failure(reason) => SimulationAttempt::Failure { reason },
        }
    }

    pub(crate) fn with_provider(
        provider: RootProvider<Ethereum>,
        native_token: Address,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            provider,
            layout_cache: Mutex::new(FxHashMap::default()),
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
        let layout = self
            .cached_layout(token, sender, router)
            .await?;
        Ok(token_overrides(sender, router, permit2, layout))
    }

    /// The token's storage layout, discovering it on the first quote that needs it.
    ///
    /// A token whose layout this build cannot resolve is remembered as unresolvable: the verdict
    /// is a property of the token, and rediscovering it would spend a trace and its probes on
    /// every quote that touches it. A node that failed to answer decides nothing, so the cell
    /// stays empty and the next quote tries again.
    async fn cached_layout(
        &self,
        token: Address,
        holder: Address,
        spender: Address,
    ) -> Result<TokenLayout, String> {
        let cell = Arc::clone(
            self.layout_cache
                .lock()
                .map_err(|_| "simulation layout cache lock poisoned".to_string())?
                .entry(token)
                .or_default(),
        );
        cell.get_or_try_init(|| self.discover_once(token, holder, spender))
            .await?
            .clone()
            .map_err(|reason| format!("simulation token layout discovery failed: {reason}"))
    }

    /// Runs discovery once, separating a verdict worth remembering from a node that failed.
    ///
    /// The outer `Err` leaves the cell empty, so only the inner one is cached.
    async fn discover_once(
        &self,
        token: Address,
        holder: Address,
        spender: Address,
    ) -> Result<Result<TokenLayout, String>, String> {
        let discovered = timeout(
            SIMULATION_LAYOUT_DISCOVERY_TIMEOUT,
            discover_layout(&self.provider, token, holder, spender),
        )
        .await
        .map_err(|_| {
            format!("simulation token layout discovery failed: timed out after {SIMULATION_LAYOUT_DISCOVERY_TIMEOUT:?}")
        })?;

        match discovered {
            Ok(layout) => Ok(Ok(layout)),
            Err(DiscoveryError::Unsupported(reason)) => Ok(Err(reason)),
            Err(DiscoveryError::Rpc(reason)) => {
                Err(format!("simulation token layout discovery failed: {reason}"))
            }
        }
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

/// Target for the reason a simulated quote reverted or failed. Emitted at DEBUG, so a deployment
/// that wants the reasons sets `RUST_LOG=...,fynd::simulation_outcome=debug`.
const SIMULATION_OUTCOME_TARGET: &str = "fynd::simulation_outcome";

/// Logs why one simulated quote did not return an amount, and names the outcome for the counter.
///
/// The reason names a contract error, so it goes to a log rather than a metric label: the set is
/// open and one unknown selector would be a new series forever.
fn log_outcome(quote: &OrderQuote, outcome: &'static str, reason: &str) -> &'static str {
    debug!(
        target: SIMULATION_OUTCOME_TARGET,
        order_id = quote.order_id(),
        pool = quote.worker_pool(),
        algorithm = quote.algorithm(),
        outcome,
        "{reason}"
    );
    outcome
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
        SimulationAttempt::Reverted { reason } => log_outcome(quote, "reverted", reason),
        SimulationAttempt::Failure { reason } => log_outcome(quote, "failed", reason),
    };
    counter!(
        "quote_simulations_total",
        "outcome" => outcome,
        "pool" => pool,
        "algorithm" => algorithm
    )
    .increment(1);
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
fn block_overrides(envelope: SimulationEnvelope) -> BlockOverrides {
    BlockOverrides {
        coinbase: Some(SIMULATION_COINBASE),
        random: Some(B256::from(rand::random::<[u8; 32]>())),
        gas_limit: Some(SIMULATION_BLOCK_GAS_LIMIT),
        number: envelope
            .block
            .map(|block| U256::from(block.number)),
        time: envelope
            .block
            .map(|block| block.timestamp),
        ..Default::default()
    }
}

/// Runs the simulated call, and names the revert when the call reverted without saying why.
///
/// Each RPC gets a budget of its own: the simulation the caller's, the trace a fixed and much
/// smaller one. A trace that runs out of time leaves the node's own message, so a revert is
/// reported either way.
async fn simulate_with_overrides(
    provider: &RootProvider<Ethereum>,
    simulated: SimulatedCall<'_>,
    overrides: StateOverride,
    envelope: SimulationEnvelope,
    request_timeout: Duration,
) -> CallOutcome {
    let call = TransactionRequest {
        from: Some(simulated.sender),
        to: Some(TxKind::Call(simulated.router)),
        value: Some(simulated.value),
        input: Bytes::copy_from_slice(simulated.data).into(),
        gas: Some(envelope.gas_limit),
        gas_price: Some(envelope.gas_price),
        ..Default::default()
    };
    // The trace has to observe the same environment as `eth_simulateV1` to reproduce the same
    // revert. `prevrandao` is drawn at random per call, so it is built once and reused rather
    // than regenerated.
    let environment = block_overrides(envelope);
    let payload = SimulatePayload::default().extend(
        SimBlock::default()
            .with_state_overrides(overrides.clone())
            .with_block_overrides(environment.clone())
            .call(call.clone()),
    );
    let response = match timeout(request_timeout, provider.simulate(&payload)).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            return CallOutcome::Failure(format!(
                "simulation eth_simulateV1 failed: {}",
                rpc_error_reason(&error)
            ))
        }
        Err(_) => {
            return CallOutcome::Failure(format!(
                "simulation request timed out after {request_timeout:?}"
            ))
        }
    };
    let Some(block) = response.first() else {
        return CallOutcome::Failure("simulation eth_simulateV1 returned no blocks".to_string());
    };
    let Some(result) = block.calls.first() else {
        return CallOutcome::Failure(
            "simulation eth_simulateV1 returned no call results".to_string(),
        );
    };
    if !result.status {
        let message = result
            .error
            .as_ref()
            .map_or("execution reverted", |error| error.message.as_str())
            .to_string();
        if let Some(decoded) = revert::decode_error(&result.return_data) {
            return CallOutcome::Reverted { reason: decoded };
        }
        // `eth_simulateV1` drops the revert payload, so the same call is replayed under the
        // tracer -- same overrides, same environment -- to read the error off the frame that
        // produced it.
        let traced = timeout(
            SIMULATION_TRACE_TIMEOUT,
            traced_revert_reason(provider, call, overrides, environment),
        )
        .await
        .ok()
        .flatten();
        return CallOutcome::Reverted { reason: traced.unwrap_or(message) };
    }
    if result.return_data.len() != 32 {
        return CallOutcome::Failure(format!(
            "simulation eth_simulateV1 returned {} bytes; expected exactly 32 bytes for uint256",
            result.return_data.len()
        ));
    }
    let amount = U256::from_be_slice(&result.return_data);
    CallOutcome::Success {
        amount_out: BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
        gas_used: result.gas_used,
    }
}

/// What one simulated call came back with.
enum CallOutcome {
    Success {
        amount_out: BigUint,
        gas_used: u64,
    },
    /// The call reverted, with the best reason available: the payload's own error, or the one the
    /// trace recovered, or the node's message.
    Reverted {
        reason: String,
    },
    Failure(String),
}

fn rpc_error_reason(
    error: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>,
) -> String {
    let response = error.as_error_resp();
    let message =
        response.map_or_else(|| error.to_string(), |response| response.message.to_string());
    response
        .and_then(|response| response.as_revert_data())
        .and_then(|data| revert::decode_error(data.as_ref()))
        .unwrap_or(message)
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
    router: Address,
    permit2: Address,
    layout: TokenLayout,
) -> StateOverride {
    let funding = B256::from(SIMULATION_FUNDING_VALUE);
    let mut state_diff = B256HashMap::default();
    for holder in [sender, router] {
        state_diff.insert(layout.balance_slot(holder), funding);
    }
    for spender in [router, permit2] {
        state_diff.insert(layout.allowance_slot(sender, spender), funding);
    }
    StateOverride::from_iter([
        (sender, sender_override()),
        (
            layout.storage_contract(),
            AccountOverride { state_diff: Some(state_diff), ..Default::default() },
        ),
    ])
}

/// Replays a reverting call under the call tracer and reads the reason off it.
///
/// Runs against the same block and the same overrides as the simulation, so it reproduces the
/// revert rather than a different one. Returns `None` when the node serves no `debug_traceCall`
/// or the trace carries no reason, which leaves the caller the message it already has.
async fn traced_revert_reason(
    provider: &RootProvider<Ethereum>,
    call: TransactionRequest,
    overrides: StateOverride,
    environment: BlockOverrides,
) -> Option<String> {
    let options = GethDebugTracingCallOptions::default()
        .with_tracing_options(
            GethDebugTracingOptions::default()
                .with_tracer(GethDebugTracerType::BuiltInTracer(
                    GethDebugBuiltInTracerType::CallTracer,
                ))
                .with_call_config(CallConfig { only_top_call: Some(false), with_log: Some(false) }),
        )
        .with_state_overrides(overrides)
        .with_block_overrides(environment);

    match provider
        .debug_trace_call_callframe(call, BlockNumberOrTag::Latest.into(), options)
        .await
    {
        Ok(frame) => revert::reason_from_frame(&frame),
        Err(error) => {
            tracing::debug!(%error, "tracing a reverted simulation failed");
            None
        }
    }
}

#[cfg(test)]
#[path = "../tests/simulation/simulator.rs"]
mod tests;
