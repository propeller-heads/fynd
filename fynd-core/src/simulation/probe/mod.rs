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
#[cfg(test)]
mod fixtures;
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

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{Address, Bytes as EvmBytes, U256},
        providers::ProviderBuilder,
        rpc::types::simulate::{SimCallResult, SimulatedBlock},
        sol_types::SolError,
    };
    use serde_json::{json, Value};
    use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;
    use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;
    use crate::{
        algorithm::test_utils::{component_with_protocol, token, MockProtocolSim},
        simulation::{
            probe::fixtures::{derived, market, sizer, BLOCK},
            revert::SolidityErrors,
        },
    };

    /// What 500 USDC buy in the fixture market.
    const EXPECTED_OUT: u128 = 250_000_000_000_000_000;

    /// The fallback router fee: 1,000 of 100,000,000 fee units of the output.
    const ROUTER_FEE: u128 = EXPECTED_OUT / 100_000;

    /// Answers `eth_simulateV1` only when it runs on `block`, so that a probe on another block gets
    /// no answer and fails.
    struct SimulatesOn {
        block: String,
        answer: Result<Value, &'static str>,
    }

    impl Match for SimulatesOn {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return false };
            body["method"] == "eth_simulateV1" && body["params"][1] == self.block.as_str()
        }
    }

    impl Respond for SimulatesOn {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let id = serde_json::from_slice::<Value>(&request.body)
                .map(|body| body["id"].clone())
                .unwrap_or_default();
            let body = match &self.answer {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(message) => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}})
                }
            };
            ResponseTemplate::new(200).set_body_json(body)
        }
    }

    fn call_result(return_data: Vec<u8>, status: bool) -> Value {
        let block = SimulatedBlock::<alloy::rpc::types::Block> {
            inner: Default::default(),
            calls: vec![SimCallResult {
                return_data: EvmBytes::from(return_data),
                gas_used: 123_456,
                status,
                ..Default::default()
            }],
        };
        json!([block])
    }

    fn paid(amount: u128) -> Value {
        call_result(
            U256::from(amount)
                .to_be_bytes::<32>()
                .to_vec(),
            true,
        )
    }

    fn real_encoder() -> Encoder {
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(None)
            .unwrap();
        Encoder::new(Chain::Ethereum, registry).unwrap()
    }

    /// Probes the fixture market's swap against a node that answers `answer` when asked to
    /// simulate on `block`.
    async fn probe_answered(block: u64, answer: Result<Value, &'static str>) -> ProbeOutcome {
        let swap =
            ProbeSwap::plan(&market(), &derived(), &sizer(), super::fixtures::POOL, 0).unwrap();
        probe_swap_answered(swap, block, answer).await
    }

    async fn probe_swap_answered(
        swap: ProbeSwap,
        block: u64,
        answer: Result<Value, &'static str>,
    ) -> ProbeOutcome {
        let server = MockServer::start().await;
        Mock::given(SimulatesOn { block: format!("{block:#x}"), answer: answer.clone() })
            .respond_with(SimulatesOn { block: String::new(), answer })
            .mount(&server)
            .await;
        // The sender is funded natively when the probe's token is the chain's native one.
        let native = Address::repeat_byte(0x01);
        let simulator = QuoteSimulator::with_provider(
            ProviderBuilder::default().connect_http(server.uri().parse().unwrap()),
            native,
            Duration::from_secs(5),
        )
        .with_block(SimulationBlock::Quote)
        .with_block_time(Duration::from_millis(1));

        ComponentProbe::new(real_encoder(), simulator)
            .probe(swap)
            .await
    }

    fn deviation_of(outcome: ProbeOutcome) -> f64 {
        let ProbeOutcome::Executed { deviation_bps, .. } = outcome else {
            panic!("expected an executed probe, got {outcome:?}");
        };
        deviation_bps
    }

    #[tokio::test]
    async fn test_probe_of_a_healthy_component_reads_no_deviation() {
        let outcome = probe_answered(BLOCK, Ok(paid(EXPECTED_OUT - ROUTER_FEE))).await;

        let ProbeOutcome::Executed { amount_out, gas_used, deviation_bps } = outcome else {
            panic!("expected an executed probe, got {outcome:?}");
        };
        assert_eq!(amount_out, BigUint::from(EXPECTED_OUT - ROUTER_FEE));
        assert_eq!(gas_used, 123_456);
        assert!(deviation_bps.abs() < 0.01, "got {deviation_bps}");
    }

    #[tokio::test]
    async fn test_probe_reads_a_shortfall_as_negative_bps() {
        let paid_out = (EXPECTED_OUT - ROUTER_FEE) * 98 / 100;

        let deviation = deviation_of(probe_answered(BLOCK, Ok(paid(paid_out))).await);

        assert!((deviation + 200.0).abs() < 0.01, "got {deviation}");
    }

    #[tokio::test]
    async fn test_probe_reads_a_surplus_as_positive_bps() {
        let paid_out = (EXPECTED_OUT - ROUTER_FEE) * 101 / 100;

        let deviation = deviation_of(probe_answered(BLOCK, Ok(paid(paid_out))).await);

        assert!((deviation - 100.0).abs() < 0.01, "got {deviation}");
    }

    /// The node answers only for the block the state belongs to, so a probe that ran elsewhere
    /// would come back as a failure.
    #[tokio::test]
    async fn test_probe_runs_on_the_block_of_the_state() {
        let outcome = probe_answered(BLOCK + 1, Ok(paid(EXPECTED_OUT))).await;

        assert!(matches!(outcome, ProbeOutcome::SetupFailed { .. }), "got {outcome:?}");
    }

    #[tokio::test]
    async fn test_probe_revert_of_the_pool() {
        let revert =
            SolidityErrors::Error { reason: "ds-math-sub-underflow".to_string() }.abi_encode();

        let outcome = probe_answered(BLOCK, Ok(call_result(revert, false))).await;

        let ProbeOutcome::Reverted { reason, blame } = outcome else {
            panic!("expected a revert, got {outcome:?}");
        };
        assert!(reason.contains("ds-math-sub-underflow"), "got {reason}");
        assert_eq!(blame, RevertBlame::Pool);
    }

    #[tokio::test]
    async fn test_probe_revert_of_a_token() {
        // The selector of `TransferFromFailed()`.
        let revert = vec![0x79, 0x39, 0xf4, 0x24];

        let outcome = probe_answered(BLOCK, Ok(call_result(revert, false))).await;

        let ProbeOutcome::Reverted { reason, blame } = outcome else {
            panic!("expected a revert, got {outcome:?}");
        };
        assert!(reason.contains("TransferFromFailed"), "got {reason}");
        assert_eq!(blame, RevertBlame::Token);
    }

    #[tokio::test]
    async fn test_probe_when_the_node_lacks_the_block() {
        let outcome = probe_answered(BLOCK, Err("header not found")).await;

        assert_eq!(outcome, ProbeOutcome::BlockUnavailable);
    }

    #[tokio::test]
    async fn test_probe_when_the_node_fails() {
        let outcome = probe_answered(BLOCK, Err("internal error")).await;

        let ProbeOutcome::SetupFailed { reason } = outcome else {
            panic!("expected a setup failure, got {outcome:?}");
        };
        assert!(reason.contains("internal error"), "got {reason}");
    }

    #[tokio::test]
    async fn test_probe_that_does_not_encode() {
        // The swap encoder needs the pool id to be an address.
        let (usdc, weth) = (token(0x01, "USDC"), token(0x02, "WETH"));
        let mut market = market();
        market.upsert_components([component_with_protocol(
            "not-an-address",
            "uniswap_v2",
            &[usdc, weth],
        )]);
        market.update_states([(
            "not-an-address".to_string(),
            Box::new(MockProtocolSim::new(0.0005)) as Box<dyn ProtocolSim>,
        )]);
        let swap = ProbeSwap::plan(&market, &derived(), &sizer(), "not-an-address", 0).unwrap();

        let outcome = probe_swap_answered(swap, BLOCK, Ok(paid(EXPECTED_OUT))).await;

        assert!(matches!(outcome, ProbeOutcome::SetupFailed { .. }), "got {outcome:?}");
    }
}
