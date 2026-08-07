//! Wiring between the monitor's block loop and the APEX batch stage: input building at the two
//! bracket seams, dispatch, and the asynchronous result drain.
//!
//! The block loop pays only for eligibility, the pool-subset clone, and a `try_send`; solving
//! happens on the stage's own threads and results join the `apex-YYYY-MM-DD.jsonl` stream
//! whenever they arrive. The per-order Fynd baseline is NOT solved here: the comparisons JSONL
//! already carries each trade's Fynd quote at the same two states (top = N−1, back = N), so the
//! offline join on `{tx_hash}:{tx_index}` is state-consistent by construction.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use apex_batch::{
    adapter::{from_apex_address, PoolAddressBook, TychoApexPool},
    apex_solver::types::Address as ApexAddress,
    live::{
        solve_live_batch, ComponentClearing, LiveBatchInput, LiveBatchReport, LiveOrder, LivePool,
        OrderStatus,
    },
    prices::TokenPriceInput,
    subset::{select_pool_subset, PoolCandidate},
};
use fynd_core::Solver;
use tracing::{debug, warn};

use crate::{
    decoder::DecodedTrade,
    resolve::apex_stage::{ApexStage, DispatchOutcome, StageDelivery},
    telemetry,
};

/// Which state a job's inputs were cloned at, mirroring the fynd brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bracket {
    /// N−1, the fair counterfactual — the headline state.
    Top,
    /// N, the biased bottom including the block's own impact.
    Bottom,
}

impl Bracket {
    fn label(self) -> &'static str {
        match self {
            Bracket::Top => "top",
            Bracket::Bottom => "bottom",
        }
    }
}

/// One dispatched solve: the bracket's cloned inputs plus bookkeeping for the join.
pub(crate) struct LiveJob {
    pub block: u64,
    pub bracket: Bracket,
    /// How many blocks of trades this batch covers. 1 is the in-block case; larger windows
    /// accumulate across blocks and clear at the window's closing state.
    pub window_blocks: u64,
    pub input: LiveBatchInput,
}

/// A solved job as delivered back to the monitor.
pub(crate) struct LiveDelivery {
    pub block: u64,
    pub bracket: Bracket,
    pub window_blocks: u64,
    pub report: LiveBatchReport,
}

/// Trades waiting for their window to close. One buffer per configured window size; the 1-block
/// window keeps no buffer since every block closes it immediately.
#[derive(Default)]
pub(crate) struct WindowBuffers {
    /// window size in blocks -> (block the window opened at, trades collected so far)
    pending: HashMap<u64, (u64, Vec<DecodedTrade>)>,
}

impl WindowBuffers {
    /// Add this block's trades to every window, and return the windows that just closed with
    /// their accumulated trades. A window of N closes when the block number is a multiple of N,
    /// so windows are aligned to absolute block numbers and never straddle a restart.
    pub(crate) fn absorb(
        &mut self,
        windows: &[u64],
        block: u64,
        trades: &[DecodedTrade],
    ) -> Vec<(u64, Vec<DecodedTrade>)> {
        let mut closed = Vec::new();
        for &window in windows {
            if window <= 1 {
                if !trades.is_empty() {
                    closed.push((window, trades.to_vec()));
                }
                continue;
            }
            let entry = self
                .pending
                .entry(window)
                .or_insert_with(|| (block, Vec::new()));
            entry.1.extend_from_slice(trades);
            if block.is_multiple_of(window) {
                let (_, collected) = self
                    .pending
                    .remove(&window)
                    .expect("just inserted");
                if !collected.is_empty() {
                    closed.push((window, collected));
                }
            }
        }
        closed
    }
}

/// The monitor's handle on the APEX stage: workers, delivery stream, JSONL sink, and budgets.
pub(crate) struct ApexRuntime {
    /// `None` only during [`ApexRuntime::shutdown`], which takes the stage to join its workers.
    stage: Option<ApexStage<LiveJob>>,
    deliveries: std::sync::mpsc::Receiver<StageDelivery<LiveDelivery>>,
    writer: super::jsonl::RotatingWriter,
    pub max_pools: usize,
    /// Stage-counter watermarks, so each drain emits only the delta to Prometheus.
    seen_overruns: u64,
    seen_worker_panics: u64,
}

