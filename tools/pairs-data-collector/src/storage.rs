//! Durable JSON-lines WAL and atomic Parquet compaction.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use arrow_array::{
    ArrayRef, Int32Array, RecordBatch, StringArray, UInt32Array, UInt64Array, UInt8Array,
};
use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};
use sha2::{Digest, Sha256};

use crate::record::{BlockRun, BlockStatusEvent, QuotePoint, RunManifest, WalRecord};

/// Synchronously durable append-only WAL writer.
pub struct WalWriter {
    path: PathBuf,
    writer: BufWriter<File>,
}

/// Hour-partitioned durable sink that compacts closed segments immediately.
pub struct HourlySink {
    output_dir: PathBuf,
    run_id: String,
    manifest: RunManifest,
    current_hour: Option<u64>,
    writer: Option<WalWriter>,
}

impl HourlySink {
    /// Create an unopened hourly sink.
    pub fn new(output_dir: PathBuf, run_id: String, manifest: RunManifest) -> Self {
        Self { output_dir, run_id, manifest, current_hour: None, writer: None }
    }

    /// Append records to the hour containing `block_timestamp`.
    ///
    /// The segment hour never moves backward: a same-height reorg can replace a
    /// block with an earlier timestamp just after an hour boundary, and rotating
    /// back would reopen an already-validated, already-compacted segment. Such
    /// stragglers are appended to the current segment instead.
    pub fn append(&mut self, block_timestamp: u64, records: &[WalRecord]) -> Result<()> {
        let hour = block_timestamp / 3_600;
        if self
            .current_hour
            .is_none_or(|current| hour > current)
        {
            self.rotate(hour)?;
        }
        self.writer
            .as_mut()
            .context("hourly WAL writer was not opened")?
            .append(records)
    }

    /// Flush and compact the active hour.
    pub fn finish(mut self) -> Result<Option<CompactionReport>> {
        self.close_current()
    }

    /// Current WAL path, if the first head has been received.
    pub fn wal_path(&self) -> Option<&Path> {
        self.writer
            .as_ref()
            .map(WalWriter::path)
    }

    fn rotate(&mut self, hour: u64) -> Result<()> {
        self.close_current()?;
        let path = self
            .output_dir
            .join("wal")
            .join(format!("{}-{hour}.ndjson", self.run_id));
        let mut writer = WalWriter::open(&path)?;
        writer.append(&[WalRecord::RunManifest(Box::new(self.manifest.clone()))])?;
        self.current_hour = Some(hour);
        self.writer = Some(writer);
        Ok(())
    }

    fn close_current(&mut self) -> Result<Option<CompactionReport>> {
        let Some(writer) = self.writer.take() else { return Ok(None) };
        let path = writer.path().to_path_buf();
        drop(writer);
        crate::validate::validate_wal(&path)?;
        let report = compact_wal(&path, &self.output_dir.join("parquet"))?;
        Ok(Some(report))
    }
}

impl WalWriter {
    /// Open a WAL file for append, creating parent directories as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating WAL directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening WAL {}", path.display()))?;
        Ok(Self { path: path.to_path_buf(), writer: BufWriter::new(file) })
    }

    /// Append a group of records and synchronize it to stable storage.
    pub fn append(&mut self, records: &[WalRecord]) -> Result<()> {
        for record in records {
            serde_json::to_writer(&mut self.writer, record)
                .with_context(|| format!("serializing record to {}", self.path.display()))?;
            self.writer.write_all(b"\n")?;
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        Ok(())
    }

    /// WAL path used by this writer.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Load a WAL, tolerating only a final torn JSON line.
pub fn read_wal(path: &Path) -> Result<Vec<WalRecord>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading WAL {}", path.display()))?;
    let has_torn_tail = !raw.is_empty() && !raw.ends_with('\n');
    let lines: Vec<&str> = raw.lines().collect();
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if has_torn_tail && index + 1 == lines.len() => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("parsing WAL {} line {}", path.display(), index + 1));
            }
        }
    }
    Ok(records)
}

