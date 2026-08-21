//! Live two-state monitor: drive an in-process `fynd-core` solver one block at a time, solving
//! each block's settled trades at top-of-block (N-1) and measuring twice at back-of-block (N) —
//! each top route is re-executed to isolate slippage between quote time and execution time, and
//! each trade is solved fresh to show what routing at the block's end state would deliver.
//!
//! The block barrier is deterministic: after releasing a block via
//! `BlockStepController::trigger_next_block`, we wait until the solver's `MarketData` reports the
//! next applied block before re-solving back-of-block. The pure orchestration is unit-tested in the
//! parent module via a mock `SteppingSolver`; this live driver is exercised by the gated
//! integration test in `tests/` (requires `TYCHO_URL` + `RPC_URL`).

use std::time::{Duration, Instant};

use alloy::{primitives::Address, providers::Provider};
use fynd_core::Solver;
use tracing::{debug, info, warn};
use tycho_simulation::tycho_common::models::{Address as CoreAddress, Chain};

use crate::{
    decoder::{DecodedTrade, Decoder, Registry},
    provider_from,
    resolve::{
        resolve_block_range,
        session::{self, Session, SolverArgs},
        step::{Encoding, StepAdapter},
        SteppingSolver,
    },
    telemetry,
    usd::Prices,
};

/// How far the receipts RPC may trail the Tycho stream before the monitor decodes the target block
/// anyway. In blocks, not seconds, because an RPC node trails by blocks.
const DECODE_RPC_LAG_BUDGET_BLOCKS: u32 = 3;
/// How often to re-check the RPC head while waiting out that lag. Well under a block time: the
/// monitor only idles until the block lands, and `eth_blockNumber` is cheap.
const DECODE_RPC_LAG_POLL: Duration = Duration::from_millis(250);

/// Inputs for the live monitor — the `monitor` subcommand's CLI arguments, used directly as its
/// configuration.
#[derive(clap::Args)]
pub(crate) struct MonitorArgs {
    #[command(flatten)]
    pub solver: SolverArgs,

    /// Write one JSON line per re-solved trade (every comparison — wins, losses, and unsolvable
    /// coverage gaps) into this directory as `comparisons-YYYY-MM-DD.jsonl`, rotated at each UTC
    /// day boundary so an external sync job can ship closed daily files. Each record carries
    /// both block states with verdict, bps, USD delta, and a slim route/calldata or unsolvable
    /// reason; filter downstream for the improvement or coverage view
    #[arg(long)]
    pub comparisons_dir: Option<std::path::PathBuf>,
}

/// Decode `block`, first waiting out any RPC lag. The HTTP RPC used for receipts can trail the
/// Tycho stream that drives `block`, so poll the RPC head until it reaches `block` or `budget`
/// expires — that distinguishes a transient race from a real failure. A block still undecodable
/// once the RPC has indexed it is a genuine error and surfaces to the caller.
async fn decode_block_when_available<P: Provider>(
    decoder: &mut Decoder<P>,
    block: u64,
    budget: Duration,
) -> anyhow::Result<Vec<DecodedTrade>> {
    let started = Instant::now();
    let mut logged_wait = false;
    loop {
        let head = match decoder
            .provider()
            .get_block_number()
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(block, "failed to fetch RPC block number: {e}");
                0
            }
        };
        if head >= block {
            break;
        }
        // Once per block, not once per poll: at this cadence a per-poll line would bury the log.
        if !logged_wait {
            logged_wait = true;
            debug!(block, head, "RPC lags the tycho stream; waiting for it to index the block");
        }
        if started.elapsed() >= budget {
            warn!(
                block,
                head,
                waited_ms = started.elapsed().as_millis(),
                "RPC never indexed the block within its lag budget; decoding anyway"
            );
            break;
        }
        tokio::time::sleep(DECODE_RPC_LAG_POLL).await;
    }
    telemetry::record_rpc_index_wait(started.elapsed());
    decoder.decode_block(block).await
}

/// One session's terminal condition: the run completed (`--max-blocks` reached) or the session
/// is unhealthy — the feed died, or the monitor fell too far behind head — and the caller should
/// rebuild the solver.
enum SessionEnd {
    Complete,
    Unhealthy(String),
}

/// Counters that survive feed rebuilds, so `--max-blocks` and skip logging span the whole run.
#[derive(Default)]
struct Totals {
    processed: u64,
    skipped_blocks: u64,
}

