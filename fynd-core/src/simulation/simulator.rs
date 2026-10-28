//! Simulation of encoded quotes using `eth_simulateV1` state overrides.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    network::Ethereum,
    primitives::{address, map::B256HashMap, uint, Address, Bytes, TxKind, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        simulate::SimulatedBlock,
        state::{AccountOverride, StateOverride},
        trace::geth::{
            CallConfig, CallFrame, GethDebugBuiltInTracerType, GethDebugTracerType,
            GethDebugTracingOptions,
        },
        Block, TransactionRequest,
    },
};
use metrics::{counter, histogram};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use tokio::{
    sync::OnceCell,
    time::{sleep, timeout, Instant},
};
use tracing::debug;
use tycho_simulation::tycho_common::models::Chain;

use crate::{
    encoding::encoder::PERMIT2_ADDRESS,
    simulation::{
        deviation::deviation_bps,
        revert,
        token_layout::{discover_layout, DiscoveryError, TokenLayout},
        wire::{SimulationBlockEnvironment, SimulationRequest, TraceCallOptions},
    },
    solver::defaults::SIMULATION_LAYOUT_DISCOVERY_TIMEOUT,
    ClientFeeParams, EventLog, OrderQuote, SimulationResult,
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
/// budget apart from a real swap and answer differently. Four times the estimate covers the
/// variance a real transaction meets while still reading like one.
const SIMULATION_GAS_LIMIT_MULTIPLIER: u64 = 4;

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

/// Runtime code that returns ERC-1271's magic value to every call, whatever its calldata or value:
/// the `isValidSignature` selector `0x1626ba7e`, left-aligned in a 32-byte word.
///
/// `PUSH4 0x1626ba7e PUSH1 0xe0 SHL PUSH1 0 MSTORE PUSH1 0x20 PUSH1 0 RETURN`
const ACCEPT_ANY_ERC1271_SIGNATURE_CODE: [u8; 16] =
    alloy::primitives::hex!("631626ba7e60e01b60005260206000f3");

/// Budget for the trace that recovers a revert's error.
///
/// Fixed, and far below the request budget: `debug_traceCall` costs a node markedly more than
/// `eth_simulateV1`, and the trace only names a revert the caller already knows about. Reusing the
/// request budget would let the diagnostic cost the caller more than the simulation did.
const SIMULATION_TRACE_TIMEOUT: Duration = Duration::from_millis(500);

/// One token's layout, or the reason this build cannot resolve one, resolved once per token.
type LayoutCell = Arc<OnceCell<Result<TokenLayout, String>>>;

/// The call that a simulation runs. All fields come from one quote.
#[derive(Clone, Copy)]
pub(crate) struct SimulatedCall<'a> {
    /// The account that sends the call.
    pub(crate) sender: Address,
    /// The router contract that the call goes to.
    pub(crate) router: Address,
    /// The amount of native token that the call sends.
    pub(crate) value: U256,
    /// The encoded router call.
    pub(crate) data: &'a [u8],
    /// The block whose state the call runs on.
    pub(crate) block: BlockNumberOrTag,
    /// How the simulated block sets `prevrandao`.
    pub(crate) prev_randao: PrevRandaoOverride,
}

/// How the simulated block sets `prevrandao`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrevRandaoOverride {
    /// A random value, so a pool cannot tell a simulation by a zero `prevrandao`.
    Random,
    /// No override: the node keeps its own value. BSC stores the millisecond part of the block
    /// time in this field and rejects values of 1000 or more.
    NodeValue,
}

impl PrevRandaoOverride {
    /// Returns the override for `chain`.
    pub(crate) fn for_chain(chain: Chain) -> Self {
        match chain {
            Chain::Bsc => Self::NodeValue,
            _ => Self::Random,
        }
    }
}

/// Why a quote is simulated.
///
/// The simulation metrics carry it as the `purpose` label, so that dashboards can show client
/// quotes apart from other simulations. Fynd simulates with [`Self::QUOTE`]. A caller that
/// simulates quotes for its own use gives its own label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationPurpose(&'static str);

impl SimulationPurpose {
    /// A client asked to simulate the quote.
    pub const QUOTE: Self = Self("quote");

    /// Returns a purpose with `label` as the value of the `purpose` label.
    ///
    /// The label is a `&'static str`, so that the number of label values stays fixed.
    pub const fn new(label: &'static str) -> Self {
        Self(label)
    }

    /// Returns the value of the `purpose` label.
    pub const fn label(self) -> &'static str {
        self.0
    }
}

