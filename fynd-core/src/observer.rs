//! Observer trait and event types for capturing solver decisions.
//!
//! The `SolverObserver` trait defines callbacks invoked on the hot path during
//! route scoring and quote production. Implementations can log, record, or
//! forward these events for downstream analysis (e.g. slippage feature
//! collection).
//!
//! `ObservedRoute` and `ObservedSwap` are owned, `Clone`-able snapshots of
//! the core [`Route`] / [`Swap`] types, which contain `Box<dyn ProtocolSim>`
//! and therefore cannot be cloned.

use std::collections::HashMap;

use tycho_simulation::tycho_core::Bytes;

use crate::types::quote::{Route, Swap};

/// Maximum number of blocks into the future we consider for features.
pub const MAX_BLOCK_OFFSET: u32 = 10;

// ---------------------------------------------------------------------------
// Observer trait
// ---------------------------------------------------------------------------

/// Callback interface for solver instrumentation.
///
/// Implementations must be thread-safe (`Send + Sync`) because they are
/// invoked from worker threads.
pub trait SolverObserver: Send + Sync {
    /// Called each time a candidate route is scored during selection.
    fn on_route_scored(&self, route: &ObservedRoute, score: f64, rank: usize);

    /// Called when a complete quote has been produced for a request.
    fn on_quote_produced(&self, event: QuoteProducedEvent);
}

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// Snapshot of a quote event emitted by a solver.
#[derive(Debug, Clone)]
pub struct QuoteProducedEvent {
    /// Opaque identifier grouping quotes from different solvers for the same
    /// upstream request.
    pub request_id: String,
    /// Unique identifier for this individual quote.
    pub quote_id: String,
    /// Identifier of the solver that produced this quote.
    pub solver_id: String,
    /// Whether this quote was selected as the winning response.
    pub is_winner: bool,
    /// Block number at which the quote was computed.
    pub block_number: u64,
    /// Chain identifier.
    pub chain_id: u64,
    /// The route chosen by the solver.
    pub route: ObservedRoute,
    /// Total input amount (BigInt string to prevent precision loss).
    pub amount_in: String,
    /// Total output amount (BigInt string to prevent precision loss).
    pub amount_out: String,
    /// Estimated gas for the route.
    pub gas_estimate: u64,
    /// ABI-encoded calldata for the transaction.
    pub calldata: Vec<u8>,
    /// Name of the algorithm that produced this route.
    pub algorithm_type: String,
    /// Algorithm-specific settings active at the time of quoting.
    pub algorithm_settings: HashMap<String, String>,
    /// Number of alternative routes that were evaluated.
    pub n_alternatives: u32,
    /// Basis-point gap between the best and second-best candidate.
    pub gap_to_second_best_bps: Option<f64>,
    /// Standard deviation of scores across all candidates.
    pub score_dispersion: Option<f64>,
    /// Slippage tolerance requested by the caller.
    pub slippage_tolerance: Option<f64>,
    /// Summary of every candidate route that was evaluated.
    pub all_candidates: Vec<CandidateSummary>,
}

/// Compact summary of a single candidate route.
#[derive(Debug, Clone)]
pub struct CandidateSummary {
    /// The candidate route.
    pub route: ObservedRoute,
    /// Score assigned to this candidate.
    pub score: f64,
    /// Output amount (BigInt string).
    pub amount_out: String,
}

// ---------------------------------------------------------------------------
// Observed route / swap (owned, cloneable mirrors of Route / Swap)
// ---------------------------------------------------------------------------

/// Cloneable snapshot of a [`Route`].
#[derive(Debug, Clone)]
pub struct ObservedRoute {
    /// Ordered swaps that make up this route.
    pub swaps: Vec<ObservedSwap>,
}

