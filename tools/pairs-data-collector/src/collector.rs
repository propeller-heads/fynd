//! Embedded Fynd collection loop and block-consistency contract.

use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use fynd_core::{
    feed::market_data::MarketData, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest,
    QuoteStatus, Route, Solver,
};
use num_bigint::BigUint;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tycho_simulation::tycho_common::models::Address;

use crate::{
    config::CollectorConfig,
    head_ledger::{now_ms, ObservedHead},
    record::{BlockRun, PointStatus, QuotePoint, QuoteRole, SCHEMA_VERSION},
    sampler::{plan_forward_points, plan_matched_reverse, PlannedPoint},
};

/// Immutable metadata copied into every output row.
#[derive(Debug, Clone)]
pub struct RuntimeMetadata {
    /// Collector run UUID.
    pub run_id: String,
    /// Fixed-grid epoch ID.
    pub grid_epoch_id: String,
    /// Full configuration digest.
    pub config_hash: String,
    /// Protocol set digest.
    pub protocol_set_hash: String,
    /// Worker configuration digest.
    pub worker_config_hash: String,
    /// Fynd package version.
    pub fynd_version: String,
    /// Source revision, if supplied at build time.
    pub fynd_git_sha: String,
}

impl RuntimeMetadata {
    /// Derive reproducibility fields from the resolved configuration.
    pub fn from_config(
        config: &CollectorConfig,
        run_id: String,
        grid_epoch_id: String,
    ) -> Result<Self> {
        let resolved = toml::to_string(config)?;
        let worker = toml::to_string(&config.fynd)?;
        Ok(Self {
            run_id,
            grid_epoch_id,
            config_hash: digest(&resolved),
            protocol_set_hash: digest(&config.fynd.protocols.join(",")),
            worker_config_hash: digest(&worker),
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
            fynd_git_sha: option_env!("GIT_SHA")
                .unwrap_or("unknown")
                .to_string(),
        })
    }
}

/// Result of collecting all configured rows for one head.
pub struct HeadCollection {
    /// Source, reverse, and explicit skipped rows.
    pub points: Vec<QuotePoint>,
    /// Block completeness summary.
    pub block_run: BlockRun,
}

/// Embedded collector with one Fynd solver and a fixed run configuration.
pub struct Collector {
    solver: Solver,
    market: MarketData,
    config: CollectorConfig,
    metadata: RuntimeMetadata,
    sender: Address,
}

impl Collector {
    /// Create a collector around an already-started solver.
    pub fn new(solver: Solver, config: CollectorConfig, metadata: RuntimeMetadata) -> Result<Self> {
        let sender = Address::from_str(&config.collection.sender)
            .context("parsing configured quote sender")?;
        let market = solver.market_data();
        Ok(Self { solver, market, config, metadata, sender })
    }