/// Files written by one compaction operation.
#[derive(Debug, Default)]
pub struct CompactionReport {
    /// Quote point rows.
    pub quote_points: usize,
    /// Block run rows.
    pub block_runs: usize,
    /// Canonicality event rows.
    pub block_status_events: usize,
    /// Output Parquet and manifest files.
    pub files: Vec<PathBuf>,
}

/// Compact one WAL into separate immutable Parquet datasets and JSON manifests.
pub fn compact_wal(wal_path: &Path, output_dir: &Path) -> Result<CompactionReport> {
    let records = read_wal(wal_path)?;
    let mut quotes = Vec::new();
    let mut blocks = Vec::new();
    let mut statuses = Vec::new();
    let mut manifests = Vec::new();
    for record in records {
        match record {
            WalRecord::QuotePoint(row) => quotes.push(*row),
            WalRecord::BlockRun(row) => blocks.push(*row),
            WalRecord::BlockStatusEvent(row) => statuses.push(*row),
            WalRecord::RunManifest(row) => manifests.push(*row),
        }
    }

    let stem = wal_path
        .file_stem()
        .and_then(|value| value.to_str())
        .context("WAL path has no UTF-8 file stem")?;
    let mut report = CompactionReport {
        quote_points: quotes.len(),
        block_runs: blocks.len(),
        block_status_events: statuses.len(),
        files: Vec::new(),
    };
    write_dataset(output_dir, "quote_points", stem, quote_batch(&quotes)?, &mut report.files)?;
    write_dataset(output_dir, "block_runs", stem, block_batch(&blocks)?, &mut report.files)?;
    write_dataset(
        output_dir,
        "block_status_events",
        stem,
        status_batch(&statuses)?,
        &mut report.files,
    )?;
    write_manifests(output_dir, stem, &manifests, &mut report.files)?;
    Ok(report)
}

fn write_dataset(
    output_dir: &Path,
    dataset: &str,
    stem: &str,
    batch: Option<RecordBatch>,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let Some(batch) = batch else { return Ok(()) };
    let directory = output_dir.join(dataset);
    std::fs::create_dir_all(&directory)?;
    let final_path = directory.join(format!("part-{stem}.parquet"));
    let temporary_path = directory.join(format!(".part-{stem}.parquet.tmp"));
    if final_path.exists() {
        anyhow::bail!("refusing to overwrite finalized dataset {}", final_path.display());
    }
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();
    let file = File::create(&temporary_path)?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(properties))?;
    writer.write(&batch)?;
    writer.close()?;
    std::fs::rename(&temporary_path, &final_path)?;
    write_checksum(&final_path)?;
    files.push(final_path);
    Ok(())
}

fn write_manifests(
    output_dir: &Path,
    stem: &str,
    manifests: &[RunManifest],
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    if manifests.is_empty() {
        return Ok(());
    }
    let directory = output_dir.join("manifests");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{stem}.json"));
    let temporary = directory.join(format!(".{stem}.json.tmp"));
    std::fs::write(&temporary, serde_json::to_vec_pretty(manifests)?)?;
    std::fs::rename(temporary, &path)?;
    write_checksum(&path)?;
    files.push(path);
    Ok(())
}

fn write_checksum(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let checksum = hex::encode(Sha256::digest(bytes));
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("checksummed path has no UTF-8 filename")?;
    std::fs::write(
        path.with_extension(format!(
            "{}.sha256",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("data")
        )),
        format!("{checksum}  {filename}\n"),
    )?;
    Ok(())
}

fn quote_batch(rows: &[QuotePoint]) -> Result<Option<RecordBatch>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let mut columns = Vec::new();
    append_quote_identity(&mut columns, rows);
    append_quote_block(&mut columns, rows);
    append_quote_amounts(&mut columns, rows);
    append_quote_result(&mut columns, rows)?;
    Ok(Some(RecordBatch::try_from_iter(columns)?))
}