/// Cloneable snapshot of a single [`Swap`].
#[derive(Debug, Clone)]
pub struct ObservedSwap {
    /// Liquidity pool component identifier.
    pub component_id: String,
    /// Protocol system (e.g. "uniswap_v2").
    pub protocol: String,
    /// Input token address.
    pub token_in: Bytes,
    /// Output token address.
    pub token_out: Bytes,
    /// Input amount (BigInt string).
    pub amount_in: String,
    /// Output amount (BigInt string).
    pub amount_out: String,
    /// Estimated gas for this swap (BigInt string).
    pub gas_estimate: String,
    /// Fraction of a split route allocated to this swap (e.g. 0.5 = 50%).
    pub split: f64,
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

impl From<&Route> for ObservedRoute {
    fn from(route: &Route) -> Self {
        Self {
            swaps: route
                .swaps()
                .iter()
                .map(ObservedSwap::from)
                .collect(),
        }
    }
}

impl From<&Swap> for ObservedSwap {
    fn from(swap: &Swap) -> Self {
        Self {
            component_id: swap.component_id().to_owned(),
            protocol: swap.protocol().to_owned(),
            token_in: swap.token_in().clone(),
            token_out: swap.token_out().clone(),
            amount_in: swap.amount_in().to_string(),
            amount_out: swap.amount_out().to_string(),
            gas_estimate: swap.gas_estimate().to_string(),
            split: *swap.split(),
        }
    }
}

// ---------------------------------------------------------------------------
// Noop implementation
// ---------------------------------------------------------------------------

/// Observer that discards all events. Used as the default when observation is
/// disabled.
pub struct NoopObserver;

impl SolverObserver for NoopObserver {
    fn on_route_scored(&self, _route: &ObservedRoute, _score: f64, _rank: usize) {}
    fn on_quote_produced(&self, _event: QuoteProducedEvent) {}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::*;
    use crate::algorithm::test_utils::{component, token, MockProtocolSim};

    fn make_address(byte: u8) -> Bytes {
        Bytes::from([byte; 20].as_slice())
    }

    fn make_swap(token_in_byte: u8, token_out_byte: u8, amount_in: u64, amount_out: u64) -> Swap {
        let tin = token(token_in_byte, "TIN");
        let tout = token(token_out_byte, "TOUT");
        Swap::new(
            "pool-1".to_string(),
            "uniswap_v2".to_string(),
            make_address(token_in_byte),
            make_address(token_out_byte),
            BigUint::from(amount_in),
            BigUint::from(amount_out),
            BigUint::from(100_000u64),
            component("test-pool", &[tin, tout]),
            Box::new(MockProtocolSim::default()),
        )
    }

    fn make_route() -> Route {
        Route::new(vec![make_swap(0x01, 0x02, 1000, 990), make_swap(0x02, 0x03, 990, 980)])
    }

    #[test]
    fn noop_observer_does_not_panic() {
        let observer = NoopObserver;

        let route = ObservedRoute { swaps: vec![] };
        observer.on_route_scored(&route, 1.23, 0);

        let event = QuoteProducedEvent {
            request_id: "req-1".into(),
            quote_id: "q-1".into(),
            solver_id: "solver-a".into(),
            is_winner: true,
            block_number: 42,
            chain_id: 1,
            route,
            amount_in: "1000".into(),
            amount_out: "990".into(),
            gas_estimate: 100_000,
            calldata: vec![0xAB],
            algorithm_type: "most_liquid".into(),
            algorithm_settings: HashMap::new(),
            n_alternatives: 3,
            gap_to_second_best_bps: Some(10.0),
            score_dispersion: Some(0.5),
            slippage_tolerance: Some(0.005),
            all_candidates: vec![],
        };
        observer.on_quote_produced(event);
    }

    #[test]
    fn observed_route_from_route_preserves_fields() {
        let route = make_route();
        let observed = ObservedRoute::from(&route);

        assert_eq!(observed.swaps.len(), 2);

        let first = &observed.swaps[0];
        assert_eq!(first.component_id, "pool-1");
        assert_eq!(first.protocol, "uniswap_v2");
        assert_eq!(first.token_in, make_address(0x01));
        assert_eq!(first.token_out, make_address(0x02));
        assert_eq!(first.amount_in, "1000");
        assert_eq!(first.amount_out, "990");
        assert_eq!(first.gas_estimate, "100000");
        assert!((first.split - 0.0).abs() < f64::EPSILON);

        let second = &observed.swaps[1];
        assert_eq!(second.token_in, make_address(0x02));
        assert_eq!(second.token_out, make_address(0x03));
        assert_eq!(second.amount_in, "990");
        assert_eq!(second.amount_out, "980");
    }

    #[test]
    fn observed_route_from_route_with_split() {
        let swap = make_swap(0x01, 0x02, 500, 495).with_split(0.5);
        let route = Route::new(vec![swap]);
        let observed = ObservedRoute::from(&route);

        assert_eq!(observed.swaps.len(), 1);
        assert!((observed.swaps[0].split - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn observed_route_from_empty_route() {
        let route = Route::new(vec![]);
        let observed = ObservedRoute::from(&route);

        assert!(observed.swaps.is_empty());
    }

    #[test]
    fn candidate_summary_is_cloneable() {
        let summary = CandidateSummary {
            route: ObservedRoute { swaps: vec![] },
            score: 42.0,
            amount_out: "12345".into(),
        };
        let cloned = summary.clone();
        assert!((cloned.score - 42.0).abs() < f64::EPSILON);
        assert_eq!(cloned.amount_out, "12345");
    }
}
