//! CLI runtime orchestration for live collection.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use fynd_core::{FyndBuilder, PoolConfig};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tycho_simulation::evm::tycho_models::Chain;
use uuid::Uuid;

use crate::{
    collector::{Collector, HeadCollection, RuntimeMetadata, StateWaitOutcome},
    config::CollectorConfig,
    head_ledger::{now_ms, run_head_subscription, HeadLedger, ObservedHead, RpcHeaderClient},
    record::{PointStatus, RunManifest, WalRecord, SCHEMA_VERSION},
    storage::HourlySink,
};

/// Live collection arguments supplied by the CLI.
pub struct CollectArgs {
    /// Validated TOML config path.
    pub config_path: PathBuf,
    /// Root output directory.
    pub output_dir: PathBuf,
    /// Optional bounded head count for probes and smoke tests.
    pub max_heads: Option<u64>,
}

/// Run live collection until Ctrl-C or the optional head limit.
pub async fn run_collect(args: CollectArgs) -> Result<()> {
    let config = CollectorConfig::load(&args.config_path)?;
    let environment = RuntimeEnvironment::resolve(&config)?;
    let run_id = Uuid::new_v4().to_string();
    let grid_epoch_id = format!("{run_id}-grid-0");
    let metadata = RuntimeMetadata::from_config(&config, run_id.clone(), grid_epoch_id.clone())?;
    let manifest = build_manifest(&config, &metadata)?;
    let mut ledger = restore_ledger(&args.output_dir, config.collection.confirmation_depth)?;
    let mut sink = HourlySink::new(args.output_dir, run_id, manifest);
    let solver = build_solver(&config, &environment)?;
    solver
        .wait_until_ready(Duration::from_secs(180))
        .await
        .context("waiting for embedded Fynd solver readiness")?;
    let collector = Collector::new(solver, config.clone(), metadata)?;

    let (head_tx, mut head_rx) = mpsc::channel(64);
    let head_task = tokio::spawn(run_head_subscription(
        environment.rpc_ws_url,
        config.fynd.rpc_ws_url_env.clone(),
        head_tx,
    ));
    let header_client =
        RpcHeaderClient::new(environment.rpc_http_url, config.fynd.rpc_http_url_env.clone());
    let mut processed = 0u64;
    loop {
        let next_head = tokio::select! {
            head = head_rx.recv() => head,
            signal = tokio::signal::ctrl_c() => {
                signal.context("installing Ctrl-C handler")?;
                None
            }
        };
        let Some(head) = next_head else { break };
        let update = ledger.observe(&head);
        if !update.is_new {
            continue;
        }
        reconcile_gaps(
            &collector,
            &header_client,
            &mut ledger,
            &mut sink,
            &head,
            &update.missing_numbers,
        )
        .await?;
        let mut head_records: Vec<WalRecord> = update
            .status_events
            .into_iter()
            .map(|event| WalRecord::BlockStatusEvent(Box::new(event)))
            .collect();
        let collection = collect_or_mark_missed(&collector, &head).await;
        log_collection_metrics(&collection);
        append_collection(&mut head_records, collection);
        sink.append(head.timestamp, &head_records)?;
        processed += 1;
        info!(block = head.number, processed, "persisted executable curve observations");
        if args
            .max_heads
            .is_some_and(|limit| processed >= limit)
        {
            break;
        }
    }
    head_task.abort();
    collector.shutdown();
    if let Some(report) = sink.finish()? {
        info!(
            quote_points = report.quote_points,
            block_runs = report.block_runs,
            status_events = report.block_status_events,
            "finalized collector segment"
        );
    }
    Ok(())
}

struct RuntimeEnvironment {
    tycho_api_key: String,
    rpc_http_url: String,
    rpc_ws_url: String,
}