    /// Wait for Fynd to expose exactly the RPC-observed head.
    pub async fn wait_for_head(&self, head: &ObservedHead) -> StateWaitOutcome {
        let timeout = Duration::from_millis(
            self.config
                .collection
                .state_wait_timeout_ms,
        );
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match market_identity(&self.market).await {
                Some(identity) if identity.matches(head) => {
                    return StateWaitOutcome::Ready { ready_at_ms: now_ms() };
                }
                Some(identity) if identity.number > head.number => {
                    return StateWaitOutcome::Missed {
                        reason: format!(
                            "Fynd advanced to {} {} before target {} {} was sampled",
                            identity.number, identity.hash, head.number, head.hash
                        ),
                    };
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return StateWaitOutcome::Timeout {
                        reason: format!(
                            "Fynd did not reach {} {} within {timeout:?}",
                            head.number, head.hash
                        ),
                    };
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }

    /// Collect all configured source and matched reverse points for one ready head.
    pub async fn collect_head(&self, head: &ObservedHead, fynd_ready_at_ms: u64) -> HeadCollection {
        let started_at_ms = now_ms();
        let deadline = tokio::time::Instant::now() +
            Duration::from_millis(
                self.config
                    .collection
                    .collection_budget_ms,
            );
        let forwards = plan_forward_points(&self.config, &self.metadata.run_id, &head.hash);
        let mut rows = Vec::with_capacity(self.config.expected_rows_per_block());
        let mut successful_forwards = Vec::new();
        for chunk in forwards.chunks(
            self.config
                .collection
                .request_chunk_size,
        ) {
            if tokio::time::Instant::now() >= deadline {
                for point in chunk {
                    let row = self.failure_row(
                        head,
                        point,
                        PointStatus::CapacitySkipped,
                        "per-head collection budget exhausted before source wave",
                    );
                    rows.push(self.skipped_reverse(head, point, &row));
                    rows.push(row);
                }
                continue;
            }
            let wave = self.collect_wave(head, chunk).await;
            for (point, row) in chunk.iter().zip(wave) {
                if row.status == PointStatus::Success {
                    if let Some(amount_out) = row.amount_out.clone() {
                        successful_forwards.push(plan_matched_reverse(
                            point,
                            &self.metadata.run_id,
                            &head.hash,
                            amount_out,
                        ));
                    }
                } else {
                    rows.push(self.skipped_reverse(head, point, &row));
                }
                rows.push(row);
            }
        }
        for chunk in successful_forwards.chunks(
            self.config
                .collection
                .request_chunk_size,
        ) {
            if tokio::time::Instant::now() >= deadline {
                rows.extend(self.failure_rows(
                    head,
                    chunk,
                    PointStatus::CapacitySkipped,
                    "per-head collection budget exhausted before reverse wave",
                ));
            } else {
                rows.extend(self.collect_wave(head, chunk).await);
            }
        }
        rows.sort_by(|left, right| left.point_id.cmp(&right.point_id));
        let successful_rows = rows
            .iter()
            .filter(|row| row.status == PointStatus::Success)
            .count();
        let market_negative_rows = count_market_negative(&rows);
        let block_run = self.block_run(
            head,
            BlockRunInput {
                fynd_ready_at_ms,
                collection_started_at_ms: started_at_ms,
                collection_finished_at_ms: now_ms(),
                scheduled_rows: rows.len(),
                successful_rows,
                market_negative_rows,
            },
        );
        HeadCollection { points: rows, block_run }
    }

    /// Produce explicit missed-state rows without asking Fynd for unavailable history.
    pub fn missed_head(&self, head: &ObservedHead, reason: &str) -> HeadCollection {
        let started_at_ms = now_ms();
        let forwards = plan_forward_points(&self.config, &self.metadata.run_id, &head.hash);
        let mut rows = Vec::with_capacity(self.config.expected_rows_per_block());
        for point in forwards {
            let source = self.failure_row(head, &point, PointStatus::MissedState, reason);
            rows.push(self.skipped_reverse(head, &point, &source));
            rows.push(source);
        }
        let block_run = self.block_run(
            head,
            BlockRunInput {
                fynd_ready_at_ms: 0,
                collection_started_at_ms: started_at_ms,
                collection_finished_at_ms: now_ms(),
                scheduled_rows: rows.len(),
                successful_rows: 0,
                market_negative_rows: 0,
            },
        );
        HeadCollection { points: rows, block_run }
    }

    /// Stop worker pools and background Fynd tasks.
    pub fn shutdown(self) {
        self.solver.shutdown();
    }

    async fn collect_wave(&self, head: &ObservedHead, points: &[PlannedPoint]) -> Vec<QuotePoint> {
        let started_at_ms = now_ms();
        let started = tokio::time::Instant::now();
        let Some(before) = market_identity(&self.market).await else {
            return self.failure_rows(head, points, PointStatus::NotReady, "no Fynd block state");
        };
        if !before.matches(head) {
            return self.failure_rows(
                head,
                points,
                PointStatus::BlockRace,
                "pre-wave state mismatch",
            );
        }
        let request = match self.quote_request(head, points) {
            Ok(request) => request,
            Err(error) => {
                return self.failure_rows(
                    head,
                    points,
                    PointStatus::RequestFailed,
                    &error.to_string(),
                );
            }
        };
        let result = self.solver.quote(request).await;
        let finished_at_ms = now_ms();
        let duration_ms = started.elapsed().as_millis() as u64;
        let after = market_identity(&self.market).await;
        if !wave_state_matches(head, &before, after.as_ref()) {
            return self.failure_rows(
                head,
                points,
                PointStatus::BlockRace,
                "post-wave state mismatch",
            );
        }
        match result {
            // Route replay inside row construction is CPU-bound; block_in_place keeps it
            // from starving the runtime that also drives Fynd's own state updates.
            Ok(quote) => tokio::task::block_in_place(|| {
                self.rows_from_quote(
                    head,
                    points,
                    &quote,
                    WaveTiming { started_at_ms, finished_at_ms, duration_ms },
                )
            }),
            Err(error) => self.failure_rows(
                head,
                points,
                PointStatus::RequestFailed,
                &format!("Fynd quote request failed: {error}"),
            ),
        }
    }

    fn quote_request(&self, head: &ObservedHead, points: &[PlannedPoint]) -> Result<QuoteRequest> {
        let mut orders = Vec::with_capacity(points.len());
        for point in points {
            let token_in = Address::from_str(&point.token_in.address)?;
            let token_out = Address::from_str(&point.token_out.address)?;
            let amount = BigUint::from_str(&point.amount_in)?;
            orders.push(
                Order::new(token_in, token_out, amount, OrderSide::Sell, self.sender.clone())
                    .with_id(point.point_id.clone()),
            );
        }
        let options = QuoteOptions::default()
            .with_timeout_ms(self.config.collection.quote_timeout_ms)
            .with_min_responses(0)
            .with_state_label(head.number.to_string());
        Ok(QuoteRequest::new(orders, options))
    }

    fn rows_from_quote(
        &self,
        head: &ObservedHead,
        points: &[PlannedPoint],
        quote: &fynd_core::Quote,
        timing: WaveTiming,
    ) -> Vec<QuotePoint> {
        let quotes: HashMap<&str, &OrderQuote> = quote
            .orders()
            .iter()
            .map(|order| (order.order_id(), order))
            .collect();
        points
            .iter()
            .map(|point| match quotes.get(point.point_id.as_str()) {
                Some(order_quote) if quote_matches_head(order_quote, head) => self
                    .success_or_status_row(head, point, order_quote, quote.solve_time_ms(), timing),
                Some(_) => self.failure_row(
                    head,
                    point,
                    PointStatus::BlockRace,
                    "quote block or state label does not match target head",
                ),
                None => self.failure_row(
                    head,
                    point,
                    PointStatus::RequestFailed,
                    "Fynd response omitted order",
                ),
            })
            .collect()
    }

    fn success_or_status_row(
        &self,
        head: &ObservedHead,
        point: &PlannedPoint,
        quote: &OrderQuote,
        batch_solve_time_ms: u64,
        timing: WaveTiming,
    ) -> QuotePoint {
        let status = point_status(quote.status());
        let is_success = status == PointStatus::Success;
        let mut row = self.base_row(head, point, status);
        row.quote_started_at_ms = timing.started_at_ms;
        row.quote_finished_at_ms = timing.finished_at_ms;
        row.monotonic_duration_ms = timing.duration_ms;
        row.batch_solve_time_ms = Some(batch_solve_time_ms);
        if is_success {
            row.amount_out = Some(quote.amount_out().to_string());
            row.amount_out_net_gas = Some(quote.amount_out_net_gas().to_string());
            row.gas_estimate = Some(quote.gas_estimate().to_string());
            row.gas_price = quote
                .gas_price()
                .map(ToString::to_string);
            row.price_impact_bps = quote.price_impact_bps();
            match quote.route() {
                Some(route) => match serialize_route(route) {
                    Ok(route_json) => {
                        row.route_json = Some(route_json);
                        if let Err(error) = replay_route(route) {
                            row.status = PointStatus::RequestFailed;
                            row.failure_reason = Some(format!("replaying Fynd route: {error}"));
                        }
                    }
                    Err(error) => {
                        row.status = PointStatus::RequestFailed;
                        row.failure_reason =
                            Some(format!("serializing successful Fynd route: {error}"));
                    }
                },
                None => {
                    row.status = PointStatus::RequestFailed;
                    row.failure_reason = Some("successful Fynd quote omitted its route".into());
                }
            }
        } else {
            row.failure_reason = Some(format!("Fynd returned {:?}", quote.status()));
        }
        row
    }

    fn failure_rows(
        &self,
        head: &ObservedHead,
        points: &[PlannedPoint],
        status: PointStatus,
        reason: &str,
    ) -> Vec<QuotePoint> {
        points
            .iter()
            .map(|point| self.failure_row(head, point, status.clone(), reason))
            .collect()
    }

    fn failure_row(
        &self,
        head: &ObservedHead,
        point: &PlannedPoint,
        status: PointStatus,
        reason: &str,
    ) -> QuotePoint {
        let mut row = self.base_row(head, point, status);
        row.failure_reason = Some(reason.to_string());
        row
    }

    fn skipped_reverse(
        &self,
        head: &ObservedHead,
        parent: &PlannedPoint,
        parent_row: &QuotePoint,
    ) -> QuotePoint {
        let reverse =
            plan_matched_reverse(parent, &self.metadata.run_id, &head.hash, "0".to_string());
        let reason = format!("parent point {} ended as {:?}", parent.point_id, parent_row.status);
        self.failure_row(head, &reverse, PointStatus::ReverseSkippedParentFailed, &reason)
    }

    fn base_row(
        &self,
        head: &ObservedHead,
        point: &PlannedPoint,
        status: PointStatus,
    ) -> QuotePoint {
        let timestamp = now_ms();
        QuotePoint {
            schema_version: SCHEMA_VERSION,
            point_id: point.point_id.clone(),
            run_id: self.metadata.run_id.clone(),
            grid_epoch_id: self.metadata.grid_epoch_id.clone(),
            pair_id: point.pair_id.clone(),
            direction: point.direction,
            depth_index: point.depth_index,
            quote_role: point.quote_role,
            attempt_id: 0,
            parent_point_id: point.parent_point_id.clone(),
            chain_id: 1,
            block_number: head.number,
            block_hash: head.hash.clone(),
            block_timestamp: head.timestamp,
            head_received_at_ms: head.received_at_ms,
            quote_started_at_ms: timestamp,
            quote_finished_at_ms: timestamp,
            batch_solve_time_ms: None,
            monotonic_duration_ms: 0,
            token_in: point.token_in.clone(),
            token_out: point.token_out.clone(),
            amount_in: point.amount_in.clone(),
            amount_out: None,
            amount_out_net_gas: None,
            gas_estimate: None,
            gas_price: None,
            price_impact_bps: None,
            forward_gross_output: (point.quote_role == QuoteRole::MatchedReverse)
                .then(|| point.amount_in.clone()),
            status,
            failure_reason: None,
            route_json: None,
            fynd_version: self.metadata.fynd_version.clone(),
            fynd_git_sha: self.metadata.fynd_git_sha.clone(),
            config_hash: self.metadata.config_hash.clone(),
            protocol_set_hash: self.metadata.protocol_set_hash.clone(),
            worker_config_hash: self.metadata.worker_config_hash.clone(),
        }
    }

    fn block_run(&self, head: &ObservedHead, input: BlockRunInput) -> BlockRun {
        BlockRun {
            schema_version: SCHEMA_VERSION,
            run_id: self.metadata.run_id.clone(),
            chain_id: 1,
            block_number: head.number,
            block_hash: head.hash.clone(),
            parent_hash: head.parent_hash.clone(),
            block_timestamp: head.timestamp,
            base_fee_per_gas: head.base_fee_per_gas,
            rpc_endpoint_id: head.rpc_endpoint_id.clone(),
            head_received_at_ms: head.received_at_ms,
            fynd_ready_at_ms: (input.fynd_ready_at_ms != 0).then_some(input.fynd_ready_at_ms),
            collection_started_at_ms: input.collection_started_at_ms,
            collection_finished_at_ms: input.collection_finished_at_ms,
            expected_rows: self.config.expected_rows_per_block(),
            scheduled_rows: input.scheduled_rows,
            successful_rows: input.successful_rows,
            failed_rows: input.scheduled_rows - input.successful_rows,
            market_negative_rows: input.market_negative_rows,
            status: if input.successful_rows + input.market_negative_rows ==
                input.scheduled_rows
            {
                "complete".to_string()
            } else {
                "partial".to_string()
            },
            config_hash: self.metadata.config_hash.clone(),
        }
    }
}

/// Rows that describe the market rather than a collection failure: the routed
/// universe has no path or depth, or a reverse was skipped because its parent
/// was market-negative.
fn count_market_negative(rows: &[QuotePoint]) -> usize {
    let statuses: HashMap<&str, &PointStatus> = rows
        .iter()
        .map(|row| (row.point_id.as_str(), &row.status))
        .collect();
    rows.iter()
        .filter(|row| match &row.status {
            PointStatus::NoRouteFound | PointStatus::InsufficientLiquidity => true,
            PointStatus::ReverseSkippedParentFailed => row
                .parent_point_id
                .as_deref()
                .and_then(|parent| statuses.get(parent))
                .is_some_and(|status| {
                    matches!(
                        status,
                        PointStatus::NoRouteFound | PointStatus::InsufficientLiquidity
                    )
                }),
            _ => false,
        })
        .count()
}

#[derive(Serialize)]
struct StoredRoute<'a> {
    swaps: Vec<StoredSwap<'a>>,
}

#[derive(Serialize)]
struct StoredSwap<'a> {
    component_id: &'a str,
    protocol: &'a str,
    token_in: String,
    token_out: String,
    amount_in: String,
    amount_out: String,
    gas_estimate: String,
    split: f64,
}