/// The block whose state a simulation runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SimulationBlock {
    /// The latest block of the node.
    #[default]
    Latest,
    /// The block that the quote was priced on.
    ///
    /// The simulated output then differs from the quote only by what the quote did not model,
    /// and not by the blocks that came after the solve. A node that does not have the block yet
    /// is asked again, for at most two block times.
    Quote,
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
    /// The block that each simulation runs on.
    block: SimulationBlock,
    /// Time between two blocks of the chain. It sets the longest wait for a block that the node
    /// does not have yet.
    block_time: Duration,
    /// How the simulated block sets `prevrandao`.
    prev_randao: PrevRandaoOverride,
}

/// Wait before the first retry when the node does not have the quote's block. Each later wait is
/// twice as long.
const BLOCK_RETRY_FIRST_DELAY: Duration = Duration::from_millis(500);
/// Longest total wait for a block that the node does not have, in block times.
const BLOCK_RETRY_BLOCKS: u32 = 2;
/// Block time of a simulator that is built without a chain, or that runs on the latest block.
const DEFAULT_BLOCK_TIME: Duration = Duration::from_secs(12);

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

/// What simulating one quote takes from the quote itself, read before any RPC call.
struct SimulationSetup<'a> {
    call: SimulatedCall<'a>,
    /// The token the sender pays in, which decides how the sender is funded.
    token_in: Address,
    /// The address that must accept the quote's placeholder client fee signature, if any.
    client_fee_receiver: Option<Address>,
}

impl<'a> SimulationSetup<'a> {
    /// Reads the call, the input token and the client fee receiver off an encoded quote.
    ///
    /// The router skips the client fee signature check for a zero receiver, so a zero receiver
    /// needs no override and is reported as none.
    ///
    /// # Errors
    ///
    /// Returns why the quote cannot be simulated: it has no transaction or no route, an address is
    /// not 20 bytes, or the client fee receiver is the router. Code at the router would replace
    /// the router itself, and the call would return the magic value as its amount out.
    fn for_quote(
        quote: &'a OrderQuote,
        client_fee: Option<&ClientFeeParams>,
        block: SimulationBlock,
        prev_randao: PrevRandaoOverride,
    ) -> Result<Self, String> {
        let transaction = quote
            .transaction()
            .ok_or("quote has no encoded transaction")?;
        let first_swap = quote
            .route()
            .ok_or("quote has no route")?
            .swaps()
            .first()
            .ok_or("route has no swaps")?;
        let call = SimulatedCall {
            sender: crate::rpc::to_address(quote.sender(), "quote sender")?,
            router: crate::rpc::to_address(transaction.to(), "transaction destination")?,
            value: U256::from_be_slice(&transaction.value().to_bytes_be()),
            data: transaction.data(),
            block: match block {
                SimulationBlock::Latest => BlockNumberOrTag::Latest,
                SimulationBlock::Quote => BlockNumberOrTag::Number(quote.block().number()),
            },
            prev_randao,
        };
        let token_in = crate::rpc::to_address(first_swap.token_in(), "route token_in")?;
        let mut client_fee_receiver = None;
        if let Some(fee) = client_fee {
            let receiver = crate::rpc::to_address(fee.receiver(), "client fee receiver")?;
            if receiver == call.router {
                return Err(format!("client fee receiver {receiver} is the router"));
            }
            if receiver != Address::ZERO {
                client_fee_receiver = Some(receiver);
            }
        }
        Ok(Self { call, token_in, client_fee_receiver })
    }
}

pub(crate) enum SimulationAttempt {
    /// The simulated call completed.
    Success { amount_out: BigUint, gas_used: u64, logs: Vec<EventLog> },
    /// The simulated call reverted.
    Reverted { reason: String, replay: Box<Replay> },
    /// Simulation could not be completed.
    Failure { reason: String },
    /// The node did not have the quote's block, also after the retries.
    BlockUnavailable { reason: String },
}