impl RuntimeEnvironment {
    fn resolve(config: &CollectorConfig) -> Result<Self> {
        Ok(Self {
            tycho_api_key: required_env(&config.fynd.tycho_api_key_env)?,
            rpc_http_url: required_env(&config.fynd.rpc_http_url_env)?,
            rpc_ws_url: required_env(&config.fynd.rpc_ws_url_env)?,
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("required environment variable {name} is not set"))
}

fn build_solver(
    config: &CollectorConfig,
    environment: &RuntimeEnvironment,
) -> Result<fynd_core::Solver> {
    let pool = PoolConfig::new(&config.fynd.algorithm)
        .with_num_workers(config.fynd.num_workers)
        .with_task_queue_capacity(config.fynd.task_queue_capacity)
        .with_max_hops(config.fynd.max_hops)
        .with_timeout_ms(config.fynd.algorithm_timeout_ms);
    let builder = FyndBuilder::new(
        Chain::Ethereum,
        &config.fynd.tycho_url,
        &environment.rpc_http_url,
        config.fynd.protocols.clone(),
        config.fynd.min_tvl,
    )
    .tycho_api_key(&environment.tycho_api_key)
    .worker_router_min_responses(0)
    .worker_router_timeout(Duration::from_millis(config.collection.quote_timeout_ms));
    builder
        .add_pool("pairs-data-collector", &pool)?
        .build()
        .context("building embedded Fynd solver")
}

fn build_manifest(config: &CollectorConfig, metadata: &RuntimeMetadata) -> Result<RunManifest> {
    Ok(RunManifest {
        schema_version: SCHEMA_VERSION,
        run_id: metadata.run_id.clone(),
        run_name: config.run_name.clone(),
        grid_epoch_id: metadata.grid_epoch_id.clone(),
        started_at_ms: now_ms(),
        resolved_config_toml: toml::to_string_pretty(config)?,
        config_hash: metadata.config_hash.clone(),
    })
}

async fn reconcile_gaps(
    collector: &Collector,
    header_client: &RpcHeaderClient,
    ledger: &mut HeadLedger,
    sink: &mut HourlySink,
    current_head: &ObservedHead,
    missing_numbers: &[u64],
) -> Result<()> {
    for number in missing_numbers {
        let (missing_head, status_events) = match header_client.header(*number).await {
            Ok(head) => {
                let update = ledger.observe(&head);
                (head, update.status_events)
            }
            Err(error) => {
                warn!(block = number, %error, "failed to enrich missed block header");
                (synthetic_missing_head(*number, current_head), Vec::new())
            }
        };
        let mut records: Vec<WalRecord> = status_events
            .into_iter()
            .map(|event| WalRecord::BlockStatusEvent(Box::new(event)))
            .collect();
        append_collection(
            &mut records,
            collector.missed_head(&missing_head, "live Fynd state was skipped or unavailable"),
        );
        sink.append(missing_head.timestamp, &records)?;
    }
    Ok(())
}

async fn collect_or_mark_missed(collector: &Collector, head: &ObservedHead) -> HeadCollection {
    match collector.wait_for_head(head).await {
        StateWaitOutcome::Ready { ready_at_ms } => {
            collector
                .collect_head(head, ready_at_ms)
                .await
        }
        StateWaitOutcome::Missed { reason } | StateWaitOutcome::Timeout { reason } => {
            collector.missed_head(head, &reason)
        }
    }
}

fn append_collection(records: &mut Vec<WalRecord>, collection: HeadCollection) {
    records.extend(
        collection
            .points
            .into_iter()
            .map(|point| WalRecord::QuotePoint(Box::new(point))),
    );
    records.push(WalRecord::BlockRun(Box::new(collection.block_run)));
}

fn log_collection_metrics(collection: &HeadCollection) {
    let run = &collection.block_run;
    info!(
        block = run.block_number,
        expected_rows = run.expected_rows,
        scheduled_rows = run.scheduled_rows,
        successful_rows = run.successful_rows,
        failed_rows = run.failed_rows,
        market_negative_rows = run.market_negative_rows,
        collection_ms = run
            .collection_finished_at_ms
            .saturating_sub(run.collection_started_at_ms),
        head_lag_ms = run
            .collection_finished_at_ms
            .saturating_sub(run.head_received_at_ms),
        status = run.status,
        "pairs collector block metrics"
    );
    let capacity_skipped = collection
        .points
        .iter()
        .filter(|point| point.status == PointStatus::CapacitySkipped)
        .count();
    if capacity_skipped > 0 {
        error!(
            block = run.block_number,
            capacity_skipped,
            "configured universe does not fit the per-head budget; shrink the universe or scale \
             the machine"
        );
    }
    let infra_failed = run
        .failed_rows
        .saturating_sub(run.market_negative_rows)
        .saturating_sub(capacity_skipped);
    if infra_failed > 0 {
        warn!(
            block = run.block_number,
            infra_failed, "collection failures unrelated to market state or capacity"
        );
    }
}

fn synthetic_missing_head(number: u64, current: &ObservedHead) -> ObservedHead {
    ObservedHead {
        number,
        hash: format!("unknown-{number}"),
        parent_hash: "unknown".into(),
        timestamp: current
            .timestamp
            .saturating_sub((current.number - number) * 12),
        base_fee_per_gas: None,
        received_at_ms: now_ms(),
        rpc_endpoint_id: current.rpc_endpoint_id.clone(),
    }
}

fn restore_ledger(output_dir: &std::path::Path, confirmation_depth: u64) -> Result<HeadLedger> {
    let directory = output_dir.join("wal");
    if !directory.exists() {
        return Ok(HeadLedger::new(confirmation_depth));
    }
    let mut events = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .with_context(|| format!("reading prior WAL directory {}", directory.display()))?
    {
        let path = entry?.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str()) !=
            Some("ndjson")
        {
            continue;
        }
        for record in crate::storage::read_wal(&path)? {
            if let WalRecord::BlockStatusEvent(event) = record {
                events.push(*event);
            }
        }
    }
    Ok(HeadLedger::restore(confirmation_depth, events))
}