fn serialize_route(route: &Route) -> Result<String, serde_json::Error> {
    let swaps = route
        .swaps()
        .iter()
        .map(|swap| StoredSwap {
            component_id: swap.component_id(),
            protocol: swap.protocol(),
            token_in: swap.token_in().to_string(),
            token_out: swap.token_out().to_string(),
            amount_in: swap.amount_in().to_string(),
            amount_out: swap.amount_out().to_string(),
            gas_estimate: swap.gas_estimate().to_string(),
            split: *swap.split(),
        })
        .collect();
    serde_json::to_string(&StoredRoute { swaps })
}

fn replay_route(route: &Route) -> std::result::Result<(), String> {
    for swap in route.swaps() {
        let token_in = route
            .tokens()
            .get(swap.token_in())
            .ok_or_else(|| format!("missing input token metadata for {}", swap.token_in()))?;
        let token_out = route
            .tokens()
            .get(swap.token_out())
            .ok_or_else(|| format!("missing output token metadata for {}", swap.token_out()))?;
        let replay = swap
            .protocol_state()
            .get_amount_out(swap.amount_in().clone(), token_in, token_out)
            .map_err(|error| {
                format!("component {} simulation failed: {error}", swap.component_id())
            })?;
        if replay.amount != *swap.amount_out() {
            return Err(format!(
                "component {} expected {} but replay returned {}",
                swap.component_id(),
                swap.amount_out(),
                replay.amount
            ));
        }
    }
    Ok(())
}