/// The pacing budgets one session runs against, both scaled to the chain's block time.
struct Pacing {
    /// Chain-head lag beyond which the session is unhealthy and the solver is rebuilt.
    max_lag_blocks: u64,
    /// How long to wait for the receipts RPC to index the target block before decoding regardless.
    rpc_lag_budget: Duration,
}

impl Pacing {
    fn for_chain(chain: Chain, max_lag_blocks: Option<u64>) -> Self {
        Self {
            max_lag_blocks: max_lag_blocks.unwrap_or_else(|| session::default_lag_blocks(chain)),
            rpc_lag_budget: session::block_time(chain) * DECODE_RPC_LAG_BUDGET_BLOCKS,
        }
    }
}

/// Build the in-process stepped solver and re-solve each block's settled trades as a top/back
/// range. When the tycho feed dies (its stream ends, or no block arrives within
/// `FEED_DEAD_TIMEOUT`), the solver is torn down and rebuilt in place — fresh subscriptions,
/// same decoder cache and comparisons file — so a long unattended run survives feed failures.
/// Ctrl-C stops the run cleanly at any await point, tearing the current solver down before
/// returning.
pub(crate) async fn run(cfg: MonitorArgs) -> anyhow::Result<()> {
    let comparisons_dir = cfg.comparisons_dir;
    let session = Session::prepare(cfg.solver).await?;
    let chain = session.chain();

    let mut decoder = Decoder::new(
        provider_from(&session.args().chain.rpc_url)?,
        session.args().chain.load_registry()?,
    );

    if let Some(port) = session.args().metrics_port {
        telemetry::install_exporter(port)?;
        info!(port, "serving Prometheus metrics at /metrics");
    }

    let mut comparisons = match comparisons_dir.as_ref() {
        Some(dir) => {
            let writer = super::jsonl::RotatingWriter::open(dir, super::jsonl::COMPARISONS_PREFIX)?;
            info!(path = %writer.current_path().display(), "appending comparisons to JSONL");
            Some(writer)
        }
        None => None,
    };

    let mut totals = Totals::default();
    let pacing = Pacing::for_chain(chain, session.args().max_lag_blocks);
    info!(
        max_lag_blocks = pacing.max_lag_blocks,
        rpc_lag_budget_ms = pacing.rpc_lag_budget.as_millis(),
        "chain pacing budgets"
    );

    // Resolves on Ctrl-C, so a long run stops cleanly at any await below — including the
    // multi-minute solver builds. On shutdown the in-flight block is abandoned, the solver's
    // workers and background tasks are torn down, and `comparisons` flushes as it drops.
    let shutdown = session::shutdown_signal();
    tokio::pin!(shutdown);

    // The first build fails fast — an error here is a configuration problem. Rebuilds after a
    // feed death retry forever, since the config is known good and failures are transient.
    let (mut solver, mut controller) = tokio::select! {
        biased;
        () = &mut shutdown => return Ok(()),
        built = session.build() => built?,
    };
    loop {
        // Encoding on: each comparison record carries the quote's on-chain transaction.
        let adapter =
            StepAdapter::new(&solver, &controller, session.args().timeout_ms, Encoding::Requested);
        let reason = tokio::select! {
            biased;
            () = &mut shutdown => {
                info!("received Ctrl-C; shutting down");
                break;
            }
            end = run_session(
                &session,
                &pacing,
                &adapter,
                &mut decoder,
                &mut comparisons,
                &mut totals,
            ) => match end {
                SessionEnd::Complete => break,
                SessionEnd::Unhealthy(reason) => reason,
            },
        };
        warn!(reason, "session unhealthy; rebuilding the solver to resubscribe");
        telemetry::record_feed_rebuild();
        solver.shutdown();
        let Some(built) = session.rebuild(shutdown.as_mut()).await else {
            info!("received Ctrl-C during rebuild; shutting down");
            return Ok(());
        };
        (solver, controller) = built;
    }
    solver.shutdown();
    Ok(())
}