/// The stage worker's occupancy envelope relative to the per-component budget: one job may hold
/// several component solves plus the singles control, so the discard threshold above the stage
/// (`OVERRUN_FACTOR` × this) must cover a legitimately busy block rather than bin it.
const JOB_ENVELOPE_FACTOR: u32 = 4;

impl ApexRuntime {
    /// The runtime the monitor's arguments describe, or `None` when `--apex-dir` is absent.
    pub(crate) fn from_args(cfg: &super::monitor::MonitorArgs) -> anyhow::Result<Option<Self>> {
        let Some(dir) = cfg.apex_dir.as_ref() else {
            return Ok(None);
        };
        let runtime = Self::spawn(
            dir,
            cfg.apex_workers,
            cfg.apex_queue_capacity,
            Duration::from_millis(cfg.apex_budget_ms),
            Duration::from_millis(cfg.apex_single_budget_ms),
            cfg.apex_max_pools,
        )?;
        tracing::info!(
            workers = cfg.apex_workers,
            queue = cfg.apex_queue_capacity,
            budget_ms = cfg.apex_budget_ms,
            "APEX batch stage running"
        );
        Ok(Some(runtime))
    }

    /// Spawn the stage. `component_budget` is APEX's per-component search budget (the plan's 1 s);
    /// `single_budget` caps each single-order control solve.
    pub(crate) fn spawn(
        apex_dir: &std::path::Path,
        workers: usize,
        queue_capacity: usize,
        component_budget: Duration,
        single_budget: Duration,
        max_pools: usize,
    ) -> anyhow::Result<Self> {
        let writer = super::jsonl::RotatingWriter::open(apex_dir, "apex")?;
        let (stage, deliveries) = ApexStage::spawn(
            workers,
            queue_capacity,
            component_budget * JOB_ENVELOPE_FACTOR,
            move |job: LiveJob, _deadline: Instant| {
                // Per-component deadlines are stamped inside the solve at each component's own
                // start; the stage deadline only shapes the overrun-discard envelope.
                let report = solve_live_batch(&job.input, component_budget, single_budget, true);
                LiveDelivery {
                    block: job.block,
                    bracket: job.bracket,
                    window_blocks: job.window_blocks,
                    report,
                }
            },
        );
        Ok(Self {
            stage: Some(stage),
            deliveries,
            writer,
            max_pools,
            seen_overruns: 0,
            seen_worker_panics: 0,
        })
    }

    /// Dispatch one bracket's job; a shed job is counted by the stage and recorded here.
    pub(crate) fn dispatch(&self, job: LiveJob) {
        let block = job.block;
        let bracket = job.bracket;
        let Some(stage) = self.stage.as_ref() else {
            return;
        };
        match stage.dispatch(job) {
            DispatchOutcome::Queued => {
                debug!(block, bracket = bracket.label(), "APEX job queued");
            }
            DispatchOutcome::Skipped(reason) => {
                telemetry::record_apex_skipped(reason);
                warn!(block, bracket = bracket.label(), ?reason, "APEX job shed");
            }
        }
    }

    /// Drain every delivered result: one JSONL line per solved bracket, plus metrics. Discarded
    /// overruns and worker-level panics never deliver, so their stage counters sync here as
    /// deltas.
    pub(crate) fn drain(&mut self) {
        if let Some(stage) = self.stage.as_ref() {
            let overruns = stage
                .counters()
                .overruns
                .load(std::sync::atomic::Ordering::Relaxed);
            for _ in self.seen_overruns..overruns {
                telemetry::record_apex_overrun();
            }
            self.seen_overruns = overruns;
            let worker_panics = stage
                .counters()
                .panics
                .load(std::sync::atomic::Ordering::Relaxed);
            for _ in self.seen_worker_panics..worker_panics {
                telemetry::record_apex_batch_errored();
            }
            self.seen_worker_panics = worker_panics;
        }

        while let Ok(delivery) = self.deliveries.try_recv() {
            self.record_delivery(&delivery);
        }
    }

