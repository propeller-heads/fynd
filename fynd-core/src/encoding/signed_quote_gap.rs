//! Measures how far each RFQ leg's signed quote lands from the price levels it was solved on.
//!
//! The tycho-execution encoder asks an RFQ leg's state for a signed quote and keeps only the
//! calldata it builds from it. [`SignedQuoteRecorder`] wraps the state the encoder is handed: it
//! forwards everything to the real state, and when the encoder requests the signed quote it
//! compares the maker's answer with the amounts the leg was priced at. That covers every RFQ
//! protocol (Hashflow, Bebop, Liquorice, Native, Metric) without reading any protocol's calldata.
//!
//! - `rfq_signed_quote_deviation_bps{protocol}`: signed rate against the price-level rate, in basis
//!   points; negative means the maker signed for less than its levels advertised. Comparing rates
//!   keeps the figure right when a maker signs for a different input than the leg asked for.
//! - `rfq_signed_quote_refusals_total{protocol}`: requests the maker did not sign.
//! - A `fynd::rfq_signed_quote` debug line per signed leg, with the order, component, pair, and the
//!   level and signed amounts.

use std::{any::Any, collections::HashMap};

use async_trait::async_trait;
use metrics::{counter, histogram};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tracing::debug;
use tycho_simulation::tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::GetAmountOutParams, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
        },
    },
    Bytes,
};

use crate::Swap;

/// Log target for one line per signed leg, off unless enabled (`fynd::rfq_signed_quote=debug`).
const SIGNED_QUOTE_TARGET: &str = "fynd::rfq_signed_quote";

/// What one RFQ leg was priced at when the route was solved.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegLevels {
    order_id: String,
    protocol: String,
    component_id: String,
    token_in: Bytes,
    token_out: Bytes,
    amount_in: BigUint,
    amount_out: BigUint,
}

/// An RFQ leg's state that records the maker's signed quote against the leg's price levels.
///
/// Every [`ProtocolSim`] call goes to the wrapped state; only `request_signed_quote` is observed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SignedQuoteRecorder {
    inner: Box<dyn ProtocolSim>,
    levels: LegLevels,
}

/// The state to hand the encoder for `swap`: wrapped in a recorder when it is an RFQ leg, so its
/// signed quote is measured, and a plain copy otherwise.
pub(crate) fn encoder_state(order_id: &str, swap: &Swap) -> Box<dyn ProtocolSim> {
    let state = swap.protocol_state().clone_box();
    if state.as_indicatively_priced().is_err() {
        return state;
    }
    Box::new(SignedQuoteRecorder {
        inner: state,
        levels: LegLevels {
            order_id: order_id.to_string(),
            protocol: swap.protocol().to_string(),
            component_id: swap.component_id().to_string(),
            token_in: swap.token_in().clone(),
            token_out: swap.token_out().clone(),
            amount_in: swap.amount_in().clone(),
            amount_out: swap.amount_out().clone(),
        },
    })
}

impl SignedQuoteRecorder {
    fn record(&self, result: &Result<SignedQuote, SimulationError>) {
        let levels = &self.levels;
        let signed = match result {
            Ok(signed) => signed,
            Err(_) => {
                counter!("rfq_signed_quote_refusals_total", "protocol" => levels.protocol.clone())
                    .increment(1);
                return;
            }
        };
        let Some(deviation_bps) = rate_deviation_bps(levels, signed) else { return };
        histogram!("rfq_signed_quote_deviation_bps", "protocol" => levels.protocol.clone())
            .record(deviation_bps);
        debug!(
            target: SIGNED_QUOTE_TARGET,
            order_id = levels.order_id,
            protocol = levels.protocol,
            component_id = levels.component_id,
            token_in = %levels.token_in,
            token_out = %levels.token_out,
            level_amount_in = %levels.amount_in,
            level_amount_out = %levels.amount_out,
            signed_amount_in = %signed.amount_in,
            signed_amount_out = %signed.amount_out,
            deviation_bps,
            "signed RFQ quote against its price levels"
        );
    }
}