impl QuoteSimulator {
    /// Creates a simulator that sends its requests to `rpc_url`.
    ///
    /// # Arguments
    ///
    /// * `rpc_url` - The URL of a node that supports `eth_simulateV1` and `debug_traceCall`.
    /// * `chain` - The chain of the node. It sets the native token, and the block time of
    ///   [`SimulationBlock::Quote`].
    /// * `request_timeout` - The time limit for the `eth_simulateV1` calls of one simulation,
    ///   retries included.
    /// * `block` - The block that each simulation runs on.
    ///
    /// # Errors
    ///
    /// Returns an error when `rpc_url` is not a valid URL, when the chain has no native token, or
    /// when `block` is [`SimulationBlock::Quote`] and the chain has no block time.
    pub fn new(
        rpc_url: &str,
        chain: Chain,
        request_timeout: std::time::Duration,
        block: SimulationBlock,
    ) -> Result<Self, String> {
        let url = rpc_url
            .parse()
            .map_err(|error| format!("invalid RPC URL {rpc_url:?}: {error}"))?;
        let native_token = chain
            .try_native_token()
            .map_err(|error| format!("native token for {chain:?}: {error}"))?;
        let simulator = Self::with_provider(
            ProviderBuilder::default().connect_http(url),
            Address::from_slice(native_token.address.as_ref()),
            request_timeout,
        )
        .with_block(block)
        .with_prev_randao(PrevRandaoOverride::for_chain(chain));
        if block == SimulationBlock::Latest {
            return Ok(simulator);
        }
        let block_time = chain
            .try_block_time_secs()
            .map_err(|error| format!("block time for {chain:?}: {error}"))?;
        Ok(simulator.with_block_time(Duration::from_secs(block_time)))
    }

    /// Simulates an encoded quote, and records the metrics of the result.
    ///
    /// The metrics carry `purpose` as a label. For a successful simulation, they also record how
    /// far the simulated output is from the quoted output, and the gas that the call used.
    ///
    /// # Arguments
    ///
    /// * `quote` - The encoded quote to simulate.
    /// * `client_fee` - The client fee params the quote was encoded with.
    /// * `purpose` - Why the quote is simulated.
    pub(crate) async fn simulate_attempt(
        &self,
        quote: &OrderQuote,
        client_fee: Option<&ClientFeeParams>,
        purpose: SimulationPurpose,
    ) -> SimulationAttempt {
        let attempt = self.attempt(quote, client_fee).await;
        record_outcome(quote, &attempt, purpose);
        attempt
    }

    async fn attempt(
        &self,
        quote: &OrderQuote,
        client_fee: Option<&ClientFeeParams>,
    ) -> SimulationAttempt {
        let setup =
            match SimulationSetup::for_quote(quote, client_fee, self.block, self.prev_randao) {
                Ok(value) => value,
                Err(error) => return failure_with(format!("simulation setup failed: {error}")),
            };
        let mut overrides = match self
            .overrides(setup.call.sender, setup.token_in, setup.call.router)
            .await
        {
            Ok(value) => value,
            Err(reason) => return failure_with(reason),
        };
        if let Some(receiver) = setup.client_fee_receiver {
            add_client_fee_receiver_override(&mut overrides, receiver);
        }
        self.simulate_within_timeout(setup.call, overrides, SimulationEnvelope::for_quote(quote))
            .await
    }

    /// Runs one simulated call on `call.block`, within the request timeout.
    ///
    /// When the node does not have that block yet, waits and tries again. The first wait is 500 ms,
    /// and each later wait is twice as long. The retries stop when the waits add up to two block
    /// times, or when the request timeout leaves no time for one more try.
    ///
    /// # Arguments
    ///
    /// * `call` - The call to simulate.
    /// * `overrides` - The state overrides that fund the sender.
    /// * `envelope` - The gas limit and the gas price of the call.
    ///
    /// # Returns
    ///
    /// The result of the call, or [`SimulationAttempt::BlockUnavailable`] when the node still does
    /// not have the block after the retries.
    pub(crate) async fn simulate_within_timeout(
        &self,
        call: SimulatedCall<'_>,
        overrides: StateOverride,
        envelope: SimulationEnvelope,
    ) -> SimulationAttempt {
        let deadline = Instant::now() + self.request_timeout;
        let budget = self.block_time * BLOCK_RETRY_BLOCKS;
        let mut waited = Duration::ZERO;
        let mut delay = BLOCK_RETRY_FIRST_DELAY;
        loop {
            let outcome = simulate_with_overrides(
                &self.provider,
                call,
                overrides.clone(),
                envelope,
                deadline.saturating_duration_since(Instant::now()),
            )
            .await;
            let CallOutcome::BlockUnavailable(reason) = outcome else {
                return outcome.into_attempt();
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let pause = delay
                .min(budget.saturating_sub(waited))
                .min(remaining);
            // A pause that leaves no time to ask again ends the wait.
            if pause.is_zero() || pause >= remaining {
                return SimulationAttempt::BlockUnavailable {
                    reason: format!("{reason} (waited {waited:?})"),
                };
            }
            debug!(
                target: SIMULATION_OUTCOME_TARGET,
                block = ?call.block,
                ?waited,
                ?pause,
                "node does not have the block yet; asking again"
            );
            sleep(pause).await;
            waited += pause;
            delay *= 2;
        }
    }

    /// Returns this simulator with `block` as the block that each simulation runs on.
    pub(crate) fn with_block(mut self, block: SimulationBlock) -> Self {
        self.block = block;
        self
    }

    /// Returns this simulator with `block_time` as the time between two blocks.
    ///
    /// The block time sets the longest wait for a block that the node does not have yet.
    pub(crate) fn with_block_time(mut self, block_time: Duration) -> Self {
        self.block_time = block_time;
        self
    }

    /// Returns this simulator with `prev_randao` as how each simulated block sets `prevrandao`.
    pub(crate) fn with_prev_randao(mut self, prev_randao: PrevRandaoOverride) -> Self {
        self.prev_randao = prev_randao;
        self
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
            block: SimulationBlock::Latest,
            block_time: DEFAULT_BLOCK_TIME,
            prev_randao: PrevRandaoOverride::Random,
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
            Self::Success { amount_out, gas_used, logs } => {
                SimulationResult::Success { amount_out, gas_used, logs }
            }
            Self::Reverted { reason, .. } => SimulationResult::Reverted { reason },
            Self::Failure { reason } => SimulationResult::Failure { reason },
            Self::BlockUnavailable { reason } => SimulationResult::BlockUnavailable { reason },
        }
    }
}