    /// One delivered bracket: metrics, then its JSONL line.
    fn record_delivery(&mut self, delivery: &StageDelivery<LiveDelivery>) {
        {
            telemetry::record_apex_delivery(&delivery.timing);
            let LiveDelivery { block, bracket, window_blocks, report } = &delivery.result;
            for _ in 0..report
                .counters
                .deadline_fired_components
            {
                telemetry::record_apex_deadline_fired();
            }
            if report.counters.solver_panics > 0 ||
                !report
                    .counters
                    .component_errors
                    .is_empty()
            {
                telemetry::record_apex_batch_errored();
            }
            for (kind, count) in &report.counters.component_errors {
                for _ in 0..*count {
                    telemetry::record_apex_component_error(match kind.as_str() {
                        "invalid_input" => "invalid_input",
                        "trade_solver" => "trade_solver",
                        "market_router" => "market_router",
                        "clearing_under_limit" => "clearing_under_limit",
                        "negative_balance_delta" => "negative_balance_delta",
                        _ => "other",
                    });
                }
            }
            let mut status_counts: HashMap<&'static str, u64> = HashMap::new();
            let orders: Vec<serde_json::Value> = report
                .statuses
                .iter()
                .map(|(id, status)| {
                    let (label, bought_raw, fill_ratio) = status_fields(status);
                    *status_counts.entry(label).or_default() += 1;
                    serde_json::json!({
                        "id": id,
                        "status": label,
                        "bought_raw": bought_raw,
                        "fill_ratio": fill_ratio,
                    })
                })
                .collect();
            for (label, count) in status_counts {
                telemetry::record_apex_orders(label, count);
            }
            let singles: Vec<serde_json::Value> = report
                .singles
                .iter()
                .map(|single| {
                    serde_json::json!({
                        "id": single.id,
                        "bought_raw": single.bought_raw.map(|amount| amount.to_string()),
                    })
                })
                .collect();
            // Internalization on FILLED notional: 1 = the bracket cleared purely order-against-
            // order, 0 = everything routed through pools. `None` when nothing filled, so a
            // fill-free bracket cannot masquerade as fully internalized.
            let internalization = (report.filled_notional_wei > 0.0).then(|| {
                (1.0 - report.pool_cleared_wei / (2.0 * report.filled_notional_wei)).clamp(0.0, 1.0)
            });
            let line = serde_json::json!({
                "block": block,
                "bracket": bracket.label(),
                "window_blocks": window_blocks,
                "queue_wait_ms": u128_ms(delivery.timing.queue_wait.as_millis()),
                "solve_wall_ms": u128_ms(delivery.timing.solve_wall.as_millis()),
                "component_solve_ms": report.component_solve_ms,
                "counters": report.counters,
                "pool_cleared_wei": report.pool_cleared_wei,
                "filled_notional_wei": report.filled_notional_wei,
                "internalization_share": internalization,
                "orders": orders,
                "singles": singles,
                "components": report
                    .components
                    .iter()
                    .map(component_record)
                    .collect::<Vec<_>>(),
            });
            let writer = self.writer.writer();
            if let Err(error) = {
                use std::io::Write as _;
                writeln!(writer, "{line}")
            } {
                warn!(%error, "failed to append an apex JSONL line");
            }
        }
    }

    /// Final drain + worker teardown at end of run: join the workers first so every queued job
    /// finishes delivering, then drain the tail.
    pub(crate) fn shutdown(mut self) {
        if let Some(stage) = self.stage.take() {
            stage.shutdown();
        }
        self.drain();
    }
}