/// How far the signed rate lands from the price-level rate, in basis points. Negative means the
/// maker signed for less than its levels advertised.
fn rate_deviation_bps(levels: &LegLevels, signed: &SignedQuote) -> Option<f64> {
    let level_rate = levels.amount_out.to_f64()? / levels.amount_in.to_f64()?;
    let signed_rate = signed.amount_out.to_f64()? / signed.amount_in.to_f64()?;
    let deviation = (signed_rate / level_rate - 1.0) * 10_000.0;
    deviation
        .is_finite()
        .then_some(deviation)
}

#[async_trait]
impl IndicativelyPriced for SignedQuoteRecorder {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        let result = self
            .inner
            .as_indicatively_priced()?
            .request_signed_quote(params)
            .await;
        self.record(&result);
        result
    }
}

#[typetag::serde]
impl ProtocolSim for SignedQuoteRecorder {
    fn fee(&self) -> f64 {
        self.inner.fee()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.inner.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.inner
            .get_amount_out(amount_in, token_in, token_out)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.inner
            .get_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.inner
            .delta_transition(delta, tokens, balances)
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        self.inner.query_pool_swap(params)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    /// The wrapped state, so a downcast reaches the real protocol type.
    fn as_any(&self) -> &dyn Any {
        self.inner.as_any()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self.inner.as_any_mut()
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        self.inner.eq(other)
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }

    fn apply_block(&mut self, block: &BlockContext) -> bool {
        self.inner.apply_block(block)
    }
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::*;
    use crate::{
        algorithm::test_utils::{component, token, MockProtocolSim},
        tests::metrics::recorded_metrics,
    };

    /// An RFQ maker priced like `MockProtocolSim` that signs for `signs`, or refuses on `None`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct SigningSim {
        inner: MockProtocolSim,
        signs: Option<(u64, u64)>,
    }