/// Target for the reason a simulated quote reverted or failed. Emitted at DEBUG, so a deployment
/// that wants the reasons sets `RUST_LOG=...,fynd::simulation_outcome=debug`.
const SIMULATION_OUTCOME_TARGET: &str = "fynd::simulation_outcome";

/// Target for everything needed to rerun a reverted simulation. Emitted at DEBUG and kept apart
/// from [`SIMULATION_OUTCOME_TARGET`] because the payload carries the full calldata, so a
/// deployment opts in per chain with `RUST_LOG=...,fynd::simulation_replay=debug`.
const SIMULATION_REPLAY_TARGET: &str = "fynd::simulation_replay";

/// Logs what it takes to rerun one reverted simulation exactly.
///
/// The payload is serialised only when the target is enabled, so a deployment that leaves it off
/// never renders the calldata.
fn log_replay(quote: &OrderQuote, replay: &Replay) {
    debug!(
        target: SIMULATION_REPLAY_TARGET,
        order_id = quote.order_id(),
        parent_block = replay.parent_block,
        payload = %replay.payload_json(),
        "simulation reverted; rerun with eth_simulateV1(payload, parent_block)"
    );
}

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
/// from what the quote promised, and the gas the quote estimated next to the gas the call used.
///
/// A revert and a failure are counted apart because they call for different work: a revert means
/// the route the solver priced does not execute, while a failure means the simulation itself did
/// not run, so it says nothing about the route. Both carry the winning pool and algorithm, so a
/// rise in either can be traced to the solver that produced the route. Every metric also carries
/// the `purpose` label.
fn record_outcome(quote: &OrderQuote, attempt: &SimulationAttempt, purpose: SimulationPurpose) {
    let pool = quote.worker_pool().to_string();
    let algorithm = quote.algorithm().to_string();
    let purpose = purpose.label();
    let outcome = match attempt {
        SimulationAttempt::Success { amount_out, gas_used, .. } => {
            if let Some(deviation) = deviation_bps(quote, amount_out) {
                histogram!(
                    "quote_simulation_deviation_bps",
                    "pool" => pool.clone(),
                    "algorithm" => algorithm.clone(),
                    "purpose" => purpose
                )
                .record(deviation);
            }
            record_gas(quote, *gas_used, &pool, &algorithm, purpose);
            "success"
        }
        SimulationAttempt::Reverted { reason, replay } => {
            log_replay(quote, replay);
            log_outcome(quote, "reverted", reason)
        }
        SimulationAttempt::Failure { reason } => log_outcome(quote, "failed", reason),
        SimulationAttempt::BlockUnavailable { reason } => {
            log_outcome(quote, "block_unavailable", reason)
        }
    };
    counter!(
        "quote_simulations_total",
        "outcome" => outcome,
        "pool" => pool,
        "algorithm" => algorithm,
        "purpose" => purpose
    )
    .increment(1);
}