fn append_quote_identity(columns: &mut Vec<(&'static str, ArrayRef)>, rows: &[QuotePoint]) {
    columns.extend([
        ("schema_version", u32s(rows, |row| row.schema_version)),
        ("point_id", strings(rows, |row| row.point_id.clone())),
        ("run_id", strings(rows, |row| row.run_id.clone())),
        ("grid_epoch_id", strings(rows, |row| row.grid_epoch_id.clone())),
        ("pair_id", strings(rows, |row| row.pair_id.clone())),
        ("direction", json_names(rows, |row| &row.direction)),
        ("depth_index", u64s(rows, |row| row.depth_index as u64)),
        ("quote_role", json_names(rows, |row| &row.quote_role)),
        ("attempt_id", u32s(rows, |row| row.attempt_id)),
        ("parent_point_id", optional_strings(rows, |row| row.parent_point_id.clone())),
    ]);
}

fn append_quote_block(columns: &mut Vec<(&'static str, ArrayRef)>, rows: &[QuotePoint]) {
    columns.extend([
        ("chain_id", u64s(rows, |row| row.chain_id)),
        ("block_number", u64s(rows, |row| row.block_number)),
        ("block_hash", strings(rows, |row| row.block_hash.clone())),
        ("block_timestamp", u64s(rows, |row| row.block_timestamp)),
        ("head_received_at_ms", u64s(rows, |row| row.head_received_at_ms)),
        ("quote_started_at_ms", u64s(rows, |row| row.quote_started_at_ms)),
        ("quote_finished_at_ms", u64s(rows, |row| row.quote_finished_at_ms)),
        ("batch_solve_time_ms", optional_u64s(rows, |row| row.batch_solve_time_ms)),
        ("monotonic_duration_ms", u64s(rows, |row| row.monotonic_duration_ms)),
        ("token_in", strings(rows, |row| row.token_in.address.clone())),
        ("token_in_symbol", strings(rows, |row| row.token_in.symbol.clone())),
        ("token_in_decimals", u8s(rows, |row| row.token_in.decimals)),
        ("token_out", strings(rows, |row| row.token_out.address.clone())),
        ("token_out_symbol", strings(rows, |row| row.token_out.symbol.clone())),
        ("token_out_decimals", u8s(rows, |row| row.token_out.decimals)),
    ]);
}

fn append_quote_amounts(columns: &mut Vec<(&'static str, ArrayRef)>, rows: &[QuotePoint]) {
    columns.extend([
        ("amount_in", strings(rows, |row| row.amount_in.clone())),
        ("amount_out", optional_strings(rows, |row| row.amount_out.clone())),
        ("amount_out_net_gas", optional_strings(rows, |row| row.amount_out_net_gas.clone())),
        ("gas_estimate", optional_strings(rows, |row| row.gas_estimate.clone())),
        ("gas_price", optional_strings(rows, |row| row.gas_price.clone())),
        ("price_impact_bps", optional_i32s(rows, |row| row.price_impact_bps)),
        ("forward_gross_output", optional_strings(rows, |row| row.forward_gross_output.clone())),
    ]);
}

fn append_quote_result(
    columns: &mut Vec<(&'static str, ArrayRef)>,
    rows: &[QuotePoint],
) -> Result<()> {
    columns.extend([
        ("status", json_names(rows, |row| &row.status)),
        ("failure_reason", optional_strings(rows, |row| row.failure_reason.clone())),
        ("route_json", optional_strings(rows, |row| row.route_json.clone())),
        ("fynd_version", strings(rows, |row| row.fynd_version.clone())),
        ("fynd_git_sha", strings(rows, |row| row.fynd_git_sha.clone())),
        ("config_hash", strings(rows, |row| row.config_hash.clone())),
        ("protocol_set_hash", strings(rows, |row| row.protocol_set_hash.clone())),
        ("worker_config_hash", strings(rows, |row| row.worker_config_hash.clone())),
    ]);
    Ok(())
}

fn block_batch(rows: &[BlockRun]) -> Result<Option<RecordBatch>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let columns = vec![
        ("schema_version", u32s(rows, |row| row.schema_version)),
        ("run_id", strings(rows, |row| row.run_id.clone())),
        ("chain_id", u64s(rows, |row| row.chain_id)),
        ("block_number", u64s(rows, |row| row.block_number)),
        ("block_hash", strings(rows, |row| row.block_hash.clone())),
        ("parent_hash", strings(rows, |row| row.parent_hash.clone())),
        ("block_timestamp", u64s(rows, |row| row.block_timestamp)),
        ("base_fee_per_gas", optional_u64s(rows, |row| row.base_fee_per_gas)),
        ("rpc_endpoint_id", strings(rows, |row| row.rpc_endpoint_id.clone())),
        ("head_received_at_ms", u64s(rows, |row| row.head_received_at_ms)),
        ("fynd_ready_at_ms", optional_u64s(rows, |row| row.fynd_ready_at_ms)),
        ("expected_rows", u64s(rows, |row| row.expected_rows as u64)),
        ("scheduled_rows", u64s(rows, |row| row.scheduled_rows as u64)),
        ("successful_rows", u64s(rows, |row| row.successful_rows as u64)),
        ("failed_rows", u64s(rows, |row| row.failed_rows as u64)),
        ("market_negative_rows", u64s(rows, |row| row.market_negative_rows as u64)),
        ("status", strings(rows, |row| row.status.clone())),
        ("config_hash", strings(rows, |row| row.config_hash.clone())),
    ];
    Ok(Some(RecordBatch::try_from_iter(columns)?))
}

fn status_batch(rows: &[BlockStatusEvent]) -> Result<Option<RecordBatch>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let columns = vec![
        ("schema_version", u32s(rows, |row| row.schema_version)),
        ("block_number", u64s(rows, |row| row.block_number)),
        ("block_hash", strings(rows, |row| row.block_hash.clone())),
        ("status", json_names(rows, |row| &row.status)),
        ("status_changed_at_ms", u64s(rows, |row| row.status_changed_at_ms)),
        ("canonical_head", optional_u64s(rows, |row| row.canonical_head)),
    ];
    Ok(Some(RecordBatch::try_from_iter(columns)?))
}

fn strings<T>(rows: &[T], value: impl Fn(&T) -> String) -> ArrayRef {
    Arc::new(StringArray::from_iter_values(rows.iter().map(value)))
}

fn optional_strings<T>(rows: &[T], value: impl Fn(&T) -> Option<String>) -> ArrayRef {
    Arc::new(StringArray::from(
        rows.iter()
            .map(value)
            .collect::<Vec<_>>(),
    ))
}

fn json_names<T, V: serde::Serialize>(rows: &[T], value: impl Fn(&T) -> &V) -> ArrayRef {
    strings(rows, |row| {
        serde_json::to_string(value(row))
            .expect("enum serializes")
            .trim_matches('"')
            .to_string()
    })
}

fn u64s<T>(rows: &[T], value: impl Fn(&T) -> u64) -> ArrayRef {
    Arc::new(UInt64Array::from_iter_values(rows.iter().map(value)))
}

fn optional_u64s<T>(rows: &[T], value: impl Fn(&T) -> Option<u64>) -> ArrayRef {
    Arc::new(UInt64Array::from(
        rows.iter()
            .map(value)
            .collect::<Vec<_>>(),
    ))
}

fn u32s<T>(rows: &[T], value: impl Fn(&T) -> u32) -> ArrayRef {
    Arc::new(UInt32Array::from_iter_values(rows.iter().map(value)))
}

fn u8s<T>(rows: &[T], value: impl Fn(&T) -> u8) -> ArrayRef {
    Arc::new(UInt8Array::from_iter_values(rows.iter().map(value)))
}

fn optional_i32s<T>(rows: &[T], value: impl Fn(&T) -> Option<i32>) -> ArrayRef {
    Arc::new(Int32Array::from(
        rows.iter()
            .map(value)
            .collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;
    use crate::record::{BlockStatusEvent, CanonicalStatus, SCHEMA_VERSION};

    #[test]
    fn wal_recovers_complete_records_and_ignores_torn_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run.ndjson");
        let event = WalRecord::BlockStatusEvent(Box::new(BlockStatusEvent {
            schema_version: SCHEMA_VERSION,
            block_number: 10,
            block_hash: "0xabc".into(),
            status: CanonicalStatus::Observed,
            status_changed_at_ms: 1,
            canonical_head: None,
        }));
        let mut writer = WalWriter::open(&path).unwrap();
        writer
            .append(std::slice::from_ref(&event))
            .unwrap();
        drop(writer);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"record_type\":")
            .unwrap();

        let recovered = read_wal(&path).unwrap();

        assert_eq!(recovered.len(), 1);
    }

    #[test]
    fn compaction_writes_readable_parquet_and_checksum() {
        let directory = tempfile::tempdir().unwrap();
        let wal = directory.path().join("run.ndjson");
        let output = directory.path().join("parquet");
        let record = WalRecord::BlockStatusEvent(Box::new(BlockStatusEvent {
            schema_version: SCHEMA_VERSION,
            block_number: 10,
            block_hash: "0xabc".into(),
            status: CanonicalStatus::Observed,
            status_changed_at_ms: 1,
            canonical_head: None,
        }));
        WalWriter::open(&wal)
            .unwrap()
            .append(&[record])
            .unwrap();

        let report = compact_wal(&wal, &output).unwrap();
        let path = &report.files[0];
        let file = File::open(path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        let rows: usize = reader
            .map(|batch| batch.unwrap().num_rows())
            .sum();

        assert_eq!(rows, 1);
        let checksum_path = path.with_extension("parquet.sha256");
        let checksum = std::fs::read_to_string(checksum_path).unwrap();
        assert!(checksum.ends_with(&format!(
            "  {}\n",
            path.file_name()
                .unwrap()
                .to_string_lossy()
        )));
    }

    fn manifest() -> RunManifest {
        RunManifest {
            schema_version: SCHEMA_VERSION,
            run_id: "run".into(),
            run_name: "test".into(),
            grid_epoch_id: "grid".into(),
            started_at_ms: 1,
            resolved_config_toml: "run_name = 'test'".into(),
            config_hash: "hash".into(),
        }
    }

    fn status_event(number: u64) -> WalRecord {
        WalRecord::BlockStatusEvent(Box::new(BlockStatusEvent {
            schema_version: SCHEMA_VERSION,
            block_number: number,
            block_hash: format!("0x{number:x}"),
            status: CanonicalStatus::Observed,
            status_changed_at_ms: number,
            canonical_head: None,
        }))
    }

    #[test]
    fn hourly_sink_rotates_without_overwriting_closed_segments() {
        let directory = tempfile::tempdir().unwrap();
        let mut sink = HourlySink::new(directory.path().to_path_buf(), "run".into(), manifest());
        sink.append(3_599, &[status_event(1)])
            .unwrap();
        sink.append(3_600, &[status_event(2)])
            .unwrap();
        sink.finish().unwrap();

        let status_dir = directory
            .path()
            .join("parquet/block_status_events");
        let files = std::fs::read_dir(status_dir)
            .unwrap()
            .count();

        assert_eq!(files, 4, "two parquet files and two checksums expected");
    }

    #[test]
    fn hourly_sink_never_rotates_back_into_a_closed_hour() {
        let directory = tempfile::tempdir().unwrap();
        let mut sink = HourlySink::new(directory.path().to_path_buf(), "run".into(), manifest());
        sink.append(3_599, &[status_event(1)])
            .unwrap();
        sink.append(3_600, &[status_event(2)])
            .unwrap();
        // Same-height reorg replacement with an earlier timestamp must not reopen
        // the finalized hour-0 segment.
        sink.append(3_595, &[status_event(3)])
            .unwrap();
        sink.finish().unwrap();

        let wal_dir = directory.path().join("wal");
        let wal_files = std::fs::read_dir(wal_dir)
            .unwrap()
            .count();
        assert_eq!(wal_files, 2, "the straggler must stay in the current segment");
        let current = std::fs::read_to_string(
            directory
                .path()
                .join("wal/run-1.ndjson"),
        )
        .unwrap();
        assert!(current.contains("\"block_number\":3"));
    }
}
