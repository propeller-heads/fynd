//! The live [`SteppingSolver`]: drives an in-process `fynd-core` solver, releasing the chain one
//! block at a time so a route quoted at one block state can be replayed against a later one.
//!
//! Shared by both measurement commands. `monitor` uses it for the two-state (N-1 → N) comparison
//! against settled trades; `decay` uses it to replay one quote across several following blocks.
//! The block barrier is deterministic: after releasing a block via
//! [`BlockStepController::trigger_next_block`], [`StepAdapter::advance`] waits until the solver's
//! `MarketData` reports a strictly newer applied block.

use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use fynd_core::{
    types::{
        EncodingOptions, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest, QuoteStatus,
    },
    BlockStepController, Solver,
};
use num_bigint::BigUint;
use tracing::warn;
use tycho_simulation::tycho_common::models::Address as CoreAddress;

use crate::resolve::{Outcome, SolvedAmount, SteppingSolver};

/// How often to warn while the solver has not applied the next block.
const STALL_WARN_INTERVAL: Duration = Duration::from_mins(5);
/// No block for this long means the feed is dead, not slow. The observed failure mode: one
/// server-side subscription goes silent, tycho-client's block synchronizer stops emitting while
/// it waits for it, and ~35 minutes later backpressure kills the remaining subscriptions
/// ("Buffer full, unsubscribing!"). Nothing resubscribes, so the stream never recovers — the
/// caller rebuilds the solver instead of waiting.
pub(crate) const FEED_DEAD_TIMEOUT: Duration = Duration::from_mins(15);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Slippage tolerance used when a quote is encoded. Only shapes the calldata's `minAmountOut`; it
/// does not affect routing or the quoted amounts.
const ENCODING_SLIPPAGE: f64 = 0.005;

/// Whether a solve also encodes its quote into an on-chain transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Encode: the quote carries its calldata and an encoding-refined gas estimate. Costs extra
    /// work per solve, and a failed encode turns an otherwise good route into
    /// [`Outcome::Unsolvable`].
    Requested,
    /// Skip encoding: routing and amounts are unaffected, so a measurement that reads only
    /// `amount_out` pays nothing for calldata it never uses and keeps routes an encoder would
    /// have rejected.
    Skipped,
}

/// Drives the in-process solver, stepping the chain one block per [`SteppingSolver::advance`].
pub(crate) struct StepAdapter<'a> {
    solver: &'a Solver,
    controller: &'a BlockStepController,
    timeout_ms: u64,
    encoding: Encoding,
}

impl<'a> StepAdapter<'a> {
    pub(crate) fn new(
        solver: &'a Solver,
        controller: &'a BlockStepController,
        timeout_ms: u64,
        encoding: Encoding,
    ) -> Self {
        Self { solver, controller, timeout_ms, encoding }
    }

    /// The solver being driven, for callers that read its market or derived data directly.
    pub(crate) fn solver(&self) -> &Solver {
        self.solver
    }

    /// The block-step controller, for callers that need to peek at the pending block.
    pub(crate) fn controller(&self) -> &BlockStepController {
        self.controller
    }
}