/// Records the gas the quote estimated and the gas the simulated call used, as absolute values.
///
/// Both are recorded for the same successful simulations only, so the two series compare like
/// for like. A reverted or failed call has no gas figure to set against the estimate.
fn record_gas(
    quote: &OrderQuote,
    gas_used: u64,
    pool: &str,
    algorithm: &str,
    purpose: &'static str,
) {
    if let Some(gas_estimate) = quote.gas_estimate().to_f64() {
        histogram!(
            "quote_simulation_gas_estimate",
            "pool" => pool.to_string(),
            "algorithm" => algorithm.to_string(),
            "purpose" => purpose
        )
        .record(gas_estimate);
    }
    histogram!(
        "quote_simulation_gas_used",
        "pool" => pool.to_string(),
        "algorithm" => algorithm.to_string(),
        "purpose" => purpose
    )
    .record(gas_used as f64);
}

fn failure_with(reason: String) -> SimulationAttempt {
    SimulationAttempt::Failure { reason }
}

/// Returns the block fields that the simulation sets itself.
///
/// `eth_simulateV1` takes the other block fields from the block that it runs on. It leaves the
/// fee recipient and `prevrandao` at zero, and a pool can read these to detect a simulation. This
/// sets them, and the block gas limit, to realistic values. `prevrandao` is random, except on BSC,
/// where it is left to the node because BSC rejects an override of 1000 or more.
fn block_environment(prev_randao: PrevRandaoOverride) -> SimulationBlockEnvironment {
    let prev_randao = match prev_randao {
        PrevRandaoOverride::Random => Some(B256::from(rand::random::<[u8; 32]>())),
        PrevRandaoOverride::NodeValue => None,
    };
    SimulationBlockEnvironment {
        fee_recipient: Some(SIMULATION_COINBASE),
        prev_randao,
        gas_limit: Some(SIMULATION_BLOCK_GAS_LIMIT),
        ..Default::default()
    }
}

/// Returns `environment` with the block number and the timestamp that the simulated block used.
///
/// The trace of a revert must run in the same block environment as the simulation.
/// `eth_simulateV1` picks the number of its block itself, so the caller takes `number` and
/// `timestamp` from its response.
fn executed_in(
    environment: SimulationBlockEnvironment,
    number: u64,
    timestamp: u64,
) -> SimulationBlockEnvironment {
    SimulationBlockEnvironment {
        number: Some(U256::from(number)),
        time: Some(timestamp),
        ..environment
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
    // revert. A random `prevrandao` is drawn per call, so it is built once and reused rather
    // than regenerated.
    let environment = block_environment(simulated.prev_randao);
    let payload =
        SimulationRequest::single_call(call.clone(), overrides.clone(), environment.clone());
    let response = match timeout(
        request_timeout,
        provider.raw_request::<_, Vec<SimulatedBlock<Block>>>(
            "eth_simulateV1".into(),
            (&payload, BlockId::from(simulated.block)),
        ),
    )
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            let reason = format!("simulation eth_simulateV1 failed: {}", rpc_error_reason(&error));
            if is_block_unavailable(&error) {
                return CallOutcome::BlockUnavailable(reason);
            }
            return CallOutcome::Failure(reason);
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
        let number = block.inner.header.number;
        let executed = executed_in(environment, number, block.inner.header.timestamp);
        let replay = Box::new(Replay {
            parent_block: number.saturating_sub(1),
            payload: SimulationRequest::single_call(
                call.clone(),
                overrides.clone(),
                executed.clone(),
            ),
        });
        if let Some(decoded) = revert::decode_error(&result.return_data) {
            return CallOutcome::Reverted { reason: decoded, replay };
        }
        let message = result
            .error
            .as_ref()
            .map_or("execution reverted", |error| error.message.as_str())
            .to_string();
        // `eth_simulateV1` drops the revert payload, so the same call is replayed under the
        // tracer -- same overrides, same environment -- to read the error off the frame that
        // produced it.
        let traced = timeout(
            SIMULATION_TRACE_TIMEOUT,
            traced_revert_reason(provider, call, simulated.block, overrides, executed),
        )
        .await
        .ok()
        .flatten();
        return CallOutcome::Reverted { reason: traced.unwrap_or(message), replay };
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
        logs: event_logs(&result.logs),
    }
}