/// Milliseconds as a JSON-representable u64; a solve cannot run for 584 million years.
fn u128_ms(millis: u128) -> u64 {
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// One solved component's clearing, verbatim: the uniform price vector plus every pool and order
/// leg it cleared. APEX builds no calldata, so this is what makes the solve reconstructible
/// offline — amounts stay decimal strings because they are 18-decimal `U256`s, not JSON numbers.
fn component_record(component: &ComponentClearing) -> serde_json::Value {
    let clearing_prices: Vec<serde_json::Value> = component
        .clearing_prices
        .iter()
        .map(|(token, price)| {
            serde_json::json!({
                "token": format!("{:#x}", from_apex_address(*token)),
                "price": price.to_string(),
            })
        })
        .collect();
    let pool_clearings: Vec<serde_json::Value> = component
        .pool_clearings
        .iter()
        .map(|clearing| {
            serde_json::json!({
                "apex_address": format!("{:#x}", from_apex_address(clearing.apex_address)),
                "component_id": clearing.component_id,
                "protocol": clearing.protocol,
                "sell_token": format!("{:#x}", from_apex_address(clearing.sell_token)),
                "buy_token": format!("{:#x}", from_apex_address(clearing.buy_token)),
                "sold_amount": clearing.sold_amount.to_string(),
                "bought_amount": clearing.bought_amount.to_string(),
                "surplus": clearing.surplus.to_string(),
                "fee": clearing.fee.map(|fee| fee.to_string()),
            })
        })
        .collect();
    let order_clearings: Vec<serde_json::Value> = component
        .order_clearings
        .iter()
        .map(|clearing| {
            serde_json::json!({
                "id": clearing.id,
                "owner": format!("{:#x}", from_apex_address(clearing.owner)),
                "sell_token": format!("{:#x}", from_apex_address(clearing.sell_token)),
                "buy_token": format!("{:#x}", from_apex_address(clearing.buy_token)),
                "sold_amount": clearing.sold_amount.to_string(),
                "bought_amount": clearing.bought_amount.to_string(),
            })
        })
        .collect();
    serde_json::json!({
        "clearing_prices": clearing_prices,
        "pool_clearings": pool_clearings,
        "order_clearings": order_clearings,
    })
}

fn status_fields(status: &OrderStatus) -> (&'static str, Option<String>, Option<f64>) {
    match status {
        OrderStatus::Filled { bought_raw, fill_ratio } => {
            ("filled", Some(bought_raw.to_string()), Some(*fill_ratio))
        }
        OrderStatus::PartiallyFilled { bought_raw, fill_ratio } => {
            ("partially_filled", Some(bought_raw.to_string()), Some(*fill_ratio))
        }
        OrderStatus::UnfilledAtLimit => ("unfilled_at_limit", None, None),
        OrderStatus::ClusterCut => ("cluster_cut", None, None),
        OrderStatus::ComponentErrored => ("component_errored", None, None),
        OrderStatus::Excluded(reason) => (reason, None, None),
    }
}

/// Build one bracket's inputs from the solver's current state and dispatch it. A `None` runtime
/// is a no-op so the call sites stay unconditional.
pub(crate) async fn dispatch_bracket(
    apex: Option<&ApexRuntime>,
    solver: &Solver,
    trades: &[DecodedTrade],
    block: u64,
    bracket: Bracket,
    window_blocks: u64,
) {
    let Some(runtime) = apex else { return };
    let input = build_live_input(solver, trades, runtime.max_pools).await;
    runtime.dispatch(LiveJob { block, bracket, window_blocks, input });
}

/// Whether this block's trades justify a batch dispatch: at least two orders sharing a token.
pub(crate) fn should_dispatch(trades: &[DecodedTrade]) -> bool {
    let pairs: Vec<(ApexAddress, ApexAddress)> = trades
        .iter()
        .map(|trade| {
            (ApexAddress(trade.token_in.into_array()), ApexAddress(trade.token_out.into_array()))
        })
        .collect();
    apex_batch::live::batch_eligible(&pairs)
}

/// Clone one bracket's inputs from the solver's current state: the native-only 2-hop pool subset
/// around the block's order tokens, the price rationals, and the orders with their floors.
///
/// Holds the market read guard across candidate collection and the subset clone (the same pattern
/// the offline snapshot uses); the subset is small by construction, and the block loop is this
/// guard's only other reader.
pub(crate) async fn build_live_input(
    solver: &Solver,
    trades: &[DecodedTrade],
    max_pools: usize,
) -> LiveBatchInput {
    let mut orders: Vec<LiveOrder> = Vec::with_capacity(trades.len());
    let mut order_tokens: HashSet<[u8; 20]> = HashSet::new();
    for trade in trades {
        let (Some(min_out), _source) = crate::capture::limit_for(trade) else {
            continue;
        };
        order_tokens.insert(trade.token_in.into_array());
        order_tokens.insert(trade.token_out.into_array());
        orders.push(LiveOrder {
            id: format!("{}:{}", trade.tx_hash, trade.tx_index),
            token_in: ApexAddress(trade.token_in.into_array()),
            token_out: ApexAddress(trade.token_out.into_array()),
            amount_in_raw: trade.amount_in,
            min_out_raw: min_out,
        });
    }

    let market = solver.market_data();
    let view = market.read().await;
    let state = view.base_market_state();

    let pools = clone_pool_subset(state, &order_tokens, max_pools);

    // Price rationals + metadata for the closure (order tokens ∪ kept pool tokens).
    let mut wanted: HashSet<[u8; 20]> = order_tokens;
    for pool in &pools {
        wanted.insert(pool.token_0.0);
        wanted.insert(pool.token_1.0);
    }
    let mut price_inputs = HashMap::new();
    let mut token_meta = HashMap::new();
    let derived = solver.derived_data();
    let derived_guard = derived.read().await;
    if let Some(token_prices) = derived_guard.token_prices() {
        for token_bytes in wanted {
            let address = tycho_simulation::tycho_common::Bytes::from(token_bytes.to_vec());
            let Some(token) = state.get_token(&address) else { continue };
            let Ok(decimals) = u8::try_from(token.decimals) else { continue };
            token_meta.insert(ApexAddress(token_bytes), (token.symbol.clone(), decimals));
            if let Some(price) = token_prices.get(&address) {
                price_inputs.insert(
                    ApexAddress(token_bytes),
                    TokenPriceInput {
                        numerator: price.numerator.clone(),
                        denominator: price.denominator.clone(),
                        decimals,
                    },
                );
            }
        }
    }

    LiveBatchInput { orders, pools, price_inputs, token_meta }
}

/// The native-only 2-hop pool subset around `order_tokens`, cloned and wrapped for APEX.
fn clone_pool_subset(
    state: &fynd_core::feed::market_data::MarketState,
    order_tokens: &HashSet<[u8; 20]>,
    max_pools: usize,
) -> Vec<LivePool> {
    let mut candidates: Vec<PoolCandidate> = Vec::new();
    for (component_id, token_addresses) in state.component_topology() {
        let Some(component) = state.get_component(&component_id) else { continue };
        candidates.push(PoolCandidate {
            component_id: component_id.clone(),
            protocol_system: component.protocol_system.clone(),
            tokens: token_addresses
                .iter()
                .filter_map(|address| {
                    let bytes: &[u8] = address.as_ref();
                    (bytes.len() == 20).then(|| {
                        let mut out = [0u8; 20];
                        out.copy_from_slice(bytes);
                        out
                    })
                })
                .collect(),
        });
    }
    let selection = select_pool_subset(&candidates, order_tokens, max_pools);

    let mut address_book = PoolAddressBook::default();
    let mut pools: Vec<LivePool> = Vec::new();
    let candidate_by_id: HashMap<&str, &PoolCandidate> = candidates
        .iter()
        .map(|candidate| (candidate.component_id.as_str(), candidate))
        .collect();
    for component_id in &selection.kept {
        let candidate = candidate_by_id[component_id.as_str()];
        if candidate.tokens.len() != 2 {
            continue;
        }
        let Some(simulation) = state.get_simulation_state(component_id) else { continue };
        let apex_address = match address_book.register(component_id) {
            Ok(address) => address,
            Err(other) => {
                warn!(component_id, colliding = other, "pool address collision; pool skipped");
                continue;
            }
        };
        let mut token_map = HashMap::new();
        for token_bytes in &candidate.tokens {
            let address = tycho_simulation::tycho_common::Bytes::from(token_bytes.to_vec());
            let Some(token) = state.get_token(&address) else { continue };
            token_map.insert(ApexAddress(*token_bytes), token.clone());
        }
        if token_map.len() != 2 {
            continue;
        }
        let boxed = simulation.clone_box();
        pools.push(LivePool {
            component_id: component_id.clone(),
            apex_address,
            token_0: ApexAddress(candidate.tokens[0]),
            token_1: ApexAddress(candidate.tokens[1]),
            adapter: std::sync::Arc::new(TychoApexPool {
                protocol: candidate.protocol_system.clone(),
                tokens: token_map,
                pool: std::sync::Arc::from(boxed),
            }),
        });
    }
    pools
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, U256};

    use super::*;
    use crate::resolve::test_support::trade_between;

    #[test]
    fn test_windows_accumulate_until_their_block_boundary() {
        let weth = Address::repeat_byte(1);
        let usdc = Address::repeat_byte(2);
        let mut buffers = WindowBuffers::default();
        let windows = vec![1u64, 4];

        // Blocks 101..103 close the 1-block window every time and never the 4-block one.
        for block in 101..=103 {
            let closed = buffers.absorb(&windows, block, &[trade_between(weth, usdc, 0)]);
            assert_eq!(closed.len(), 1, "only the 1-block window closes at {block}");
            assert_eq!(closed[0].0, 1);
            assert_eq!(closed[0].1.len(), 1);
        }

        // 104 is divisible by 4: the long window closes carrying every trade since it opened.
        let closed = buffers.absorb(&windows, 104, &[trade_between(weth, usdc, 0)]);
        let long = closed
            .iter()
            .find(|(window, _)| *window == 4)
            .expect("4-block window closed");
        assert_eq!(long.1.len(), 4, "three buffered blocks plus the closing one");

        // ...and it starts empty again rather than replaying what it already dispatched.
        let closed = buffers.absorb(&windows, 105, &[trade_between(weth, usdc, 0)]);
        assert!(closed
            .iter()
            .all(|(window, _)| *window == 1));
    }

    #[test]
    fn test_empty_blocks_never_dispatch() {
        let mut buffers = WindowBuffers::default();
        assert!(buffers
            .absorb(&[1, 4], 104, &[])
            .is_empty());
    }

    #[test]
    fn test_should_dispatch_needs_two_trades_sharing_a_token() {
        let weth = Address::repeat_byte(1);
        let usdc = Address::repeat_byte(2);
        let dai = Address::repeat_byte(3);
        let pepe = Address::repeat_byte(4);

        let lone = vec![trade_between(weth, usdc, 0)];
        assert!(!should_dispatch(&lone), "a single trade is never a batch");

        let disjoint = vec![trade_between(weth, usdc, 0), trade_between(dai, pepe, 1)];
        assert!(!should_dispatch(&disjoint), "no shared token, nothing to cross or net");

        let connected = vec![trade_between(weth, usdc, 0), trade_between(usdc, dai, 1)];
        assert!(should_dispatch(&connected));
    }

    /// Two 18-dec tokens priced 1:1, one crossing pair with permissive floors — fills poollessly.
    fn crossing_input() -> apex_batch::live::LiveBatchInput {
        let token_a = ApexAddress([1u8; 20]);
        let token_b = ApexAddress([2u8; 20]);
        let one_to_one = || TokenPriceInput {
            numerator: num_bigint::BigUint::from(10u128.pow(18)),
            denominator: num_bigint::BigUint::from(10u128.pow(18)),
            decimals: 18,
        };
        let amount_in = U256::from(10u128.pow(18));
        let floor = U256::from(9 * 10u128.pow(17));
        let order = |id: &str, token_in, token_out| LiveOrder {
            id: id.to_string(),
            token_in,
            token_out,
            amount_in_raw: amount_in,
            min_out_raw: floor,
        };
        LiveBatchInput {
            orders: vec![order("a:0", token_a, token_b), order("b:0", token_b, token_a)],
            pools: Vec::new(),
            price_inputs: HashMap::from([(token_a, one_to_one()), (token_b, one_to_one())]),
            token_meta: HashMap::from([
                (token_a, ("AAA".to_string(), 18)),
                (token_b, ("BBB".to_string(), 18)),
            ]),
        }
    }

    #[test]
    fn test_runtime_lands_one_jsonl_line_per_bracket() {
        // The monitor's contract with the offline join: every dispatched bracket becomes exactly
        // one JSONL line carrying its block, bracket label, and per-order reconciled statuses.
        // `shutdown` joins the workers before the tail drain, so both lines must have landed.
        let dir = std::env::temp_dir().join(format!("apex-live-runtime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let runtime =
            ApexRuntime::spawn(&dir, 1, 4, Duration::from_secs(2), Duration::from_millis(250), 400)
                .expect("spawn runtime");

        let input = crossing_input();
        runtime.dispatch(LiveJob {
            block: 42,
            bracket: Bracket::Top,
            window_blocks: 1,
            input: input.clone(),
        });
        runtime.dispatch(LiveJob { block: 42, bracket: Bracket::Bottom, window_blocks: 1, input });
        runtime.shutdown();

        let mut lines: Vec<serde_json::Value> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read temp dir") {
            let path = entry.expect("dir entry").path();
            if path
                .extension()
                .is_some_and(|ext| ext == "jsonl")
            {
                let content = std::fs::read_to_string(&path).expect("read jsonl");
                lines.extend(
                    content
                        .lines()
                        .map(|line| serde_json::from_str(line).expect("valid json line")),
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(lines.len(), 2, "one line per bracket: {lines:?}");
        let mut brackets: Vec<&str> = lines
            .iter()
            .map(|line| {
                line["bracket"]
                    .as_str()
                    .expect("bracket label")
            })
            .collect();
        brackets.sort_unstable();
        assert_eq!(brackets, vec!["bottom", "top"]);
        for line in &lines {
            assert_eq!(line["block"].as_u64(), Some(42));
            let orders = line["orders"]
                .as_array()
                .expect("orders array");
            assert_eq!(orders.len(), 2, "{line}");
            assert!(
                orders
                    .iter()
                    .all(|order| order["status"] == "filled"),
                "the crossing pair reconciles as filled in both brackets: {line}"
            );
            // APEX's clearing is the record's substitute for calldata: without the price vector
            // and the order legs the line cannot be reconstructed offline.
            let components = line["components"]
                .as_array()
                .expect("components array");
            assert_eq!(components.len(), 1, "{line}");
            assert_eq!(
                components[0]["clearing_prices"]
                    .as_array()
                    .map(Vec::len),
                Some(2),
                "{line}"
            );
            assert_eq!(
                components[0]["order_clearings"]
                    .as_array()
                    .map(Vec::len),
                Some(2),
                "{line}"
            );
        }
    }

    #[test]
    fn test_live_orders_carry_extracted_or_synthetic_floors() {
        // The order builder is exercised through build_live_input's order loop; here the floor
        // logic itself: an extracted limit passes through, an absent one gets the synthetic
        // floor — both via capture::limit_for, the single implementation.
        let mut with_limit = trade_between(Address::repeat_byte(1), Address::repeat_byte(2), 0);
        with_limit.min_amount_out = Some(U256::from(950u64));
        let (floor, _) = crate::capture::limit_for(&with_limit);
        assert_eq!(floor, Some(U256::from(950u64)));

        let mut synthetic = trade_between(Address::repeat_byte(1), Address::repeat_byte(2), 1);
        synthetic.min_amount_out = None;
        synthetic.amount_out = U256::from(1_000_000u64);
        let (floor, _) = crate::capture::limit_for(&synthetic);
        assert_eq!(floor, Some(U256::from(990_000u64)), "100 bps below executed");
    }
}
