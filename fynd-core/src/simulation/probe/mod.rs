//! Probes: one swap through one component, encoded and simulated on chain.
//!
//! A monitor plans a [`ProbeSwap`] from the market, sized by a [`ProbeSizer`], and sends it
//! through a [`ComponentProbe`]. The outcome says whether the swap executes, why it reverts, and
//! how far the on-chain output is from the output the component's state predicted.
//!
//! ```ignore
//! let sizer = ProbeSizer::new(500.0, 0.01)?;
//! let probe = ComponentProbe::connect(chain, rpc_url, Duration::from_secs(3)).await?;
//! let swap = {
//!     let view = market.read().await;
//!     let derived = derived.read().await;
//!     ProbeSwap::plan(view.base_market_state(), &derived, &sizer, component_id, 0)?
//! };
//! match probe.probe(swap).await {
//!     ProbeOutcome::Executed { deviation_bps, .. } => { /* healthy unless far below zero */ }
//!     ProbeOutcome::Reverted { blame: RevertBlame::Pool, .. } => { /* count towards broken */ }
//!     ProbeOutcome::Reverted { blame: RevertBlame::Token, .. } => { /* record only */ }
//!     ProbeOutcome::SetupFailed { .. } | ProbeOutcome::BlockUnavailable => { /* no verdict */ }
//! }
//! ```

mod blame;
mod sizing;
mod swap;

use std::time::Duration;

pub use blame::RevertBlame;
use num_bigint::BigUint;
pub use sizing::{ProbeSizer, SizingError};
pub use swap::{ProbePlanError, ProbeSwap};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
use tycho_simulation::tycho_common::{models::Chain, Bytes};

use crate::{
    encoding::{encoder::Encoder, fee_fetcher::RouterFeeFetcher},
    simulation::{
        deviation::deviation_bps,
        simulator::{QuoteSimulator, SimulationAttempt, SimulationBlock, SimulationPurpose},
    },
    solver::defaults::ROUTER_FEE_REFRESH_INTERVAL,
    EncodingOptions, OrderQuote, QuoteStatus,
};

/// The slippage a probe is encoded with: so loose that the router's minimum output check never
/// turns an inaccurate component into a revert.
const PROBE_SLIPPAGE: f64 = 0.5;

/// The account that sends a probe. The simulation funds it, so it holds nothing on chain.
const PROBE_SENDER: [u8; 20] = [
    0x9d, 0xd4, 0xce, 0x3b, 0x9b, 0xfa, 0x1c, 0x1f, 0x5d, 0x0e, 0x7b, 0x3c, 0x6a, 0x2f, 0x4e, 0x8d,
    0x1b, 0x5a, 0x7c, 0x30,
];

/// The `purpose` label of the simulation metrics of probes.
const PROBE_PURPOSE: SimulationPurpose = SimulationPurpose::new("component_probe");

/// What a probe swap came to.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeOutcome {
    /// The swap executed on chain.
    Executed {
        /// What the router paid out, in raw units of the token bought, after its fees.
        amount_out: BigUint,
        /// The gas the call used.
        gas_used: u64,
        /// How far `amount_out` is from the output the component's state predicted, less the
        /// router fee, in basis points: `(simulated - expected) / expected`. About zero for a
        /// component that prices correctly, negative when it paid less than predicted. A positive
        /// value means a surplus.
        deviation_bps: f64,
    },
    /// The swap reverted on chain.
    Reverted {
        /// The decoded revert, such as the name of the contract error.
        reason: String,
        /// Whom the revert points at.
        blame: RevertBlame,
    },
    /// The probe could not run, so it says nothing about the component: the swap did not encode,
    /// the simulation could not fund the sender or find the token's storage layout, the node
    /// failed or timed out, or there was no output to compare against.
    SetupFailed {
        /// What went wrong.
        reason: String,
    },
    /// The node did not have the block of the swap's state, also after waiting for it.
    BlockUnavailable,
}

/// Why a [`ComponentProbe`] could not be connected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeConnectError {
    /// The swap encoder could not be created.
    #[error("cannot create the swap encoder: {0}")]
    Encoder(String),
    /// The chain has no Tycho router, so nothing can be encoded.
    #[error("chain {0} has no Tycho router deployment, so probes cannot be encoded")]
    NoRouter(Chain),
    /// The quote simulator could not be created.
    #[error("cannot create the quote simulator: {0}")]
    Simulator(String),
    /// The router fee reader could not be created.
    #[error("cannot create the router fee reader: {0}")]
    RouterFees(String),
}

/// Encodes one swap through the Tycho router and simulates it on chain.
///
/// The simulator should run on the block of the swap's state, so that the output is compared with
/// the state it was predicted from: build it with [`SimulationBlock::Quote`], as
/// [`connect`](Self::connect) does. The node must support `eth_simulateV1`, and
/// `debug_traceCall` for the revert reasons.
///
/// The simulation metrics of probes carry the `purpose` label `component_probe`.
pub struct ComponentProbe {
    encoder: Encoder,
    simulator: QuoteSimulator,
    /// Keeps the router fees current. Stopped when the probe is dropped.
    router_fee_refresh: Option<JoinHandle<()>>,
}