/// What it takes to rerun a reverted simulation exactly.
///
/// The payload carries the call, the funding overrides and the block environment, with the number,
/// timestamp and `prevrandao` the simulated block actually had. Sent to `eth_simulateV1` on top of
/// `parent_block`, it executes the same call against the same state. A route can revert on any of
/// those inputs, a signed RFQ quote on the timestamp alone, so none of them is left to a guess.
#[derive(Debug)]
pub(crate) struct Replay {
    parent_block: u64,
    payload: SimulationRequest,
}

impl Replay {
    fn payload_json(&self) -> String {
        serde_json::to_string(&self.payload)
            .unwrap_or_else(|error| format!("payload did not serialize: {error}"))
    }
}

/// Converts the logs that the node returned to `EventLog`s, in the order that the call emitted
/// them.
fn event_logs(logs: &[alloy::rpc::types::Log]) -> Vec<EventLog> {
    let mut events = Vec::with_capacity(logs.len());
    for log in logs {
        let mut topics = Vec::with_capacity(log.topics().len());
        for topic in log.topics() {
            topics.push(topic.to_vec().into());
        }
        events.push(EventLog {
            address: log.address().to_vec().into(),
            topics,
            data: log.data().data.to_vec().into(),
        });
    }
    events
}

/// What one simulated call came back with.
enum CallOutcome {
    Success {
        amount_out: BigUint,
        gas_used: u64,
        logs: Vec<EventLog>,
    },
    /// The call reverted, with the best reason available: the payload's own error, or the one the
    /// trace recovered, or the node's message.
    Reverted {
        reason: String,
        replay: Box<Replay>,
    },
    Failure(String),
    /// The node does not have the block the call is to run on.
    BlockUnavailable(String),
}

impl CallOutcome {
    /// Converts this outcome to the [`SimulationAttempt`] that a caller gets.
    fn into_attempt(self) -> SimulationAttempt {
        match self {
            Self::Success { amount_out, gas_used, logs } => {
                SimulationAttempt::Success { amount_out, gas_used, logs }
            }
            Self::Reverted { reason, replay } => SimulationAttempt::Reverted {
                reason: format!("simulation reverted: {reason}"),
                replay,
            },
            Self::Failure(reason) => SimulationAttempt::Failure { reason },
            Self::BlockUnavailable(reason) => SimulationAttempt::BlockUnavailable { reason },
        }
    }
}

/// Returns whether `error` says that the node does not have the requested block.
///
/// Matches the messages "header not found", "block not found" and "unknown block".
fn is_block_unavailable(
    error: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>,
) -> bool {
    let Some(response) = error.as_error_resp() else {
        return false;
    };
    let message = response.message.to_ascii_lowercase();
    ["header not found", "block not found", "unknown block"]
        .iter()
        .any(|phrase| message.contains(phrase))
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

/// Lets the router's client fee signature check pass for a quote the client has not signed yet.
///
/// The client signs `ClientFee` after quoting, over the amounts and swaps the quote returns, so
/// the calldata carries a placeholder: the caller's signature bytes, zero-padded to 65. The router
/// tries ECDSA first and falls back to ERC-1271 on the receiver, so code at the receiver that
/// accepts every signature lets the rest of the call run, whatever the placeholder holds. Only the
/// receiver's code changes, not its address, so the FeeCalculator resolves the same fee client,
/// and so the same rates, as for the signed transaction.
///
/// This sets only `code` on any override the receiver already has, so a receiver that is also the
/// sender keeps its funding. Such a sender also passes Permit2's signature check, which falls back
/// to ERC-1271 for an owner with code, so its permit signature goes unchecked in the simulation.
fn add_client_fee_receiver_override(overrides: &mut StateOverride, receiver: Address) {
    overrides
        .entry(receiver)
        .or_default()
        .code = Some(Bytes::from_static(&ACCEPT_ANY_ERC1271_SIGNATURE_CODE));
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
    let balance = B256::from(layout.balance_word(SIMULATION_FUNDING_VALUE));
    let mut state_diff = B256HashMap::default();
    for holder in [sender, router] {
        state_diff.insert(layout.balance_slot(holder), balance);
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
    block: BlockNumberOrTag,
    overrides: StateOverride,
    environment: SimulationBlockEnvironment,
) -> Option<String> {
    let options = TraceCallOptions {
        tracing_options: GethDebugTracingOptions::default()
            .with_tracer(GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::CallTracer))
            .with_call_config(CallConfig { only_top_call: Some(false), with_log: Some(false) }),
        state_overrides: overrides,
        block_overrides: environment,
    };

    match provider
        .raw_request::<_, CallFrame>(
            "debug_traceCall".into(),
            (call, BlockId::from(block), options),
        )
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