    #[typetag::serde]
    impl ProtocolSim for SigningSim {
        fn fee(&self) -> f64 {
            self.inner.fee()
        }
        fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
            self.inner.spot_price(base, quote)
        }
        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            self.inner
                .get_amount_out(amount_in, token_in, token_out)
        }
        fn get_limits(
            &self,
            sell_token: Bytes,
            buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            self.inner
                .get_limits(sell_token, buy_token)
        }
        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("SigningSim holds a fixed state")
        }
        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn eq(&self, _other: &dyn ProtocolSim) -> bool {
            false
        }
        fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
            Ok(self)
        }
    }

    #[async_trait]
    impl IndicativelyPriced for SigningSim {
        async fn request_signed_quote(
            &self,
            params: GetAmountOutParams,
        ) -> Result<SignedQuote, SimulationError> {
            let (amount_in, amount_out) = self
                .signs
                .ok_or_else(|| SimulationError::FatalError("maker withdrew".to_string()))?;
            Ok(SignedQuote {
                base_token: params.token_in,
                quote_token: params.token_out,
                amount_in: BigUint::from(amount_in),
                amount_out: BigUint::from(amount_out),
                quote_attributes: HashMap::new(),
            })
        }
    }

    /// A 1_000 -> 2_000 leg on `protocol`, whose state is `state`.
    fn leg(protocol: &str, state: Box<dyn ProtocolSim>) -> Swap {
        let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
        Swap::new(
            "pool".to_string(),
            protocol.to_string(),
            a.address.clone(),
            b.address.clone(),
            BigUint::from(1_000u64),
            BigUint::from(2_000u64),
            BigUint::ZERO,
            component("pool", &[a, b]),
            state,
        )
    }

    fn rfq_leg(signs: Option<(u64, u64)>) -> Swap {
        leg("rfq:bebop", Box::new(SigningSim { inner: MockProtocolSim::new(2.0), signs }))
    }

    async fn request(state: &dyn ProtocolSim) -> Result<SignedQuote, SimulationError> {
        let params = GetAmountOutParams {
            amount_in: BigUint::from(1_000u64),
            token_in: Bytes::from(vec![0x0A; 20]),
            token_out: Bytes::from(vec![0x0B; 20]),
            sender: Bytes::from(vec![0x11; 20]),
            receiver: Bytes::from(vec![0x11; 20]),
        };
        state
            .as_indicatively_priced()?
            .request_signed_quote(params)
            .await
    }

    type Recorded = Vec<(String, Vec<String>, DebugValue)>;

    /// Requests a signed quote through the state the encoder would be handed, under a recorder.
    fn record(swap: &Swap) -> (Result<SignedQuote, SimulationError>, Recorded) {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let state = encoder_state("order", swap);
        let result = metrics::with_local_recorder(&recorder, || {
            futures::executor::block_on(request(state.as_ref()))
        });
        (result, recorded_metrics(&snapshotter))
    }

    fn deviation(recorded: &Recorded) -> Option<f64> {
        recorded
            .iter()
            .find(|(name, ..)| name == "rfq_signed_quote_deviation_bps")
            .and_then(|(.., value)| match value {
                DebugValue::Histogram(values) => values.first().map(|v| v.into_inner()),
                _ => None,
            })
    }

    #[test]
    fn test_encoder_state_records_signed_quote() {
        let (result, recorded) = record(&rfq_leg(Some((1_000, 1_980))));

        assert_eq!(
            result.unwrap().amount_out,
            BigUint::from(1_980u64),
            "the maker's answer passes through"
        );
        let gap = deviation(&recorded).expect("the signed quote is recorded");
        assert!((gap - -100.0).abs() < 1e-9, "1_980 signed against 2_000 levels: {gap}");
        assert!(
            recorded
                .iter()
                .any(|(name, labels, _)| name == "rfq_signed_quote_deviation_bps" &&
                    labels.contains(&"protocol=rfq:bebop".to_string())),
            "{recorded:?}"
        );
    }

    #[test]
    fn test_encoder_state_records_refusal() {
        let (result, recorded) = record(&rfq_leg(None));

        assert!(result.is_err(), "the refusal still reaches the encoder");
        assert!(deviation(&recorded).is_none());
        assert!(
            recorded
                .iter()
                .any(|(name, ..)| name == "rfq_signed_quote_refusals_total"),
            "{recorded:?}"
        );
    }

    #[test]
    fn test_encoder_state_leaves_amm_leg_unwrapped() {
        let swap = leg("uniswap_v2", Box::new(MockProtocolSim::new(2.0)));

        let state = encoder_state("order", &swap);

        assert!(state.as_indicatively_priced().is_err());
    }

    /// The encoder prices and inspects the wrapped state exactly as it would the real one.
    #[test]
    fn test_encoder_state_forwards_to_the_real_state() {
        let swap = rfq_leg(Some((1_000, 1_980)));
        let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));

        let state = encoder_state("order", &swap);

        let out = state
            .get_amount_out(BigUint::from(1_000u64), &a, &b)
            .unwrap()
            .amount;
        assert_eq!(out, BigUint::from(2_000u64));
        assert!(
            state
                .as_any()
                .downcast_ref::<SigningSim>()
                .is_some(),
            "a downcast reaches the real state"
        );
    }

    #[test]
    fn test_rate_deviation_bps() {
        let levels = LegLevels {
            order_id: String::new(),
            protocol: String::new(),
            component_id: String::new(),
            token_in: Bytes::default(),
            token_out: Bytes::default(),
            amount_in: BigUint::from(1_000u64),
            amount_out: BigUint::from(2_000u64),
        };
        let signed = |amount_in: u64, amount_out: u64| SignedQuote {
            base_token: Bytes::default(),
            quote_token: Bytes::default(),
            amount_in: BigUint::from(amount_in),
            amount_out: BigUint::from(amount_out),
            quote_attributes: HashMap::new(),
        };

        assert_eq!(rate_deviation_bps(&levels, &signed(1_000, 2_000)), Some(0.0));
        let partial = rate_deviation_bps(&levels, &signed(500, 990)).unwrap();
        assert!((partial - -100.0).abs() < 1e-9, "a smaller fill at a worse rate: {partial}");
        assert_eq!(rate_deviation_bps(&levels, &signed(0, 0)), None, "no rate to compare");
    }
}