/// Drive one solver session: step blocks and re-solve each block's settled trades until the run
/// completes or the feed dies.
async fn run_session<P: Provider>(
    session: &Session,
    pacing: &Pacing,
    adapter: &StepAdapter<'_>,
    decoder: &mut Decoder<P>,
    comparisons: &mut Option<super::jsonl::RotatingWriter>,
    totals: &mut Totals,
) -> SessionEnd {
    // Establish a baseline applied state (N-1) before the first comparison.
    if adapter.current_block().await.is_none() {
        info!("waiting for solver to apply its first block…");
        if let Err(e) = adapter.advance().await {
            return SessionEnd::Unhealthy(e.to_string());
        }
    }

    loop {
        if adapter
            .controller()
            .peek_next_block()
            .await
            .is_none()
        {
            return SessionEnd::Unhealthy("block stream ended".to_string());
        }
        let Some(top_block) = adapter.current_block().await else {
            warn!("no applied block yet; advancing");
            if let Err(e) = adapter.advance().await {
                return SessionEnd::Unhealthy(e.to_string());
            }
            continue;
        };
        let target = top_block + 1;

        // A healthy monitor keeps pace with the chain. Skip the check on a transient RPC error —
        // rebuilding is expensive (minutes of token loading), so only a confirmed lag triggers it.
        if let Ok(head) = decoder
            .provider()
            .get_block_number()
            .await
        {
            telemetry::record_head_lag_blocks(head.saturating_sub(target));
            if head.saturating_sub(target) > pacing.max_lag_blocks {
                return SessionEnd::Unhealthy(format!(
                    "monitor is {} blocks behind head {head}; presuming an unhealthy session",
                    head - target
                ));
            }
        }

        let trades = match decode_block_when_available(decoder, target, pacing.rpc_lag_budget).await
        {
            Ok(trades) => trades,
            Err(e) => {
                totals.skipped_blocks += 1;
                telemetry::record_skipped_block();
                warn!(
                    block = target,
                    skipped_total = totals.skipped_blocks,
                    "decode failed, skipping block: {e}"
                );
                if let Err(e) = adapter.advance().await {
                    return SessionEnd::Unhealthy(e.to_string());
                }
                continue;
            }
        };

        let start = Instant::now();
        // Snapshot token prices at top-of-block (N-1) for the headline metric and the top-of-block
        // USD valuation.
        let prices_top = snapshot_prices(adapter.solver(), decoder.registry()).await;
        let ranges = match resolve_block_range(adapter, &trades, &prices_top).await {
            Ok(ranges) => ranges,
            Err(e) => return SessionEnd::Unhealthy(e.to_string()),
        };
        // resolve_block_range advanced the solver to back-of-block (N); snapshot again so the
        // back-of-block improvement is valued against the state it was solved at.
        let prices_back = snapshot_prices(adapter.solver(), decoder.registry()).await;
        // The back-of-block solve should land on `target`. On a reorg/gap/resync the stream can
        // apply a different block, silently pairing the back state with another block's trades.
        // The top-of-block (N-1) headline is unaffected; warn so the mispaired back state is
        // visible.
        let applied = adapter.current_block().await;
        if applied != Some(target) {
            warn!(
                target,
                applied = ?applied,
                "back-of-block state is not the target block; back comparison may be off"
            );
        }
        for range in &ranges {
            telemetry::record_range(
                range,
                &session.args().chain.name,
                &prices_top,
                &prices_back,
                decoder.registry(),
            );
        }
        if let Some(rotating) = comparisons.as_mut() {
            super::jsonl::write_comparisons(rotating.writer(), &ranges, &prices_top, &prices_back);
        }
        let elapsed_s = start.elapsed().as_secs_f64();
        telemetry::record_block_seconds(elapsed_s);

        info!(block = target, trades = ranges.len(), elapsed_s, "re-solved block (top/back)");

        totals.processed += 1;
        if session
            .args()
            .max_blocks
            .is_some_and(|max| totals.processed >= max)
        {
            info!(processed = totals.processed, "reached --max-blocks");
            return SessionEnd::Complete;
        }
    }
}

/// Snapshot the solver's current token prices as `Prices` (token native-units per wei of
/// the gas token), anchored by `registry`'s USD anchor tokens. Empty until the first
/// derived-data computation completes; tokens with an unconvertible price are skipped.
async fn snapshot_prices(solver: &Solver, registry: &Registry) -> Prices {
    let mut prices = Prices::new(registry);
    let derived = solver.derived_data();
    let guard = derived.read().await;
    let Some(token_prices) = guard.token_prices() else {
        return prices;
    };
    for (token, price) in token_prices {
        let (Ok(numerator), Ok(denominator)) = (
            price
                .numerator
                .to_string()
                .parse::<f64>(),
            price
                .denominator
                .to_string()
                .parse::<f64>(),
        ) else {
            continue;
        };
        if denominator <= 0.0 {
            continue;
        }
        if let Some(address) = core_to_alloy(token) {
            prices.insert(address, numerator / denominator);
        }
    }
    prices
}