#[async_trait]
impl SteppingSolver for StepAdapter<'_> {
    async fn current_block(&self) -> Option<u64> {
        self.solver
            .market_data()
            .read()
            .await
            .last_updated()
            .map(fynd_core::BlockInfo::number)
    }

    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome {
        let Ok(amount) = amount_in.to_string().parse::<BigUint>() else {
            return Outcome::Unsolvable("unparseable amount_in".to_string());
        };
        // Placeholder receiver: routing/amounts are receiver-independent; it only fills the encoded
        // calldata's recipient.
        let order = Order::new(
            CoreAddress::from(token_in.into_array()),
            CoreAddress::from(token_out.into_array()),
            amount,
            OrderSide::Sell,
            CoreAddress::from([0x11u8; 20]),
        );
        let options = QuoteOptions::default().with_timeout_ms(self.timeout_ms);
        let options = match self.encoding {
            Encoding::Requested => {
                options.with_encoding_options(EncodingOptions::new(ENCODING_SLIPPAGE))
            }
            Encoding::Skipped => options,
        };

        let quote = match self
            .solver
            .quote(QuoteRequest::new(vec![order], options))
            .await
        {
            Ok(quote) => quote,
            Err(e) => return Outcome::Unsolvable(format!("solve error: {e}")),
        };
        let Some(order) = quote.orders().first() else {
            return Outcome::Unsolvable("solver returned no order quote".to_string());
        };
        order_quote_to_outcome(order)
    }

    async fn reexecute(&self, top: &SolvedAmount) -> Outcome {
        let Some(route) = top.solved_route.as_ref() else {
            return Outcome::Unsolvable("quote carried no route".to_string());
        };
        let market = self.solver.market_data();
        let view = market.read().await;
        match fynd_core::replay_route(route, view.base_market_state()) {
            Ok(replay) => {
                let amount_out = biguint_to_u256(&replay.amount_out);
                // Same route ⇒ same gas: reuse the original quote's gas deduction (in token_out
                // units) and its gas estimate instead of re-deriving gas prices at the new block
                // state.
                let gas_deduction = top
                    .amount_out
                    .saturating_sub(top.amount_out_net_gas);
                Outcome::Solved(SolvedAmount {
                    amount_out,
                    amount_out_net_gas: amount_out.saturating_sub(gas_deduction),
                    gas_estimate: top.gas_estimate,
                    // Same route re-executed: attribution carries over from the original quote. The
                    // route itself does not — nothing serializes a re-executed outcome's route
                    // (it only feeds the slippage numbers via its amounts).
                    algorithm: top.algorithm.clone(),
                    quote_json: top.quote_json.clone(),
                    solved_route: None,
                })
            }
            Err(e) => Outcome::Unsolvable(format!("re-execution failed: {e}")),
        }
    }

    async fn advance(&self) -> anyhow::Result<()> {
        let before = self.current_block().await;
        self.controller
            .trigger_next_block()
            .map_err(|_| anyhow::anyhow!("tycho stream ended (trigger channel closed)"))?;

        // Deterministic barrier: wait until the solver applies a block strictly newer than
        // `before`. An error here means the feed died — either its stream ended (peek returns
        // None once the gating task exits) or it jammed without ending (no block within
        // FEED_DEAD_TIMEOUT). The caller rebuilds the solver on any error.
        let stall_started = Instant::now();
        let mut next_warn = stall_started + STALL_WARN_INTERVAL;
        loop {
            if let Some(now) = self.current_block().await {
                if before.is_none_or(|b| now > b) {
                    return Ok(());
                }
            }
            if stall_started.elapsed() >= FEED_DEAD_TIMEOUT {
                anyhow::bail!(
                    "no block applied in {}s; tycho feed presumed dead",
                    stall_started.elapsed().as_secs()
                );
            }
            if Instant::now() >= next_warn {
                warn!(
                    waited_s = stall_started.elapsed().as_secs(),
                    last_applied_block = ?before,
                    "tycho stream stalled; waiting for the next block"
                );
                next_warn += STALL_WARN_INTERVAL;
            }
            tokio::select! {
                () = tokio::time::sleep(POLL_INTERVAL) => {}
                peeked = self.controller.peek_next_block() => {
                    if peeked.is_none() {
                        anyhow::bail!("tycho stream ended while waiting for the next block");
                    }
                }
            }
        }
    }
}

fn order_quote_to_outcome(quote: &OrderQuote) -> Outcome {
    if quote.status() != QuoteStatus::Success {
        return Outcome::Unsolvable(format!("{:?}", quote.status()));
    }
    // Project the quote to a slim route + calldata, built directly from the quote object. We must
    // NOT serialize the whole `OrderQuote`: it embeds each hop's `protocol_state`, which both
    // dominates size and fails to serialize for vm pools (e.g. Curve) — dropping the entire route
    // for exactly the deep-liquidity stable trades we care about.
    let quote_json = serde_json::to_string(&crate::resolve::jsonl::slim_quote(quote)).ok();
    Outcome::Solved(SolvedAmount {
        amount_out: biguint_to_u256(quote.amount_out()),
        amount_out_net_gas: biguint_to_u256(quote.amount_out_net_gas()),
        gas_estimate: biguint_to_u256(quote.gas_estimate()),
        // Which algorithm won the quote — the winning quote is the one the `WorkerPoolRouter`
        // ranked first across every configured pool, so this is the pool that beat the others on
        // this order. The readable path is derived from `solved_route` at serialization time
        // (see `resolve::render_route`), not stored here.
        algorithm: quote.algorithm().to_string(),
        quote_json,
        // Kept in memory so the route can be replayed at a later block state.
        solved_route: quote.route().cloned().map(Box::new),
    })
}

pub(crate) fn biguint_to_u256(value: &BigUint) -> U256 {
    // Convert via big-endian bytes: avoids a decimal string round-trip and catches overflow
    // without relying on parse. U256 fits in 32 bytes; a larger value is a solver bug.
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        warn!(bits = value.bits(), "solver quote amount overflows U256; treating as zero");
        return U256::ZERO;
    }
    let mut buf = [0u8; 32];
    buf[32 - bytes.len()..].copy_from_slice(&bytes);
    U256::from_be_bytes(buf)
}