/// Fynd state readiness for an RPC head.
pub enum StateWaitOutcome {
    /// Fynd exposes the exact target.
    Ready {
        /// Wall time of readiness.
        ready_at_ms: u64,
    },
    /// Fynd advanced past the target.
    Missed {
        /// Diagnostic context.
        reason: String,
    },
    /// Fynd did not reach the target before the deadline.
    Timeout {
        /// Diagnostic context.
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarketIdentity {
    number: u64,
    hash: String,
}

impl MarketIdentity {
    fn matches(&self, head: &ObservedHead) -> bool {
        self.number == head.number &&
            self.hash
                .eq_ignore_ascii_case(&head.hash)
    }
}

#[derive(Clone, Copy)]
struct WaveTiming {
    started_at_ms: u64,
    finished_at_ms: u64,
    duration_ms: u64,
}

struct BlockRunInput {
    fynd_ready_at_ms: u64,
    collection_started_at_ms: u64,
    collection_finished_at_ms: u64,
    scheduled_rows: usize,
    successful_rows: usize,
    market_negative_rows: usize,
}

async fn market_identity(market: &MarketData) -> Option<MarketIdentity> {
    let view = market.read().await;
    let block = view.last_updated()?;
    Some(MarketIdentity { number: block.number(), hash: block.hash().to_string() })
}

fn wave_state_matches(
    head: &ObservedHead,
    before: &MarketIdentity,
    after: Option<&MarketIdentity>,
) -> bool {
    before.matches(head) && after.is_some_and(|identity| identity.matches(head))
}

fn quote_matches_head(quote: &OrderQuote, head: &ObservedHead) -> bool {
    quote.block().number() == head.number &&
        quote
            .block()
            .hash()
            .eq_ignore_ascii_case(&head.hash) &&
        quote.solved_against() == &head.number.to_string()
}

fn point_status(status: QuoteStatus) -> PointStatus {
    match status {
        QuoteStatus::Success => PointStatus::Success,
        QuoteStatus::NoRouteFound => PointStatus::NoRouteFound,
        QuoteStatus::InsufficientLiquidity => PointStatus::InsufficientLiquidity,
        QuoteStatus::Timeout => PointStatus::Timeout,
        QuoteStatus::NotReady => PointStatus::NotReady,
        QuoteStatus::PriceCheckFailed => PointStatus::PriceCheckFailed,
        // QuoteStatus is non-exhaustive so future Fynd releases remain readable.
        _ => PointStatus::Unknown,
    }
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> ObservedHead {
        ObservedHead {
            number: 10,
            hash: "0xabc".into(),
            parent_hash: "0xparent".into(),
            timestamp: 1,
            base_fee_per_gas: Some(1),
            received_at_ms: 1,
            rpc_endpoint_id: "test".into(),
        }
    }

    #[test]
    fn state_contract_rejects_same_height_different_hash() {
        let target = head();
        let before = MarketIdentity { number: 10, hash: "0xabc".into() };
        let after = MarketIdentity { number: 10, hash: "0xdef".into() };

        assert!(!wave_state_matches(&target, &before, Some(&after)));
    }

    #[test]
    fn state_contract_accepts_exact_hash_before_and_after() {
        let target = head();
        let identity = MarketIdentity { number: 10, hash: "0xAbC".into() };

        assert!(wave_state_matches(&target, &identity, Some(&identity)));
    }

    fn quote_row(point_id: &str, status: PointStatus, parent: Option<&str>) -> QuotePoint {
        let token = crate::record::TokenRecord {
            address: "0x000000000000000000000000000000000000000a".into(),
            symbol: "A".into(),
            decimals: 18,
        };
        QuotePoint {
            schema_version: SCHEMA_VERSION,
            point_id: point_id.into(),
            run_id: "run".into(),
            grid_epoch_id: "grid".into(),
            pair_id: "a-b".into(),
            direction: crate::record::Direction::AToB,
            depth_index: 0,
            quote_role: if parent.is_some() {
                QuoteRole::MatchedReverse
            } else {
                QuoteRole::LadderForward
            },
            attempt_id: 0,
            parent_point_id: parent.map(Into::into),
            chain_id: 1,
            block_number: 10,
            block_hash: "0xabc".into(),
            block_timestamp: 1,
            head_received_at_ms: 1,
            quote_started_at_ms: 1,
            quote_finished_at_ms: 1,
            batch_solve_time_ms: None,
            monotonic_duration_ms: 0,
            token_in: token.clone(),
            token_out: token,
            amount_in: "1".into(),
            amount_out: None,
            amount_out_net_gas: None,
            gas_estimate: None,
            gas_price: None,
            price_impact_bps: None,
            forward_gross_output: None,
            status,
            failure_reason: None,
            route_json: None,
            fynd_version: "test".into(),
            fynd_git_sha: "test".into(),
            config_hash: "hash".into(),
            protocol_set_hash: "hash".into(),
            worker_config_hash: "hash".into(),
        }
    }

    #[test]
    fn market_negative_counts_no_route_and_its_skipped_reverse_only() {
        let rows = vec![
            quote_row("forward-no-route", PointStatus::NoRouteFound, None),
            quote_row(
                "reverse-of-no-route",
                PointStatus::ReverseSkippedParentFailed,
                Some("forward-no-route"),
            ),
            quote_row("forward-timeout", PointStatus::Timeout, None),
            quote_row(
                "reverse-of-timeout",
                PointStatus::ReverseSkippedParentFailed,
                Some("forward-timeout"),
            ),
            quote_row("forward-success", PointStatus::Success, None),
        ];

        assert_eq!(count_market_negative(&rows), 2);
    }
}
