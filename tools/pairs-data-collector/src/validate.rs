//! Structural validation for durable collector WAL files.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    str::FromStr,
};

use anyhow::{bail, Context, Result};
use num_bigint::BigUint;

use crate::{
    record::{BlockRun, PointStatus, QuotePoint, QuoteRole, WalRecord},
    storage::read_wal,
};

/// Structural validation summary.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Unique quote points.
    pub quote_points: usize,
    /// Complete block records.
    pub block_runs: usize,
    /// Canonicality events.
    pub block_status_events: usize,
    /// Run manifests.
    pub manifests: usize,
}

/// Validate uniqueness, parent references, and block cardinality in a WAL.
pub fn validate_wal(path: &Path) -> Result<ValidationReport> {
    let records = read_wal(path)?;
    let mut point_ids = HashSet::new();
    let mut parent_ids = Vec::new();
    let mut quote_counts: HashMap<(String, String), usize> = HashMap::new();
    let mut blocks = Vec::new();
    let mut report = ValidationReport::default();
    for record in records {
        match record {
            WalRecord::QuotePoint(point) => {
                validate_point(&point)?;
                if !point_ids.insert(point.point_id.clone()) {
                    bail!("duplicate quote point id: {}", point.point_id);
                }
                if let Some(parent) = point.parent_point_id.clone() {
                    parent_ids.push(parent);
                }
                *quote_counts
                    .entry((point.run_id.clone(), point.block_hash.clone()))
                    .or_default() += 1;
                report.quote_points += 1;
            }
            WalRecord::BlockRun(block) => {
                validate_block(&block)?;
                blocks.push(*block);
                report.block_runs += 1;
            }
            WalRecord::BlockStatusEvent(_) => report.block_status_events += 1,
            WalRecord::RunManifest(_) => report.manifests += 1,
        }
    }
    for parent in parent_ids {
        if !point_ids.contains(&parent) {
            bail!("matched reverse references missing parent point: {parent}");
        }
    }
    for block in blocks {
        let actual = quote_counts
            .get(&(block.run_id.clone(), block.block_hash.clone()))
            .copied()
            .unwrap_or_default();
        if actual != block.scheduled_rows {
            bail!(
                "block {} declares {} scheduled rows but WAL contains {actual}",
                block.block_number,
                block.scheduled_rows
            );
        }
    }
    if report.manifests != 1 {
        bail!("WAL {} must contain exactly one run manifest", path.display());
    }
    Ok(report)
}

fn validate_point(point: &QuotePoint) -> Result<()> {
    BigUint::from_str(&point.amount_in)
        .with_context(|| format!("point {} has invalid amount_in", point.point_id))?;
    match point.quote_role {
        QuoteRole::LadderForward if point.parent_point_id.is_some() => {
            bail!("source point {} unexpectedly has a parent", point.point_id)
        }
        QuoteRole::MatchedReverse if point.parent_point_id.is_none() => {
            bail!("reverse point {} has no parent", point.point_id)
        }
        QuoteRole::LadderForward | QuoteRole::MatchedReverse => {}
    }
    if point.status == PointStatus::Success {
        let Some(route_json) = point.route_json.as_deref() else {
            bail!("successful point {} lacks amount_out or route", point.point_id);
        };
        if point.amount_out.is_none() {
            bail!("successful point {} lacks amount_out or route", point.point_id);
        }
        validate_route_json(&point.point_id, route_json)?;
    }
    Ok(())
}

fn validate_route_json(point_id: &str, route_json: &str) -> Result<()> {
    let route: serde_json::Value = serde_json::from_str(route_json)
        .with_context(|| format!("point {point_id} has unreadable route JSON"))?;
    let has_swaps = route
        .get("swaps")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|swaps| !swaps.is_empty());
    if !has_swaps {
        bail!("point {point_id} route JSON has no swaps");
    }
    Ok(())
}

fn validate_block(block: &BlockRun) -> Result<()> {
    if block.scheduled_rows != block.successful_rows + block.failed_rows {
        bail!(
            "block {} scheduled rows do not equal successful plus failed rows",
            block.block_number
        );
    }
    if block.scheduled_rows > block.expected_rows {
        bail!("block {} scheduled more rows than configured", block.block_number);
    }
    if block.market_negative_rows > block.failed_rows {
        bail!(
            "block {} declares more market-negative rows than failed rows",
            block.block_number
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        record::{RunManifest, SCHEMA_VERSION},
        storage::WalWriter,
    };

    fn manifest() -> WalRecord {
        WalRecord::RunManifest(Box::new(RunManifest {
            schema_version: SCHEMA_VERSION,
            run_id: "run".into(),
            run_name: "test".into(),
            grid_epoch_id: "grid".into(),
            started_at_ms: 1,
            resolved_config_toml: "run_name = 'test'".into(),
            config_hash: "hash".into(),
        }))
    }

    #[test]
    fn rejects_block_run_whose_declared_rows_are_missing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.ndjson");
        let block = WalRecord::BlockRun(Box::new(BlockRun {
            schema_version: SCHEMA_VERSION,
            run_id: "run".into(),
            chain_id: 1,
            block_number: 10,
            block_hash: "0xabc".into(),
            parent_hash: "0xparent".into(),
            block_timestamp: 1,
            base_fee_per_gas: Some(1),
            rpc_endpoint_id: "test".into(),
            head_received_at_ms: 1,
            fynd_ready_at_ms: None,
            collection_started_at_ms: 1,
            collection_finished_at_ms: 2,
            expected_rows: 1,
            scheduled_rows: 1,
            successful_rows: 0,
            failed_rows: 1,
            market_negative_rows: 0,
            status: "partial".into(),
            config_hash: "hash".into(),
        }));
        WalWriter::open(&path)
            .unwrap()
            .append(&[manifest(), block])
            .unwrap();

        let error = validate_wal(&path)
            .unwrap_err()
            .to_string();

        assert!(error.contains("WAL contains 0"));
    }

    #[test]
    fn rejects_duplicate_manifests() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.ndjson");
        WalWriter::open(&path)
            .unwrap()
            .append(&[manifest(), manifest()])
            .unwrap();

        let error = validate_wal(&path)
            .unwrap_err()
            .to_string();

        assert!(error.contains("exactly one run manifest"));
    }

    #[test]
    fn rejects_unreadable_or_empty_route_json() {
        assert!(validate_route_json("point", "not-json").is_err());
        assert!(validate_route_json("point", r#"{"swaps":[]}"#).is_err());
        validate_route_json("point", r#"{"swaps":[{"protocol":"test"}]}"#).unwrap();
    }
}