impl ComponentProbe {
    /// A probe that encodes with `encoder` and simulates with `simulator`.
    ///
    /// The deviation of an executed probe is measured against the router fee in `encoder`, so
    /// the caller keeps those fees current, for instance with a `RouterFeeFetcher`.
    pub fn new(encoder: Encoder, simulator: QuoteSimulator) -> Self {
        Self { encoder, simulator, router_fee_refresh: None }
    }

    /// Connects a probe to the node at `rpc_url`.
    ///
    /// Creates the default encoder of `chain`, a simulator on the block of each swap's state, and
    /// reads the router fees from chain once before returning, then keeps them current in the
    /// background. A failed first read is logged and the encoder uses its fallback fee until a
    /// later read succeeds. `request_timeout` limits the simulation calls of one probe.
    ///
    /// Call it inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// A [`ProbeConnectError`] when the encoder, the simulator or the fee reader cannot be created,
    /// or the chain has no Tycho router.
    pub async fn connect(
        chain: Chain,
        rpc_url: &str,
        request_timeout: Duration,
    ) -> Result<Self, ProbeConnectError> {
        let registry = SwapEncoderRegistry::new(chain)
            .add_default_encoders(None)
            .map_err(|error| ProbeConnectError::Encoder(error.to_string()))?;
        let encoder = Encoder::new(chain, registry)
            .map_err(|error| ProbeConnectError::Encoder(error.to_string()))?;
        let router = encoder
            .router_address()
            .cloned()
            .ok_or(ProbeConnectError::NoRouter(chain))?;
        let simulator =
            QuoteSimulator::new(rpc_url, chain, request_timeout, SimulationBlock::Quote)
                .map_err(ProbeConnectError::Simulator)?;
        let fetcher = RouterFeeFetcher::new(
            rpc_url,
            &router,
            encoder.router_fees(),
            ROUTER_FEE_REFRESH_INTERVAL,
        )
        .map_err(|error| ProbeConnectError::RouterFees(error.to_string()))?;
        if let Err(error) = fetcher.refresh().await {
            warn!(%error, "cannot read the router fees yet; probes use the fallback fee");
        }
        let router_fee_refresh = tokio::spawn(async move { fetcher.run().await });
        Ok(Self { encoder, simulator, router_fee_refresh: Some(router_fee_refresh) })
    }

    /// Encodes `swap` and simulates it on the block of its state.
    pub async fn probe(&self, swap: ProbeSwap) -> ProbeOutcome {
        let quote = probe_quote(&swap);
        let encoded = match self
            .encoder
            .encode(vec![quote], EncodingOptions::new(PROBE_SLIPPAGE))
            .await
        {
            Ok(mut quotes) => quotes.pop(),
            Err(error) => {
                return ProbeOutcome::SetupFailed { reason: format!("encoding failed: {error}") }
            }
        };
        let Some(quote) = encoded.filter(|quote| {
            quote.status() == QuoteStatus::Success && quote.transaction().is_some()
        }) else {
            return ProbeOutcome::SetupFailed { reason: "the swap did not encode".to_string() };
        };
        let attempt = self
            .simulator
            .simulate_attempt(&quote, None, PROBE_PURPOSE)
            .await;
        debug!(
            component = swap.component_id(),
            protocol = swap.protocol_system(),
            block = swap.block().number(),
            "probed a component"
        );
        outcome_of(&quote, attempt)
    }
}

impl Drop for ComponentProbe {
    fn drop(&mut self) {
        if let Some(handle) = self.router_fee_refresh.take() {
            handle.abort();
        }
    }
}

/// The quote that holds `swap` as its only route, at the block of the swap's state.
fn probe_quote(swap: &ProbeSwap) -> OrderQuote {
    let sender = Bytes::from(PROBE_SENDER.as_slice());
    let quote = OrderQuote::new(
        format!("probe-{}", swap.component_id()),
        QuoteStatus::Success,
        swap.amount_in().clone(),
        swap.expected_out().clone(),
        swap.swap.gas_estimate().clone(),
        swap.expected_out().clone(),
        swap.block.clone(),
        "component_probe".to_string(),
        sender.clone(),
        sender,
        swap.state_label.clone(),
    )
    .with_route(swap.route());
    match &swap.gas_price {
        Some(gas_price) => quote.with_gas_price(gas_price.clone()),
        None => quote,
    }
}

/// Maps a simulation of the encoded probe `quote` to what the probe came to.
fn outcome_of(quote: &OrderQuote, attempt: SimulationAttempt) -> ProbeOutcome {
    match attempt {
        SimulationAttempt::Success { amount_out, gas_used, .. } => {
            match deviation_bps(quote, &amount_out) {
                Some(deviation_bps) => {
                    ProbeOutcome::Executed { amount_out, gas_used, deviation_bps }
                }
                None => ProbeOutcome::SetupFailed {
                    reason: "no expected output to compare the simulated one with".to_string(),
                },
            }
        }
        SimulationAttempt::Reverted { reason, .. } => {
            let blame = RevertBlame::of_reason(&reason);
            ProbeOutcome::Reverted { reason, blame }
        }
        SimulationAttempt::Failure { reason } => ProbeOutcome::SetupFailed { reason },
        SimulationAttempt::BlockUnavailable { .. } => ProbeOutcome::BlockUnavailable,
    }
}