/// Convert a tycho-core 20-byte address to an alloy `Address`.
fn core_to_alloy(address: &CoreAddress) -> Option<Address> {
    let bytes: &[u8] = address.as_ref();
    (bytes.len() == 20).then(|| Address::from_slice(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pacing_scales_both_budgets_with_block_time() {
        // The RPC-lag budget is a block count, so it must shrink on a fast chain: a fixed
        // seconds budget spends several Base blocks waiting for a block that already landed.
        let base = Pacing::for_chain(Chain::Base, None);
        assert_eq!(base.max_lag_blocks, 600);
        assert_eq!(base.rpc_lag_budget, Duration::from_secs(6));

        let ethereum = Pacing::for_chain(Chain::Ethereum, None);
        assert_eq!(ethereum.max_lag_blocks, 100);
        assert_eq!(ethereum.rpc_lag_budget, Duration::from_secs(36));

        // An explicit --max-lag-blocks overrides only the lag threshold.
        let overridden = Pacing::for_chain(Chain::Base, Some(7));
        assert_eq!(overridden.max_lag_blocks, 7);
        assert_eq!(overridden.rpc_lag_budget, Duration::from_secs(6));
    }

    /// A mocked provider whose `eth_blockNumber` answers come from `heads`, in order.
    fn decoder_with_heads(heads: &[u64]) -> Decoder<impl Provider> {
        use alloy::providers::{mock::Asserter, ProviderBuilder};

        let asserter = Asserter::new();
        for head in heads {
            asserter.push_success(&format!("0x{head:x}"));
        }
        Decoder::new(
            ProviderBuilder::default().connect_mocked_client(asserter),
            Registry::ethereum(),
        )
    }

    #[tokio::test]
    async fn test_decode_waits_only_while_the_rpc_lags() {
        // Head already covers the target: no wait, straight to decoding (which then fails on the
        // receipts call the mock has no answer for — proof it got that far).
        let mut ready = decoder_with_heads(&[100]);
        let started = Instant::now();
        assert!(decode_block_when_available(&mut ready, 100, Duration::from_secs(6))
            .await
            .is_err());
        assert!(started.elapsed() < DECODE_RPC_LAG_POLL, "waited despite an indexed block");
    }

    #[tokio::test]
    async fn test_decode_gives_up_after_the_lag_budget() {
        // Head never reaches the target. A zero budget proves the wait is bounded by the budget
        // alone — the old fixed backoff slept before ever re-checking, so it could not express
        // "don't wait".
        let mut lagging = decoder_with_heads(&[99]);
        let started = Instant::now();
        assert!(decode_block_when_available(&mut lagging, 100, Duration::ZERO)
            .await
            .is_err());
        assert!(started.elapsed() < DECODE_RPC_LAG_POLL, "slept despite a spent budget");
    }

    /// End-to-end smoke test of the live two-state monitor against a real solver.
    ///
    /// `#[ignore]`d so it never runs in CI (no Tycho/RPC). Run with:
    /// `TYCHO_URL=<ws> RPC_URL=<https> cargo test -p hindsight --bin hindsight \
    ///   resolve::monitor -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires live TYCHO_URL + RPC_URL"]
    async fn test_monitor_one_block_smoke() {
        let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL");
        let tycho_url = std::env::var("TYCHO_URL").expect("set TYCHO_URL");
        let api_key = std::env::var("TYCHO_API_KEY").ok();
        run(MonitorArgs {
            solver: SolverArgs {
                chain: crate::ChainArgs { name: "ethereum".to_string(), rpc_url, registry: None },
                tycho_url,
                protocols: vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
                // High TVL floor → fewer pools → faster load for a smoke test.
                min_tvl: 10_000.0,
                tycho_api_key: api_key,
                worker_pools_config: std::path::PathBuf::from("worker_pools.toml"),
                timeout_ms: fynd_rpc::config::defaults::WORKER_ROUTER_TIMEOUT_MS,
                metrics_port: None,
                max_blocks: Some(1),
                max_lag_blocks: Some(100),
            },
            comparisons_dir: None,
        })
        .await
        .expect("monitor should process one block without error");
    }
}
